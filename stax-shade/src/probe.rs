use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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
use nwind::{
    CapturedImageMapping, CapturedStack, CapturedStackUnwinder, CapturedThreadState,
    CapturedUnwindError, CapturedUnwindOptions, UnwindFailure, UnwindMode,
    captured_frame_pointer_walk, strip_code_pointer, strip_data_pointer,
};
use stax_mac_capture::sample_sink::CpuIntervalEvent;
use stax_mac_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, MachOByteSource, ProbeQueueStats,
    ProbeResultEvent, ProbeTiming, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};
use staxd_client::KperfProbeTriggerTiming;

const STACK_SNAPSHOT_BYTES: usize = 512 * 1024;
const STACK_SNAPSHOT_CHUNK: usize = 16 * 1024;
const PROBE_UNWIND_QUEUE_CAPACITY: usize = 1024;
const TIMER_SLEEP_MARGIN: Duration = Duration::from_millis(1);
const TIMER_YIELD_WINDOW: Duration = Duration::from_micros(200);
const THREAD_DEMAND_DECAY_INTERVAL: Duration = Duration::from_millis(250);
const THREAD_DEMAND_RETENTION: Duration = Duration::from_secs(5);
const INFERIOR_HELPER_MAGIC: u32 = 0x3158_5453; // STX1, little-endian on the wire.
const INFERIOR_HELPER_OP_CAPTURE: u32 = 1;
const INFERIOR_HELPER_OP_HELLO: u32 = 2;
const INFERIOR_HELPER_STATUS_OK: u32 = 0;
const INFERIOR_HELPER_TIMEOUT: Duration = Duration::from_millis(50);
const INFERIOR_HELPER_RECONNECT_INTERVAL: Duration = Duration::from_millis(10);
const INFERIOR_HELPER_RESPONSE_HEADER_BYTES: usize = 88;
const INFERIOR_HELPER_THREAD_NAME: &str = "stax-inferior-helper";

#[cfg(target_os = "macos")]
const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(
        qos_class: libc::c_uint,
        relative_priority: libc::c_int,
    ) -> libc::c_int;
}

pub struct RaceKperfSink<S> {
    inner: S,
    probe: Option<RaceProbeWorker>,
}

#[derive(Clone, Copy, Debug)]
pub enum RaceProbeMode {
    /// Independently issue at most `frequency_hz` probe requests per
    /// second across the target process. Kperf observations maintain a
    /// decaying per-tid demand score, and each tick probes the tid with
    /// the highest current demand.
    Correlated { frequency_hz: u32 },
}

impl<S> RaceKperfSink<S> {
    pub fn disabled(inner: S) -> Self {
        Self { inner, probe: None }
    }

    pub fn enabled(pid: u32, task: mach_port_t, inner: S, mode: RaceProbeMode) -> Self {
        Self {
            inner,
            probe: Some(RaceProbeWorker::new(pid, task, mode)),
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
        if let Some(probe) = self.probe.as_ref() {
            probe.on_binary_loaded(&ev);
        }
        self.inner.on_binary_loaded(ev);
    }

    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>) {
        self.drain_probe_results();
        if let Some(probe) = self.probe.as_ref() {
            probe.on_binary_unloaded(&ev);
        }
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
        for event in probe.drain_results() {
            match event {
                ProbeWorkerEvent::Snapshot(result) => {
                    self.inner.on_probe_result(ProbeResultEvent {
                        tid: result.tid,
                        timing: result.timing,
                        queue: result.queue,
                        mach_pc: result.pc,
                        mach_lr: 0,
                        mach_fp: result.fp,
                        mach_sp: result.sp,
                        mach_walked: &result.fp_walked,
                        compact_walked: &result.compact_walked,
                        compact_dwarf_walked: &result.compact_dwarf_walked,
                        dwarf_walked: &result.dwarf_walked,
                        used_framehop: result.used_dwarf,
                    });
                }
                ProbeWorkerEvent::ThreadName { pid, tid, name } => {
                    self.inner
                        .on_thread_name(ThreadNameEvent { pid, tid, name });
                }
            }
        }
    }
}

struct RaceProbeWorker {
    trigger: RaceProbeTrigger,
    res_rx: Receiver<ProbeWorkerEvent>,
    image_tx: mpsc::Sender<ProbeImageUpdate>,
}

