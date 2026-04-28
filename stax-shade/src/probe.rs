use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_port::mach_port_deallocate;
use mach2::mach_time::mach_absolute_time;
use mach2::mach_types::{thread_act_array_t, thread_act_t};
use mach2::message::mach_msg_type_number_t;
use mach2::port::mach_port_t;
use mach2::task::task_threads;
use mach2::thread_act::{thread_get_state, thread_resume, thread_suspend};
use mach2::thread_status::thread_state_t;
use mach2::traps::mach_task_self;
use mach2::vm::mach_vm_deallocate;
use mach2::vm::mach_vm_read_overwrite;
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t, natural_t};
use stax_mac_capture::sample_sink::CpuIntervalEvent;
use stax_mac_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, MachOByteSource, ProbeQueueStats,
    ProbeResultEvent, ProbeTiming, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};

const MAX_FP_FRAMES: usize = 64;
const MAX_FP_DELTA: u64 = 8 * 1024 * 1024;
pub struct RaceKperfSink<S> {
    inner: S,
    probe: Option<RaceProbeWorker>,
}

impl<S> RaceKperfSink<S> {
    pub fn disabled(inner: S) -> Self {
        Self { inner, probe: None }
    }

    pub fn enabled(task: mach_port_t, inner: S) -> Self {
        Self {
            inner,
            probe: Some(RaceProbeWorker::new(task)),
        }
    }

    pub fn trigger(&self) -> Option<RaceProbeTrigger> {
        self.probe.as_ref().map(RaceProbeWorker::trigger)
    }
}

impl<S: SampleSink> SampleSink for RaceKperfSink<S> {
    fn on_sample(&mut self, sample: SampleEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_sample(sample);
    }

    fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_binary_loaded(ev);
    }

    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_binary_unloaded(ev);
    }

    fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_thread_name(ev);
    }

    fn on_jitdump(&mut self, ev: JitdumpEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_jitdump(ev);
    }

    fn on_kallsyms(&mut self, data: &[u8]) {
        self.drain_probe_results();
        self.inner.on_kallsyms(data);
        self.drain_probe_results();
    }

    fn on_wakeup(&mut self, event: WakeupEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_wakeup(event);
    }

    fn on_cpu_interval(&mut self, event: CpuIntervalEvent<'_>) {
        self.drain_probe_results();
        self.inner.on_cpu_interval(event);
    }

    fn on_probe_result(&mut self, ev: ProbeResultEvent<'_>) {
        self.inner.on_probe_result(ev);
    }

    fn on_macho_byte_source(&mut self, source: std::sync::Arc<dyn MachOByteSource>) {
        self.inner.on_macho_byte_source(source);
    }
}

impl<S: SampleSink> RaceKperfSink<S> {
    fn drain_probe_results(&mut self) {
        let Some(probe) = self.probe.as_mut() else {
            return;
        };
        for result in probe.drain_results() {
            self.inner.on_probe_result(ProbeResultEvent {
                tid: result.tid,
                timing: result.timing,
                queue: result.queue,
                mach_pc: result.pc,
                mach_lr: result.lr,
                mach_fp: result.fp,
                mach_sp: result.sp,
                mach_walked: &result.walked,
                used_framehop: false,
            });
        }
    }
}

struct RaceProbeWorker {
    trigger: RaceProbeTrigger,
    res_rx: Receiver<ProbeSnapshotWithKey>,
}