impl RaceProbeWorker {
    fn new(pid: u32, task: mach_port_t, mode: RaceProbeMode) -> Self {
        let requests = Arc::new(LatestProbeRequests::default());
        let thread_demands = Arc::new(ThreadDemandTracker::new());
        let worker_requests = requests.clone();
        let worker_thread_demands = thread_demands.clone();
        let (capture_tx, capture_rx) =
            mpsc::sync_channel::<ProbeCaptureWithKey>(PROBE_UNWIND_QUEUE_CAPACITY);
        let (image_tx, image_rx) = mpsc::channel::<ProbeImageUpdate>();
        let (res_tx, res_rx) = mpsc::channel::<ProbeWorkerEvent>();
        let res_tx_from_helper = res_tx.clone();
        let image_tx_from_probe = image_tx.clone();
        std::thread::Builder::new()
            .name("stax-race-unwind".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_unwind_worker(capture_rx, image_rx, res_tx);
                }));
                if let Err(payload) = result {
                    tracing::error!(
                        panic = %panic_payload_message(payload.as_ref()),
                        "race-kperf unwind worker panicked"
                    );
                }
            })
            .expect("spawn race unwind worker");
        std::thread::Builder::new()
            .name("stax-race-probe".to_owned())
            .spawn(move || {
                set_probe_thread_qos("race-kperf probe worker");
                tracing::info!("race-kperf probe worker started");
                let mut probe = RaceProbe::new(task);
                let mut helper =
                    connect_inprocess_helper(pid, &worker_thread_demands, &res_tx_from_helper);
                let mut helper_next_connect = Instant::now() + INFERIOR_HELPER_RECONNECT_INTERVAL;
                let mut batches: u64 = 0;
                let mut requests_seen: u64 = 0;
                let mut captures_sent: u64 = 0;
                let mut helper_captures_sent: u64 = 0;
                let mut captures_dropped: u64 = 0;
                let mut first_helper_capture_logged = false;
                let mut first_request_logged = false;
                let mut images_seeded = false;
                while let Some(batch) = worker_requests.take_all() {
                    if !images_seeded {
                        images_seeded = seed_target_images(task, &image_tx_from_probe);
                    }
                    batches += 1;
                    let worker_batch_len = batch.len() as u32;
                    for req in batch {
                        requests_seen += 1;
                        if !first_request_logged {
                            first_request_logged = true;
                            tracing::info!(
                                tid = req.tid,
                                kperf_ts = req.timing.kperf_ts,
                                coalesced = req.coalesced_requests,
                                worker_batch_len,
                                "race-kperf probe worker received first request"
                            );
                        }
                        let mut timing = req.timing;
                        timing.worker_started = unsafe { mach_absolute_time() };
                        if helper.is_none() && Instant::now() >= helper_next_connect {
                            helper =
                                connect_inprocess_helper(
                                    pid,
                                    &worker_thread_demands,
                                    &res_tx_from_helper,
                                );
                            helper_next_connect = Instant::now() + INFERIOR_HELPER_RECONNECT_INTERVAL;
                        }
                        if let Some(helper_client) = helper.as_mut() {
                            match helper_client.capture(req.tid, timing) {
                                Ok(Some(mut capture)) => {
                                    capture.queue = ProbeQueueStats {
                                        coalesced_requests: req.coalesced_requests,
                                        worker_batch_len,
                                    };
                                    let stack_bytes = capture.stack.bytes.len();
                                    match capture_tx.try_send(capture) {
                                        Ok(()) => {
                                            captures_sent += 1;
                                            helper_captures_sent =
                                                helper_captures_sent.saturating_add(1);
                                        }
                                        Err(mpsc::TrySendError::Full(_)) => {
                                            captures_dropped += 1;
                                            if captures_dropped.is_multiple_of(1024) {
                                                tracing::warn!(
                                                    captures_dropped,
                                                    captures_sent,
                                                    helper_captures_sent,
                                                    "race-kperf unwind queue full; dropping captures"
                                                );
                                            }
                                        }
                                        Err(mpsc::TrySendError::Disconnected(_)) => {
                                            tracing::info!(
                                                batches,
                                                requests_seen,
                                                captures_sent,
                                                helper_captures_sent,
                                                captures_dropped,
                                                "race-kperf unwind worker closed"
                                            );
                                            return;
                                        }
                                    }
                                    if !first_helper_capture_logged {
                                        first_helper_capture_logged = true;
                                        tracing::info!(
                                            tid = req.tid,
                                            kperf_ts = timing.kperf_ts,
                                            helper_captures_sent,
                                            stack_bytes,
                                            "race-kperf inferior helper sent first stack capture"
                                        );
                                    }
                                    continue;
                                }
                                Ok(None) => continue,
                                Err(err) => {
                                    tracing::warn!(
                                        pid,
                                        error = ?err,
                                        "race-kperf inferior helper failed; falling back to remote capture"
                                    );
                                    helper = None;
                                }
                            }
                        }
                        if let Some(capture) = probe.probe_sample(req.tid, timing) {
                            let out = ProbeCaptureWithKey {
                                tid: req.tid,
                                timing: capture.timing,
                                queue: ProbeQueueStats {
                                    coalesced_requests: req.coalesced_requests,
                                    worker_batch_len,
                                },
                                pc: capture.pc,
                                lr: capture.lr,
                                fp: capture.fp,
                                sp: capture.sp,
                                stack: capture.stack,
                            };
                            match capture_tx.try_send(out) {
                                Ok(()) => captures_sent += 1,
                                Err(mpsc::TrySendError::Full(_)) => {
                                    captures_dropped += 1;
                                    if captures_dropped.is_multiple_of(1024) {
                                        tracing::warn!(
                                            captures_dropped,
                                            captures_sent,
                                            "race-kperf unwind queue full; dropping captures"
                                        );
                                    }
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                    tracing::info!(
                                        batches,
                                        requests_seen,
                                        captures_sent,
                                        captures_dropped,
                                        "race-kperf unwind worker closed"
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }
                tracing::info!(
                    batches,
                    requests_seen,
                    captures_sent,
                    helper_captures_sent,
                    captures_dropped,
                    "race-kperf probe worker exiting"
                );
            })
            .expect("spawn race probe worker");
        let RaceProbeMode::Correlated { frequency_hz } = mode;
        let sampler_requests = requests.clone();
        let sampler_thread_demands = thread_demands.clone();
        let sampler_enqueued = Arc::new(AtomicU64::new(0));
        let sampler_enqueued_for_thread = sampler_enqueued.clone();
        std::thread::Builder::new()
            .name("stax-corr-probe-timer".to_owned())
            .spawn(move || {
                periodic_probe_sampler(
                    sampler_requests,
                    sampler_thread_demands,
                    sampler_enqueued_for_thread,
                    frequency_hz,
                )
            })
            .expect("spawn correlated probe sampler");
        Self {
            trigger: RaceProbeTrigger {
                requests,
                thread_demands,
            },
            res_rx,
            image_tx,
        }
    }

    fn trigger(&self) -> RaceProbeTrigger {
        self.trigger.clone()
    }

    fn drain_results(&self) -> Vec<ProbeWorkerEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.res_rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn on_binary_loaded(&self, ev: &BinaryLoadedEvent<'_>) {
        if ev.vmsize == 0 || ev.path.is_empty() || ev.text_bytes.is_some() {
            return;
        }
        let mapping = CapturedImageMapping::executable_text(ev.path, ev.base_avma, ev.vmsize, 0);
        let _ = self.image_tx.send(ProbeImageUpdate::Loaded(mapping));
    }

    fn on_binary_unloaded(&self, ev: &BinaryUnloadedEvent<'_>) {
        let _ = self.image_tx.send(ProbeImageUpdate::Unloaded {
            base_avma: ev.base_avma,
        });
    }
}

fn connect_inprocess_helper(
    pid: u32,
    thread_demands: &ThreadDemandTracker,
    results: &mpsc::Sender<ProbeWorkerEvent>,
) -> Option<InProcessHelperClient> {
    let helper = InProcessHelperClient::connect(pid)?;
    let helper_tid = helper.helper_tid;
    thread_demands.suppress(helper_tid);
    let _ = results.send(ProbeWorkerEvent::ThreadName {
        pid,
        tid: helper_tid,
        name: INFERIOR_HELPER_THREAD_NAME,
    });
    Some(helper)
}

impl Drop for RaceProbeWorker {
    fn drop(&mut self) {
        self.trigger.close();
    }
}

#[derive(Clone)]
pub struct RaceProbeTrigger {
    requests: Arc<LatestProbeRequests>,
    thread_demands: Arc<ThreadDemandTracker>,
}

impl RaceProbeTrigger {
    /// Record demand for this tid; the correlated sampler decides
    /// when to issue the actual probe.
    pub fn enqueue(&self, tid: u32, _trigger: KperfProbeTriggerTiming) -> bool {
        self.thread_demands.observe(tid);
        false
    }

    fn close(&self) {
        self.requests.close();
    }
}

struct ThreadDemandTracker {
    state: Mutex<ThreadDemandState>,
}

struct ThreadDemandState {
    threads: HashMap<u32, ThreadDemand>,
    suppressed: HashSet<u32>,
    last_decay: Instant,
}

struct ThreadDemand {
    demand: u32,
    observed: u64,
    probed: u64,
    last_seen: Instant,
    last_probed: Option<Instant>,
}

struct ThreadDemandChoice {
    tid: u32,
    known_tids: usize,
    demand: u32,
    deficit: u64,
}

impl ThreadDemandTracker {
    fn new() -> Self {
        Self {
            state: Mutex::new(ThreadDemandState {
                threads: HashMap::new(),
                suppressed: HashSet::new(),
                last_decay: Instant::now(),
            }),
        }
    }

    fn observe(&self, tid: u32) -> bool {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .expect("thread demand tracker lock poisoned");
        state.decay(now);
        if state.suppressed.contains(&tid) {
            state.threads.remove(&tid);
            return false;
        }
        let thread = state.threads.entry(tid).or_insert(ThreadDemand {
            demand: 0,
            observed: 0,
            probed: 0,
            last_seen: now,
            last_probed: None,
        });
        thread.demand = thread.demand.saturating_add(1);
        thread.observed = thread.observed.saturating_add(1);
        thread.last_seen = now;
        true
    }

    fn suppress(&self, tid: u32) {
        let mut state = self
            .state
            .lock()
            .expect("thread demand tracker lock poisoned");
        state.suppressed.insert(tid);
        state.threads.remove(&tid);
    }

    fn pick(&self) -> Option<ThreadDemandChoice> {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .expect("thread demand tracker lock poisoned");
        state.decay(now);
        let mut best: Option<ThreadDemandCandidate> = None;
        for (&tid, thread) in &state.threads {
            if state.suppressed.contains(&tid) {
                continue;
            }
            if thread.demand == 0 {
                continue;
            }
            let idle_ns = thread
                .last_probed
                .map(|last_probed| now.saturating_duration_since(last_probed).as_nanos())
                .unwrap_or(1_000_000_000);
            let candidate = ThreadDemandCandidate {
                tid,
                demand: thread.demand,
                deficit: thread.observed.saturating_sub(thread.probed),
                urgency: u128::from(thread.demand).saturating_mul(idle_ns),
            };
            if best
                .as_ref()
                .is_none_or(|current| candidate.is_better_than(current))
            {
                best = Some(candidate);
            }
        }
        let best = best?;
        let known_tids = state.threads.len();
        let thread = state
            .threads
            .get_mut(&best.tid)
            .expect("selected thread disappeared from demand tracker");
        thread.probed = thread.probed.saturating_add(1);
        thread.last_probed = Some(now);
        Some(ThreadDemandChoice {
            tid: best.tid,
            known_tids,
            demand: best.demand,
            deficit: best.deficit,
        })
    }
}

impl ThreadDemandState {
    fn decay(&mut self, now: Instant) {
        if now.duration_since(self.last_decay) < THREAD_DEMAND_DECAY_INTERVAL {
            return;
        }
        self.last_decay = now;
        for thread in self.threads.values_mut() {
            thread.demand /= 2;
        }
        self.threads.retain(|_, thread| {
            thread.demand > 0 || now.duration_since(thread.last_seen) < THREAD_DEMAND_RETENTION
        });
    }
}

struct ThreadDemandCandidate {
    tid: u32,
    demand: u32,
    deficit: u64,
    urgency: u128,
}

impl ThreadDemandCandidate {
    fn is_better_than(&self, other: &Self) -> bool {
        (
            self.urgency,
            self.demand,
            self.deficit,
            std::cmp::Reverse(self.tid),
        ) > (
            other.urgency,
            other.demand,
            other.deficit,
            std::cmp::Reverse(other.tid),
        )
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
        if state.closed {
            return false;
        }
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

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .expect("race probe request lock poisoned")
            .closed
    }
}

fn periodic_probe_sampler(
    requests: Arc<LatestProbeRequests>,
    thread_demands: Arc<ThreadDemandTracker>,
    enqueued_probe_requests: Arc<AtomicU64>,
    frequency_hz: u32,
) {
    set_probe_thread_qos("correlated kperf probe sampler");
    let frequency_hz = frequency_hz.max(1);
    let interval = Duration::from_nanos((1_000_000_000u64 / u64::from(frequency_hz)).max(1));
    let mut first_logged = false;
    let mut ticks: u64 = 0;
    let mut next_tick = Instant::now();
    tracing::info!(
        frequency_hz,
        interval_ns = interval.as_nanos() as u64,
        "correlated kperf probe sampler started"
    );
    while !requests.is_closed() {
        next_tick += interval;
        if let Some(choice) = thread_demands.pick() {
            ticks = ticks.saturating_add(1);
            let request_ticks = unsafe { mach_absolute_time() };
            let enqueued = enqueued_probe_requests
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            let replaced = requests.push(ProbeRequest {
                tid: choice.tid,
                timing: correlated_probe_timing(request_ticks),
                coalesced_requests: 0,
            });
            if !first_logged {
                first_logged = true;
                tracing::info!(
                    tid = choice.tid,
                    probe_ts = request_ticks,
                    known_tids = choice.known_tids,
                    demand = choice.demand,
                    deficit = choice.deficit,
                    replaced,
                    "correlated kperf first periodic probe request enqueued"
                );
            } else if enqueued.is_multiple_of(4096) {
                tracing::debug!(
                    enqueued,
                    ticks,
                    tid = choice.tid,
                    known_tids = choice.known_tids,
                    demand = choice.demand,
                    deficit = choice.deficit,
                    "correlated kperf periodic probe requests enqueued"
                );
            }
        }
        wait_until(next_tick);
    }
    tracing::info!(
        ticks,
        enqueued = enqueued_probe_requests.load(Ordering::Relaxed),
        "correlated kperf probe sampler exiting"
    );
}

fn wait_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline - now;
        if remaining > TIMER_SLEEP_MARGIN + TIMER_YIELD_WINDOW {
            std::thread::sleep(remaining - TIMER_SLEEP_MARGIN);
        } else if remaining > TIMER_YIELD_WINDOW {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

fn set_probe_thread_qos(label: &'static str) {
    #[cfg(target_os = "macos")]
    unsafe {
        let rc = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        if rc != 0 {
            tracing::warn!(errno = rc, label, "failed to raise probe thread QoS");
        }
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

    fn probe_sample(&mut self, tid: u32, mut timing: ProbeTiming) -> Option<ProbeCapture> {
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
    ) -> Result<ProbeCapture, ProbeError> {
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
        let stack = copy_stack_window(self.task, state.sp);
        timing.state_done = unsafe { mach_absolute_time() };
        let resume_kr = unsafe { thread_resume(thread) };
        timing.resume_done = unsafe { mach_absolute_time() };
        if resume_kr != KERN_SUCCESS {
            return Err(ProbeError::Kernel {
                op: "thread_resume",
                kr: resume_kr,
            });
        }

        Ok(ProbeCapture {
            timing,
            pc: strip_code_pointer(state.pc),
            lr: strip_code_pointer(state.lr),
            fp: strip_data_pointer(state.fp),
            sp: strip_data_pointer(state.sp),
            stack,
        })
    }
}

struct ProbeCapture {
    timing: ProbeTiming,
    pc: u64,
    lr: u64,
    fp: u64,
    sp: u64,
    stack: StackSnapshot,
}

struct ProbeRequest {
    tid: u32,
    timing: ProbeTiming,
    coalesced_requests: u64,
}

fn correlated_probe_timing(request_ticks: u64) -> ProbeTiming {
    ProbeTiming {
        kperf_ts: request_ticks,
        client_received: request_ticks,
        enqueued: request_ticks,
        ..ProbeTiming::default()
    }
}

struct ProbeSnapshotWithKey {
    tid: u32,
    timing: ProbeTiming,
    queue: ProbeQueueStats,
    pc: u64,
    fp: u64,
    sp: u64,
    fp_walked: Vec<u64>,
    compact_walked: Vec<u64>,
    compact_dwarf_walked: Vec<u64>,
    dwarf_walked: Vec<u64>,
    used_dwarf: bool,
}

enum ProbeImageUpdate {
    Loaded(CapturedImageMapping),
    Unloaded { base_avma: u64 },
}

#[derive(Default)]
struct DwarfFailureStats {
    no_mappings: u64,
    no_mapped_regions: u64,
    missing_stack_pointer: u64,
    missing_instruction_pointer: u64,
    empty_stack: u64,
    no_binary: u64,
    no_unwind_info: u64,
    null_instruction_pointer: u64,
    missing_cfa: u64,
    missing_cfa_register: u64,
    cfa_expression_failed: u64,
    register_memory_read_failed: u64,
    register_expression_failed: u64,
    unsupported_register_rule: u64,
    missing_return_address: u64,
    no_caller_frames: u64,
    other: u64,
}

impl DwarfFailureStats {
    fn record(&mut self, error: &CapturedUnwindError) {
        match error {
            CapturedUnwindError::NoMappings => self.no_mappings += 1,
            CapturedUnwindError::NoMappedRegions => self.no_mapped_regions += 1,
            CapturedUnwindError::MissingStackPointer => self.missing_stack_pointer += 1,
            CapturedUnwindError::MissingInstructionPointer => self.missing_instruction_pointer += 1,
            CapturedUnwindError::EmptyStack => self.empty_stack += 1,
            CapturedUnwindError::OnlyLeafFrame { reason } => match reason {
                Some(UnwindFailure::NoBinary) => self.no_binary += 1,
                Some(UnwindFailure::NoUnwindInfo) => self.no_unwind_info += 1,
                Some(UnwindFailure::NullInstructionPointer) => self.null_instruction_pointer += 1,
                Some(UnwindFailure::MissingCfa) => self.missing_cfa += 1,
                Some(UnwindFailure::MissingCfaRegister) => self.missing_cfa_register += 1,
                Some(UnwindFailure::CfaExpressionFailed) => self.cfa_expression_failed += 1,
                Some(UnwindFailure::RegisterMemoryReadFailed) => {
                    self.register_memory_read_failed += 1
                }
                Some(UnwindFailure::RegisterExpressionFailed) => {
                    self.register_expression_failed += 1
                }
                Some(UnwindFailure::UnsupportedRegisterRule) => self.unsupported_register_rule += 1,
                Some(UnwindFailure::MissingReturnAddress) => self.missing_return_address += 1,
                Some(UnwindFailure::MissingInstructionPointer) => {
                    self.missing_instruction_pointer += 1
                }
                Some(_) => self.other += 1,
                None => self.no_caller_frames += 1,
            },
        }
    }
}

fn run_unwind_worker(
    capture_rx: mpsc::Receiver<ProbeCaptureWithKey>,
    image_rx: mpsc::Receiver<ProbeImageUpdate>,
    res_tx: mpsc::Sender<ProbeWorkerEvent>,
) {
    set_probe_thread_qos("race-kperf unwind worker");
    tracing::info!("race-kperf unwind worker started");
    let mut results_sent: u64 = 0;
    let mut dwarf_successes: u64 = 0;
    let mut dwarf_failures: u64 = 0;
    let mut dwarf_bridge_attempts: u64 = 0;
    let mut dwarf_bridge_successes: u64 = 0;
    let mut dwarf_bridge_steps: u64 = 0;
    let mut fp_successes: u64 = 0;
    let mut dwarf_failure_stats = DwarfFailureStats::default();
    let mut first_result_logged = false;
    let mut first_compact_failure_logged = false;
    let mut first_compact_dwarf_failure_logged = false;
    let mut unwinder = CapturedStackUnwinder::new();
    let mut compact_frames = Vec::new();
    let mut compact_dwarf_frames = Vec::new();
    let mut dwarf_frames = Vec::new();
    while let Ok(capture) = capture_rx.recv() {
        drain_image_updates(&mut unwinder, &image_rx);
        let mut timing = capture.timing;
        let fp_walked = captured_frame_pointer_walk(
            capture.thread_state(),
            capture.stack(),
            CapturedUnwindOptions::DEFAULT_MAX_FRAMES,
        );
        if !fp_walked.is_empty() {
            fp_successes = fp_successes.saturating_add(1);
        }
        let compact_walked = match unwinder.unwind_callers(
            capture.thread_state(),
            capture.stack(),
            &mut compact_frames,
            CapturedUnwindOptions::metadata(UnwindMode::CompactOnly),
        ) {
            Ok(outcome) => outcome.callers,
            Err(failure) => {
                if !first_compact_failure_logged {
                    first_compact_failure_logged = true;
                    tracing::debug!(
                        tid = capture.tid,
                        kperf_ts = capture.timing.kperf_ts,
                        error = ?failure.error,
                        bridge_attempted = failure.bridge_attempted,
                        bridge_steps = failure.bridge_steps,
                        "race-kperf compact-only unwind produced no caller frames"
                    );
                }
                Vec::new()
            }
        };
        let compact_dwarf_walked = match unwinder.unwind_callers(
            capture.thread_state(),
            capture.stack(),
            &mut compact_dwarf_frames,
            CapturedUnwindOptions::metadata(UnwindMode::CompactWithDwarfRefs),
        ) {
            Ok(outcome) => outcome.callers,
            Err(failure) => {
                if !first_compact_dwarf_failure_logged {
                    first_compact_dwarf_failure_logged = true;
                    tracing::debug!(
                        tid = capture.tid,
                        kperf_ts = capture.timing.kperf_ts,
                        error = ?failure.error,
                        bridge_attempted = failure.bridge_attempted,
                        bridge_steps = failure.bridge_steps,
                        "race-kperf compact+fde unwind produced no caller frames"
                    );
                }
                Vec::new()
            }
        };
        let dwarf_walked = match unwinder.unwind_callers(
            capture.thread_state(),
            capture.stack(),
            &mut dwarf_frames,
            CapturedUnwindOptions::dwarf_with_fp_bridge(),
        ) {
            Ok(outcome) => {
                dwarf_successes = dwarf_successes.saturating_add(1);
                if outcome.bridge_attempted {
                    dwarf_bridge_attempts = dwarf_bridge_attempts.saturating_add(1);
                    dwarf_bridge_successes = dwarf_bridge_successes.saturating_add(1);
                    dwarf_bridge_steps =
                        dwarf_bridge_steps.saturating_add(outcome.bridge_steps as u64);
                }
                outcome.callers
            }
            Err(failure) => {
                dwarf_failures = dwarf_failures.saturating_add(1);
                if failure.bridge_attempted {
                    dwarf_bridge_attempts = dwarf_bridge_attempts.saturating_add(1);
                    dwarf_bridge_steps =
                        dwarf_bridge_steps.saturating_add(failure.bridge_steps as u64);
                }
                dwarf_failure_stats.record(&failure.error);
                if dwarf_failures == 1 || dwarf_failures.is_multiple_of(1024) {
                    let reload = unwinder.last_reload();
                    tracing::warn!(
                        tid = capture.tid,
                        kperf_ts = capture.timing.kperf_ts,
                        error = ?failure.error,
                        bridge_attempted = failure.bridge_attempted,
                        bridge_steps = failure.bridge_steps,
                        mapped_regions = reload.mapped_regions,
                        loaded_binaries = reload.loaded_binaries,
                        load_failures = reload.load_failures.len(),
                        dwarf_failures,
                        dwarf_bridge_attempts,
                        dwarf_bridge_successes,
                        dwarf_bridge_steps,
                        no_mappings = dwarf_failure_stats.no_mappings,
                        no_mapped_regions = dwarf_failure_stats.no_mapped_regions,
                        missing_sp = dwarf_failure_stats.missing_stack_pointer,
                        missing_ip = dwarf_failure_stats.missing_instruction_pointer,
                        empty_stack = dwarf_failure_stats.empty_stack,
                        no_binary = dwarf_failure_stats.no_binary,
                        no_unwind_info = dwarf_failure_stats.no_unwind_info,
                        null_ip = dwarf_failure_stats.null_instruction_pointer,
                        missing_cfa = dwarf_failure_stats.missing_cfa,
                        missing_cfa_register = dwarf_failure_stats.missing_cfa_register,
                        cfa_expression_failed = dwarf_failure_stats.cfa_expression_failed,
                        register_memory_read_failed =
                            dwarf_failure_stats.register_memory_read_failed,
                        register_expression_failed = dwarf_failure_stats.register_expression_failed,
                        unsupported_register_rule = dwarf_failure_stats.unsupported_register_rule,
                        missing_return_address = dwarf_failure_stats.missing_return_address,
                        no_caller_frames = dwarf_failure_stats.no_caller_frames,
                        other_failures = dwarf_failure_stats.other,
                        "race-kperf dwarf unwind failed; keeping FP validator only"
                    );
                }
                Vec::new()
            }
        };
        timing.walk_done = unsafe { mach_absolute_time() };
        let out = ProbeSnapshotWithKey {
            tid: capture.tid,
            timing,
            queue: capture.queue,
            pc: capture.pc,
            fp: capture.fp,
            sp: capture.sp,
            fp_walked,
            compact_walked,
            compact_dwarf_walked,
            used_dwarf: !dwarf_walked.is_empty(),
            dwarf_walked,
        };
        if res_tx.send(ProbeWorkerEvent::Snapshot(out)).is_err() {
            tracing::info!(results_sent, "race-kperf probe result receiver closed");
            return;
        }
        results_sent += 1;
        if !first_result_logged {
            first_result_logged = true;
            tracing::info!(
                tid = capture.tid,
                kperf_ts = capture.timing.kperf_ts,
                results_sent,
                coalesced = capture.queue.coalesced_requests,
                worker_batch_len = capture.queue.worker_batch_len,
                stack_bytes = capture.stack.bytes.len(),
                fp_successes,
                dwarf_successes,
                dwarf_failures,
                dwarf_bridge_attempts,
                dwarf_bridge_successes,
                dwarf_bridge_steps,
                "race-kperf unwind worker sent first result"
            );
        }
    }
    tracing::info!(
        results_sent,
        fp_successes,
        dwarf_successes,
        dwarf_failures,
        dwarf_bridge_attempts,
        dwarf_bridge_successes,
        dwarf_bridge_steps,
        no_mappings = dwarf_failure_stats.no_mappings,
        no_mapped_regions = dwarf_failure_stats.no_mapped_regions,
        missing_sp = dwarf_failure_stats.missing_stack_pointer,
        missing_ip = dwarf_failure_stats.missing_instruction_pointer,
        empty_stack = dwarf_failure_stats.empty_stack,
        no_binary = dwarf_failure_stats.no_binary,
        no_unwind_info = dwarf_failure_stats.no_unwind_info,
        null_ip = dwarf_failure_stats.null_instruction_pointer,
        missing_cfa = dwarf_failure_stats.missing_cfa,
        missing_cfa_register = dwarf_failure_stats.missing_cfa_register,
        cfa_expression_failed = dwarf_failure_stats.cfa_expression_failed,
        register_memory_read_failed = dwarf_failure_stats.register_memory_read_failed,
        register_expression_failed = dwarf_failure_stats.register_expression_failed,
        unsupported_register_rule = dwarf_failure_stats.unsupported_register_rule,
        missing_return_address = dwarf_failure_stats.missing_return_address,
        no_caller_frames = dwarf_failure_stats.no_caller_frames,
        other_failures = dwarf_failure_stats.other,
        "race-kperf unwind worker exiting"
    );
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

fn seed_target_images(task: mach_port_t, image_tx: &mpsc::Sender<ProbeImageUpdate>) -> bool {
    let walker = stax_target_images::TargetImageWalker::new(task);
    let images = match walker.enumerate() {
        Ok(images) => images,
        Err(error) => {
            tracing::debug!(?error, "race-kperf target image seed failed");
            return false;
        }
    };

    let mut sent = 0usize;
    let mut skipped = 0usize;
    for image in images {
        let Some(sections) = image.sections.as_ref() else {
            skipped += 1;
            continue;
        };
        let Some(text) = sections.text_avma.as_ref() else {
            skipped += 1;
            continue;
        };
        if text.start >= text.end {
            skipped += 1;
            continue;
        }
        let mapping =
            CapturedImageMapping::executable_text(image.path, text.start, text.end - text.start, 0);
        if image_tx.send(ProbeImageUpdate::Loaded(mapping)).is_ok() {
            sent += 1;
        }
    }

    tracing::info!(
        sent,
        skipped,
        "race-kperf seeded target image mappings for dwarf unwind"
    );
    sent > 0
}

fn drain_image_updates(
    unwinder: &mut CapturedStackUnwinder,
    image_rx: &mpsc::Receiver<ProbeImageUpdate>,
) {
    while let Ok(update) = image_rx.try_recv() {
        match update {
            ProbeImageUpdate::Loaded(mapping) => unwinder.add_mapping(mapping),
            ProbeImageUpdate::Unloaded { base_avma } => unwinder.remove_mapping_by_start(base_avma),
        }
    }
}

impl ProbeCaptureWithKey {
    fn thread_state(&self) -> CapturedThreadState {
        CapturedThreadState::new(self.pc, self.lr, self.fp, self.sp)
    }

    fn stack(&self) -> CapturedStack<'_> {
        CapturedStack::new(self.stack.base, &self.stack.bytes)
    }
}

enum ProbeWorkerEvent {
    Snapshot(ProbeSnapshotWithKey),
    ThreadName {
        pid: u32,
        tid: u32,
        name: &'static str,
    },
}

struct ProbeCaptureWithKey {
    tid: u32,
    timing: ProbeTiming,
    queue: ProbeQueueStats,
    pc: u64,
    lr: u64,
    fp: u64,
    sp: u64,
    stack: StackSnapshot,
}

struct InProcessHelperClient {
    stream: UnixStream,
    next_seq: u64,
    helper_tid: u32,
}

impl InProcessHelperClient {
    fn connect(pid: u32) -> Option<Self> {
        let path = inferior_helper_socket_path(pid);
        match UnixStream::connect(&path) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(INFERIOR_HELPER_TIMEOUT));
                let _ = stream.set_write_timeout(Some(INFERIOR_HELPER_TIMEOUT));
                let mut client = Self {
                    stream,
                    next_seq: 1,
                    helper_tid: 0,
                };
                match client.query_helper_tid() {
                    Ok(helper_tid) => {
                        client.helper_tid = helper_tid;
                        tracing::info!(
                            pid,
                            helper_tid,
                            path = %path.display(),
                            "connected to inferior stack helper"
                        );
                        Some(client)
                    }
                    Err(err) => {
                        tracing::warn!(
                            pid,
                            path = %path.display(),
                            error = ?err,
                            "inferior stack helper hello failed"
                        );
                        None
                    }
                }
            }
            Err(err) => {
                tracing::debug!(
                    pid,
                    path = %path.display(),
                    error = ?err,
                    "inferior stack helper unavailable"
                );
                None
            }
        }
    }

    fn query_helper_tid(&mut self) -> std::io::Result<u32> {
        let (status, header, _) = self.roundtrip(INFERIOR_HELPER_OP_HELLO, 0)?;
        if status != INFERIOR_HELPER_STATUS_OK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("inferior helper hello status {status}"),
            ));
        }
        u32::try_from(read_u64(&header, 48)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "inferior helper tid does not fit u32",
            )
        })
    }

    fn capture(
        &mut self,
        tid: u32,
        mut timing: ProbeTiming,
    ) -> std::io::Result<Option<ProbeCaptureWithKey>> {
        let (status, header, stack_bytes) = self.roundtrip(INFERIOR_HELPER_OP_CAPTURE, tid)?;
        if status != INFERIOR_HELPER_STATUS_OK {
            return Ok(None);
        }

        timing.thread_lookup_done = read_u64(&header, 16);
        timing.state_done = read_u64(&header, 24);
        timing.resume_done = read_u64(&header, 32);

        Ok(Some(ProbeCaptureWithKey {
            tid,
            timing,
            queue: ProbeQueueStats::default(),
            pc: strip_code_pointer(read_u64(&header, 48)),
            lr: strip_code_pointer(read_u64(&header, 56)),
            fp: strip_data_pointer(read_u64(&header, 64)),
            sp: strip_data_pointer(read_u64(&header, 72)),
            stack: StackSnapshot {
                base: strip_data_pointer(read_u64(&header, 72)),
                bytes: stack_bytes,
            },
        }))
    }

    fn roundtrip(&mut self, op: u32, tid: u32) -> std::io::Result<(u32, [u8; 88], Vec<u8>)> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);

        let mut request = [0u8; 24];
        write_u32(&mut request, 0, INFERIOR_HELPER_MAGIC);
        write_u32(&mut request, 4, op);
        write_u64(&mut request, 8, seq);
        write_u32(&mut request, 16, tid);
        self.stream.write_all(&request)?;

        let mut header = [0u8; INFERIOR_HELPER_RESPONSE_HEADER_BYTES];
        self.stream.read_exact(&mut header)?;
        let magic = read_u32(&header, 0);
        if magic != INFERIOR_HELPER_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad inferior helper magic {magic:#x}"),
            ));
        }
        let status = read_u32(&header, 4);
        let response_seq = read_u64(&header, 8);
        if response_seq != seq {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("inferior helper response seq {response_seq} != request seq {seq}"),
            ));
        }
        let payload_len = read_u32(&header, 80) as usize;
        if payload_len > STACK_SNAPSHOT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("inferior helper returned {payload_len} stack bytes"),
            ));
        }
        let mut payload = vec![0u8; payload_len];
        self.stream.read_exact(&mut payload)?;
        Ok((status, header, payload))
    }
}

fn inferior_helper_socket_path(pid: u32) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/stax-inferior-helper-{pid}.sock"))
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().expect("u32 field"))
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().expect("u64 field"))
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

struct ThreadState {
    pc: u64,
    lr: u64,
    fp: u64,
    sp: u64,
}

struct StackSnapshot {
    base: u64,
    bytes: Vec<u8>,
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

fn copy_stack_window(task: mach_port_t, sp: u64) -> StackSnapshot {
    let base = strip_data_pointer(sp);
    let mut bytes = vec![0u8; STACK_SNAPSHOT_BYTES];
    let mut copied = 0usize;
    while copied < STACK_SNAPSHOT_BYTES {
        let chunk = (STACK_SNAPSHOT_BYTES - copied).min(STACK_SNAPSHOT_CHUNK);
        let mut got: mach_vm_size_t = 0;
        let kr = unsafe {
            mach_vm_read_overwrite(
                task,
                base.saturating_add(copied as u64) as mach_vm_address_t,
                chunk as mach_vm_size_t,
                bytes[copied..].as_mut_ptr() as mach_vm_address_t,
                &mut got,
            )
        };
        if kr != KERN_SUCCESS || got == 0 {
            break;
        }
        copied = copied.saturating_add(got as usize);
        if got as usize != chunk {
            break;
        }
    }
    bytes.truncate(copied);
    StackSnapshot { base, bytes }
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