impl RaceProbeWorker {
    fn new(task: mach_port_t) -> Self {
        let requests = Arc::new(LatestProbeRequests::default());
        let worker_requests = requests.clone();
        let (res_tx, res_rx) = mpsc::channel::<ProbeSnapshotWithKey>();
        std::thread::Builder::new()
            .name("stax-race-probe".to_owned())
            .spawn(move || {
                let mut probe = RaceProbe::new(task);
                while let Some(batch) = worker_requests.take_all() {
                    let worker_batch_len = batch.len() as u32;
                    for req in batch {
                        let timing = ProbeTiming {
                            kperf_ts: req.kperf_ts,
                            enqueued: req.enqueued_ticks,
                            worker_started: unsafe { mach_absolute_time() },
                            ..ProbeTiming::default()
                        };
                        if let Some(snapshot) = probe.probe_sample(req.tid, timing) {
                            let out = ProbeSnapshotWithKey {
                                tid: req.tid,
                                timing: snapshot.timing,
                                queue: ProbeQueueStats {
                                    coalesced_requests: req.coalesced_requests,
                                    worker_batch_len,
                                },
                                pc: snapshot.pc,
                                lr: snapshot.lr,
                                fp: snapshot.fp,
                                sp: snapshot.sp,
                                walked: snapshot.walked,
                            };
                            if res_tx.send(out).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .expect("spawn race probe worker");
        Self {
            trigger: RaceProbeTrigger {
                requests,
                coalesced_probe_requests: Arc::new(AtomicU64::new(0)),
            },
            res_rx,
        }
    }

    fn trigger(&self) -> RaceProbeTrigger {
        self.trigger.clone()
    }

    fn drain_results(&self) -> Vec<ProbeSnapshotWithKey> {
        let mut out = Vec::new();
        while let Ok(snapshot) = self.res_rx.try_recv() {
            out.push(snapshot);
        }
        out
    }
}

impl Drop for RaceProbeWorker {
    fn drop(&mut self) {
        self.trigger.close();
    }
}

#[derive(Clone)]
pub struct RaceProbeTrigger {
    requests: Arc<LatestProbeRequests>,
    coalesced_probe_requests: Arc<AtomicU64>,
}

impl RaceProbeTrigger {
    /// Enqueue immediately when the raw kdebug stream shows a kperf
    /// sample start. Returns true when this replaced an older pending
    /// request for the same tid; that is intentional because
    /// race-kperf wants fresh observations, not FIFO delivery.
    pub fn enqueue(&self, tid: u32, kperf_ts: u64) -> bool {
        let replaced = self.requests.push(ProbeRequest {
            tid,
            kperf_ts,
            enqueued_ticks: unsafe { mach_absolute_time() },
            coalesced_requests: 0,
        });
        if replaced {
            let coalesced = self
                .coalesced_probe_requests
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if coalesced.is_multiple_of(1024) {
                tracing::warn!(
                    coalesced,
                    "race-kperf probe worker lagging; replacing stale pending requests"
                );
            }
        }
        replaced
    }

    fn close(&self) {
        self.requests.close();
    }
}

#[derive(Default)]
struct LatestProbeRequests {
    state: Mutex<LatestProbeState>,
    ready: Condvar,
}

#[derive(Default)]
struct LatestProbeState {
    pending: HashMap<u32, ProbeRequest>,
    closed: bool,
}

impl LatestProbeRequests {
    fn push(&self, request: ProbeRequest) -> bool {
        let mut state = self.state.lock().expect("race probe request lock poisoned");
        let mut request = request;
        let replaced = if let Some(previous) = state.pending.remove(&request.tid) {
            request.coalesced_requests = previous.coalesced_requests.saturating_add(1);
            true
        } else {
            false
        };
        state.pending.insert(request.tid, request);
        self.ready.notify_one();
        replaced
    }

    fn take_all(&self) -> Option<Vec<ProbeRequest>> {
        let mut state = self.state.lock().expect("race probe request lock poisoned");
        while state.pending.is_empty() && !state.closed {
            state = self
                .ready
                .wait(state)
                .expect("race probe request lock poisoned");
        }
        if state.pending.is_empty() && state.closed {
            return None;
        }
        Some(state.pending.drain().map(|(_, request)| request).collect())
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("race probe request lock poisoned");
        state.closed = true;
        self.ready.notify_all();
    }
}

struct RaceProbe {
    task: mach_port_t,
    threads: ThreadPortCache,
}

impl RaceProbe {
    fn new(task: mach_port_t) -> Self {
        Self {
            task,
            threads: ThreadPortCache::new(task),
        }
    }

    fn probe_sample(&mut self, tid: u32, mut timing: ProbeTiming) -> Option<ProbeSnapshot> {
        let thread = self.threads.get(tid)?;
        timing.thread_lookup_done = unsafe { mach_absolute_time() };
        match self.probe_thread(thread, timing) {
            Ok(snapshot) => Some(snapshot),
            Err(ProbeError::Kernel { op, kr }) => {
                tracing::debug!(tid, op, kr, "race-kperf probe failed");
                self.threads.forget(tid);
                None
            }
        }
    }

    fn probe_thread(
        &self,
        thread: thread_act_t,
        mut timing: ProbeTiming,
    ) -> Result<ProbeSnapshot, ProbeError> {
        let kr = unsafe { thread_suspend(thread) };
        if kr != KERN_SUCCESS {
            return Err(ProbeError::Kernel {
                op: "thread_suspend",
                kr,
            });
        }

        let state = match read_thread_state(thread) {
            Ok(state) => state,
            Err(err) => {
                let _ = unsafe { thread_resume(thread) };
                return Err(err);
            }
        };
        timing.state_done = unsafe { mach_absolute_time() };
        let resume_kr = unsafe { thread_resume(thread) };
        timing.resume_done = unsafe { mach_absolute_time() };
        if resume_kr != KERN_SUCCESS {
            return Err(ProbeError::Kernel {
                op: "thread_resume",
                kr: resume_kr,
            });
        }

        let walked = fp_walk(self.task, state.fp);
        timing.walk_done = unsafe { mach_absolute_time() };
        Ok(ProbeSnapshot {
            timing,
            pc: strip_ptr(state.pc),
            lr: strip_ptr(state.lr),
            fp: strip_ptr(state.fp),
            sp: strip_ptr(state.sp),
            walked,
        })
    }
}

struct ProbeSnapshot {
    timing: ProbeTiming,
    pc: u64,
    lr: u64,
    fp: u64,
    sp: u64,
    walked: Vec<u64>,
}

struct ProbeRequest {
    tid: u32,
    kperf_ts: u64,
    enqueued_ticks: u64,
    coalesced_requests: u64,
}

struct ProbeSnapshotWithKey {
    tid: u32,
    timing: ProbeTiming,
    queue: ProbeQueueStats,
    pc: u64,
    lr: u64,
    fp: u64,
    sp: u64,
    walked: Vec<u64>,
}

struct ThreadState {
    pc: u64,
    lr: u64,
    fp: u64,
    sp: u64,
}

#[derive(Debug)]
enum ProbeError {
    Kernel { op: &'static str, kr: i32 },
}

struct ThreadPortCache {
    task: mach_port_t,
    by_tid: HashMap<u32, thread_act_t>,
}

impl ThreadPortCache {
    fn new(task: mach_port_t) -> Self {
        Self {
            task,
            by_tid: HashMap::new(),
        }
    }

    fn get(&mut self, tid: u32) -> Option<thread_act_t> {
        if let Some(&thread) = self.by_tid.get(&tid) {
            return Some(thread);
        }
        self.refresh();
        self.by_tid.get(&tid).copied()
    }

    fn forget(&mut self, tid: u32) {
        if let Some(thread) = self.by_tid.remove(&tid) {
            deallocate_port(thread);
        }
    }

    fn refresh(&mut self) {
        let mut list: thread_act_array_t = std::ptr::null_mut();
        let mut count: mach_msg_type_number_t = 0;
        let kr = unsafe { task_threads(self.task, &mut list, &mut count) };
        if kr != KERN_SUCCESS {
            tracing::debug!(kr, "task_threads failed while refreshing race-kperf cache");
            return;
        }

        let threads = unsafe { std::slice::from_raw_parts(list, count as usize) };
        for &thread in threads {
            match thread_id(thread) {
                Some(tid) => {
                    if self.by_tid.contains_key(&tid) {
                        deallocate_port(thread);
                    } else {
                        self.by_tid.insert(tid, thread);
                    }
                }
                None => deallocate_port(thread),
            }
        }

        let bytes = count as u64 * std::mem::size_of::<thread_act_t>() as u64;
        let _ = unsafe { mach_vm_deallocate(mach_task_self(), list as mach_vm_address_t, bytes) };
    }
}

impl Drop for ThreadPortCache {
    fn drop(&mut self) {
        for (_, thread) in self.by_tid.drain() {
            deallocate_port(thread);
        }
    }
}

fn thread_id(thread: thread_act_t) -> Option<u32> {
    let mut info = libc::thread_identifier_info_data_t {
        thread_id: 0,
        thread_handle: 0,
        dispatch_qaddr: 0,
    };
    let mut count = libc::THREAD_IDENTIFIER_INFO_COUNT;
    let kr = unsafe {
        libc::thread_info(
            thread,
            libc::THREAD_IDENTIFIER_INFO as u32,
            (&mut info as *mut libc::thread_identifier_info_data_t).cast(),
            &mut count,
        )
    };
    if kr == KERN_SUCCESS {
        u32::try_from(info.thread_id).ok()
    } else {
        None
    }
}

fn deallocate_port(port: mach_port_t) {
    let _ = unsafe { mach_port_deallocate(mach_task_self(), port) };
}

fn fp_walk(task: mach_port_t, mut fp: u64) -> Vec<u64> {
    let mut out = Vec::new();
    fp = strip_ptr(fp);
    for _ in 0..MAX_FP_FRAMES {
        if fp == 0 || fp & 0xf != 0 {
            break;
        }
        let Some((next_fp, ret)) = read_frame_record(task, fp) else {
            break;
        };
        let next_fp = strip_ptr(next_fp);
        let ret = strip_ptr(ret);
        if ret == 0 {
            break;
        }
        out.push(ret);
        if next_fp <= fp || next_fp.saturating_sub(fp) > MAX_FP_DELTA {
            break;
        }
        fp = next_fp;
    }
    out
}

fn read_frame_record(task: mach_port_t, fp: u64) -> Option<(u64, u64)> {
    let mut pair = [0u64; 2];
    let mut got: mach_vm_size_t = 0;
    let kr = unsafe {
        mach_vm_read_overwrite(
            task,
            fp as mach_vm_address_t,
            std::mem::size_of_val(&pair) as mach_vm_size_t,
            pair.as_mut_ptr() as mach_vm_address_t,
            &mut got,
        )
    };
    if kr == KERN_SUCCESS && got as usize == std::mem::size_of_val(&pair) {
        Some((pair[0], pair[1]))
    } else {
        None
    }
}

#[cfg(target_arch = "aarch64")]
fn read_thread_state(thread: thread_act_t) -> Result<ThreadState, ProbeError> {
    #[repr(C)]
    #[derive(Default)]
    struct ArmThreadState64 {
        x: [u64; 29],
        fp: u64,
        lr: u64,
        sp: u64,
        pc: u64,
        cpsr: u32,
        pad: u32,
    }

    let mut state = ArmThreadState64::default();
    let mut count: mach_msg_type_number_t =
        (std::mem::size_of::<ArmThreadState64>() / std::mem::size_of::<natural_t>()) as _;
    let kr = unsafe {
        thread_get_state(
            thread,
            mach2::thread_status::ARM_THREAD_STATE64,
            (&mut state as *mut ArmThreadState64).cast::<natural_t>() as thread_state_t,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(ProbeError::Kernel {
            op: "thread_get_state",
            kr,
        });
    }
    Ok(ThreadState {
        pc: state.pc,
        lr: state.lr,
        fp: state.fp,
        sp: state.sp,
    })
}

#[cfg(target_arch = "x86_64")]
fn read_thread_state(thread: thread_act_t) -> Result<ThreadState, ProbeError> {
    #[repr(C)]
    #[derive(Default)]
    struct X86ThreadState64 {
        rax: u64,
        rbx: u64,
        rcx: u64,
        rdx: u64,
        rdi: u64,
        rsi: u64,
        rbp: u64,
        rsp: u64,
        r8: u64,
        r9: u64,
        r10: u64,
        r11: u64,
        r12: u64,
        r13: u64,
        r14: u64,
        r15: u64,
        rip: u64,
        rflags: u64,
        cs: u64,
        fs: u64,
        gs: u64,
    }

    let mut state = X86ThreadState64::default();
    let mut count: mach_msg_type_number_t =
        (std::mem::size_of::<X86ThreadState64>() / std::mem::size_of::<natural_t>()) as _;
    let kr = unsafe {
        thread_get_state(
            thread,
            mach2::thread_status::x86_THREAD_STATE64,
            (&mut state as *mut X86ThreadState64).cast::<natural_t>() as thread_state_t,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(ProbeError::Kernel {
            op: "thread_get_state",
            kr,
        });
    }
    Ok(ThreadState {
        pc: state.rip,
        lr: 0,
        fp: state.rbp,
        sp: state.rsp,
    })
}

#[cfg(target_arch = "aarch64")]
fn strip_ptr(ptr: u64) -> u64 {
    ptr & 0x0000_ffff_ffff_ffff
}

#[cfg(not(target_arch = "aarch64"))]
fn strip_ptr(ptr: u64) -> u64 {
    ptr
}
