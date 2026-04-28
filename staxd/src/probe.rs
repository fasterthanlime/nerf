//! Race-against-return measurement prototype.
//!
//! On every kperf sample (one per (tid, PET tick)) we fire a
//! `thread_get_state` against the originating thread and record:
//!
//! - drift between the kdebug record's timestamp and the probe
//! - whether the leaf user PC kperf observed equals the PC the
//!   Mach call returns *now* (after PAC strip)
//! - same for the appended LR
//!
//! When the PCs match, the thread either was still in kernel mode
//! when we issued `thread_get_state` (saved trapframe was still
//! frozen) or the thread is back in user mode but hasn't moved
//! past the leaf — either way, the registers we get *are* the
//! atomic-at-PMI user state for that sample. That's the foundation
//! for atomic-stitched DWARF unwinding without a kext, but the open
//! question this prototype answers is: how often does the match
//! actually hold under the current kdebug→userspace pipeline
//! latency?
//!
//! Each probe emits one `tracing::info!` line under the
//! `staxd::probe` target with structured fields (queryable via
//! `log show --predicate 'subsystem == "eu.bearcove.staxd"'`).
//! A summary fires at session end. Nothing here changes the
//! records staxd ships to the client; the probe is purely
//! side-instrumentation.

use std::collections::HashMap;
use std::time::Duration;

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_types::thread_act_array_t;
use mach2::message::mach_msg_type_number_t;
use mach2::port::{mach_port_t, MACH_PORT_NULL};
use mach2::structs::arm_thread_state64_t;
use mach2::task::task_threads;
use mach2::thread_act::{thread_get_state, thread_resume, thread_suspend};
use mach2::thread_status::ARM_THREAD_STATE64;
use mach2::traps::{mach_task_self, task_for_pid};
use staxd_proto::ProbeResultWire;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::unwind::{self, TargetUnwinder};

/// Handles for the probe back-channel. The drain loop pushes
/// `ProbeRequest`s onto `requests` and drains accumulated
/// `ProbeResultWire`s from `results` for inclusion in the next
/// `KdBufBatch`.
pub struct ProbeChannel {
    pub requests: mpsc::Sender<ProbeRequest>,
    pub results: mpsc::Receiver<ProbeResultWire>,
}

/// One sample-completed cue from the drain loop: "fire a probe
/// against `tid`, the matching kperf sample carried timestamp
/// `kperf_ts_mach`". The probe worker only needs these two
/// fields; the kperf-walked user backtrace ships independently
/// in the same `KdBufBatch` and the client side correlates by
/// `(tid, kperf_ts_mach)`.
#[derive(Debug, Copy, Clone)]
pub struct ProbeRequest {
    pub tid: u32,
    pub kperf_ts_mach: u64,
}

/// Probe-side bookkeeping. Lives on a dedicated tokio task so the
/// `read_trace` loop never blocks on a Mach call.
struct ProbeWorker {
    target_pid: u32,
    task: mach_port_t,
    /// `tid -> thread_act_t`. Built lazily on miss via
    /// `task_threads`; on `thread_suspend` failure (port stale)
    /// we evict and rebuild.
    tid_cache: HashMap<u32, mach_port_t>,
    /// framehop unwinder built from the target's loaded images.
    /// `None` if dyld walk failed at session start; we then fall
    /// back to the simple FP walker.
    unwinder: Option<TargetUnwinder>,
    /// Send results back to the drain loop for inclusion in the
    /// next `KdBufBatch`. `try_send`; if the channel is full the
    /// drain loop is slow, count the drop and move on.
    results_tx: mpsc::Sender<ProbeResultWire>,
    stats: ProbeStats,
}

#[derive(Default)]
struct ProbeStats {
    probes_attempted: u64,
    /// Probes that returned KERN_SUCCESS for both
    /// `thread_get_state` and `thread_resume`.
    probes_ok: u64,
    /// Probes whose `ProbeResultWire` was successfully shipped
    /// back to the drain loop.
    results_shipped: u64,
    /// Probes whose result was dropped because the back-channel
    /// to the drain loop was full.
    results_dropped: u64,
    /// Number of probes where the stack walk yielded zero frames
    /// (target memory unreadable, FP=0, framehop bailed early).
    walk_empty: u64,
    /// Whether framehop was used (vs FP-walk fallback). Logged at
    /// session-end summary, not per-probe.
    framehop_used: u64,
    fp_walk_used: u64,
}

/// Cap on stack walk depth. kperf's MAX_CALLSTACK_FRAMES is
/// similar; pick something reasonable that won't blow up wire
/// size but covers any legitimate stack.
const MAX_WALK_FRAMES: usize = 64;

/// Spawn a probe worker bound to `target_pid`. Returns the request
/// sender + result receiver. Channel is bounded so a stuck worker
/// or a stuck drain loop doesn't unbound-grow memory; both sides
/// `try_send` and count drops.
pub fn spawn(target_pid: u32) -> Option<ProbeChannel> {
    let task = match task_for_pid_root(target_pid) {
        Ok(t) => t,
        Err(kr) => {
            warn!(target_pid, kr, "probe: task_for_pid failed; skipping probe");
            return None;
        }
    };

    let unwinder = unwind::build(task);
    if let Some(tu) = unwinder.as_ref() {
        info!(
            target_pid,
            images_total = tu.stats.images_total,
            modules_added = tu.stats.modules_added,
            with_unwind_info = tu.stats.with_unwind_info,
            with_eh_frame = tu.stats.with_eh_frame,
            "probe: framehop unwinder built"
        );
    } else {
        warn!(
            target_pid,
            "probe: framehop unwinder unavailable; falling back to FP walk"
        );
    }

    info!(target_pid, "probe: race-against-return capture started; results streamed back to drain loop");

    let (req_tx, mut req_rx) = mpsc::channel::<ProbeRequest>(4096);
    let (res_tx, res_rx) = mpsc::channel::<ProbeResultWire>(4096);

    let mut worker = ProbeWorker {
        target_pid,
        task,
        tid_cache: HashMap::new(),
        unwinder,
        results_tx: res_tx,
        stats: ProbeStats::default(),
    };

    tokio::spawn(async move {
        // Refresh the tid cache periodically — threads come and
        // go in any non-trivial app. Cheap, takes microseconds.
        let mut refresh = tokio::time::interval(Duration::from_secs(1));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                req = req_rx.recv() => match req {
                    Some(r) => worker.probe_one(r),
                    None => break,
                },
                _ = refresh.tick() => worker.refresh_tid_cache(),
            }
        }

        worker.flush_and_log_summary();
    });

    Some(ProbeChannel {
        requests: req_tx,
        results: res_rx,
    })
}

/// `task_for_pid` from a root process — we have no entitlement
/// requirement here; staxd runs as the LaunchDaemon.
fn task_for_pid_root(pid: u32) -> Result<mach_port_t, i32> {
    let mut task: mach_port_t = MACH_PORT_NULL;
    // SAFETY: out-pointer is valid for the duration of the call;
    // pid is plain integer; mach_task_self is always-safe.
    let kr = unsafe { task_for_pid(mach_task_self(), pid as i32, &mut task) };
    if kr == KERN_SUCCESS {
        Ok(task)
    } else {
        Err(kr)
    }
}

impl ProbeWorker {
    fn refresh_tid_cache(&mut self) {
        let mut threads: thread_act_array_t = std::ptr::null_mut();
        let mut count: mach_msg_type_number_t = 0;
        // SAFETY: task is a live port; out-params owned by us.
        let kr = unsafe { task_threads(self.task, &mut threads, &mut count) };
        if kr != KERN_SUCCESS {
            warn!(kr, "probe: task_threads refresh failed");
            return;
        }

        // Rebuild cache from scratch. Mach thread ports are tids
        // on macOS — we use thread_info(THREAD_IDENTIFIER_INFO)
        // to map mach_port_t → kernel tid the kdebug stream uses.
        self.tid_cache.clear();
        for i in 0..count as usize {
            // SAFETY: kernel-allocated array of `count` ports.
            let port = unsafe { *threads.add(i) };
            if let Some(tid) = thread_port_to_kernel_tid(port) {
                self.tid_cache.insert(tid, port);
            }
        }

        // Free the array storage. We deliberately don't
        // mach_port_deallocate the individual thread ports —
        // they live in the cache and we'll release them when
        // the cache rebuilds (kernel cleans up at task exit too).
        let bytes = (count as usize)
            .saturating_mul(std::mem::size_of::<mach_port_t>()) as u64;
        // SAFETY: kernel handed us this buffer via task_threads.
        unsafe {
            let _ = mach2::vm::mach_vm_deallocate(
                mach_task_self(),
                threads as mach2::vm_types::mach_vm_address_t,
                bytes,
            );
        }
    }

    fn probe_one(&mut self, req: ProbeRequest) {
        self.stats.probes_attempted += 1;

        if !self.tid_cache.contains_key(&req.tid) {
            self.refresh_tid_cache();
        }
        let Some(port) = self.tid_cache.get(&req.tid).copied() else {
            return;
        };

        // SAFETY: port came from task_threads; valid until task exit.
        let kr_susp = unsafe { thread_suspend(port) };
        if kr_susp != KERN_SUCCESS {
            self.tid_cache.remove(&req.tid);
            return;
        }

        // SAFETY: state is a fresh zeroed struct of the right size.
        let mut state: arm_thread_state64_t = unsafe { std::mem::zeroed() };
        let mut count = (std::mem::size_of::<arm_thread_state64_t>()
            / std::mem::size_of::<u32>()) as mach_msg_type_number_t;
        let kr_get = unsafe {
            thread_get_state(
                port,
                ARM_THREAD_STATE64,
                (&mut state) as *mut _ as *mut u32,
                &mut count,
            )
        };

        // Walk while the thread is still suspended so the stack
        // image is consistent. framehop (DWARF / compact unwind)
        // first; FP-walk fallback if the unwinder failed to build
        // at session start.
        let (mach_walked, used_framehop): (Vec<u64>, bool) = if kr_get != KERN_SUCCESS {
            (Vec::new(), false)
        } else if let Some(tu) = self.unwinder.as_mut() {
            let frames = unwind::walk(
                tu,
                state.__pc,
                state.__lr,
                state.__sp,
                state.__fp,
                MAX_WALK_FRAMES,
            )
            .into_iter()
            .map(pac_strip)
            .collect();
            (frames, true)
        } else {
            (fp_walk(self.task, state.__fp, MAX_WALK_FRAMES), false)
        };

        let probe_done_mach = mach_now();

        // SAFETY: paired resume.
        let _ = unsafe { thread_resume(port) };

        if kr_get != KERN_SUCCESS {
            return;
        }
        self.stats.probes_ok += 1;
        if used_framehop {
            self.stats.framehop_used += 1;
        } else {
            self.stats.fp_walk_used += 1;
        }
        if mach_walked.is_empty() {
            self.stats.walk_empty += 1;
        }

        let result = ProbeResultWire {
            tid: req.tid,
            kperf_ts_mach: req.kperf_ts_mach,
            probe_done_mach,
            mach_pc: pac_strip(state.__pc),
            mach_lr: pac_strip(state.__lr),
            mach_fp: state.__fp,
            mach_sp: state.__sp,
            mach_walked,
            used_framehop,
        };
        match self.results_tx.try_send(result) {
            Ok(()) => self.stats.results_shipped += 1,
            Err(_) => self.stats.results_dropped += 1,
        }
    }

    fn flush_and_log_summary(&mut self) {
        let total = self.stats.probes_attempted.max(1);
        info!(
            target_pid = self.target_pid,
            attempted = total,
            ok = self.stats.probes_ok,
            ok_rate_pct = (self.stats.probes_ok * 100) / total,
            results_shipped = self.stats.results_shipped,
            results_dropped = self.stats.results_dropped,
            walk_empty = self.stats.walk_empty,
            framehop_used = self.stats.framehop_used,
            fp_walk_used = self.stats.fp_walk_used,
            "probe: race-against-return summary"
        );
    }
}

/// Walk the FP chain in the target task starting at `fp` and
/// return up to `max` PAC-stripped return addresses. Mirrors what
/// kperf does in xnu's backtrace_user (osfmk/kern/backtrace.c) but
/// reads memory via `mach_vm_read_overwrite` instead of in-kernel
/// pmap_copyin. Each frame is 16 bytes on arm64: { prev_fp,
/// ret_addr }. Stops on FP=0, misalignment (caught loop), or read
/// failure.
fn fp_walk(task: mach_port_t, fp_in: u64, max: usize) -> Vec<u64> {
    use mach2::vm::mach_vm_read_overwrite;
    use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

    const SWIFT_ASYNC_FP_BIT: u64 = 1 << 60;

    let mut frames: Vec<u64> = Vec::with_capacity(max);
    let mut fp = fp_in & !SWIFT_ASYNC_FP_BIT;

    while fp != 0 && frames.len() < max {
        if (fp & 0x3) != 0 {
            break;
        }
        let mut buf = [0u8; 16];
        let mut out_size: mach_vm_size_t = 0;
        // SAFETY: out buffer lives for the duration of the call.
        let kr = unsafe {
            mach_vm_read_overwrite(
                task,
                fp as mach_vm_address_t,
                16,
                buf.as_mut_ptr() as mach_vm_address_t,
                &mut out_size,
            )
        };
        if kr != KERN_SUCCESS || out_size != 16 {
            break;
        }
        let prev_fp = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let ret = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        frames.push(pac_strip(ret));
        let next_fp = prev_fp & !SWIFT_ASYNC_FP_BIT;
        if next_fp <= fp {
            // Frame pointer must climb upward in arm64 ABI; if it
            // doesn't we're in malformed memory (likely JIT or a
            // smashed stack). Stop the walk to avoid looping.
            break;
        }
        fp = next_fp;
    }
    frames
}

/// How many frames at the deepest end of the two stacks match
/// after PAC stripping. "Deepest" = highest index. A common suffix
/// of length k means "the k oldest frames are identical." When
/// both stacks share their root frames (typical case for threads
/// in the same dispatch worker pool, etc.) but diverged at the
/// leaf, that k captures exactly how much of the parent context
/// is recoverable.
/// Resolve a Mach thread port to the kernel tid the kdebug stream
/// uses. `arg5` of every kperf record is the kernel-side
/// `thread_t::thread_id` (a globally-unique 64-bit identifier);
/// `THREAD_IDENTIFIER_INFO`'s `thread_id` field returns the same
/// value. mach2 doesn't expose this flavour, so the FFI is
/// declared inline.
fn thread_port_to_kernel_tid(port: mach_port_t) -> Option<u32> {
    // <mach/thread_info.h>: THREAD_IDENTIFIER_INFO == 4. The
    // struct is { uint64 thread_id; uint64 thread_handle;
    // uint8[32] dispatch_qaddr; }; first u64 is what we want.
    const THREAD_IDENTIFIER_INFO: i32 = 4;
    #[repr(C)]
    #[derive(Default)]
    struct IdInfo {
        thread_id: u64,
        thread_handle: u64,
        dispatch_qaddr: [u8; 32],
    }
    unsafe extern "C" {
        fn thread_info(
            target_thread: mach_port_t,
            flavor: i32,
            thread_info_out: *mut i32,
            thread_info_out_cnt: *mut mach_msg_type_number_t,
        ) -> i32;
    }

    let mut info = IdInfo::default();
    let mut count = (std::mem::size_of::<IdInfo>() / std::mem::size_of::<u32>())
        as mach_msg_type_number_t;
    // SAFETY: out-pointers + matching size; flavor matches struct.
    let kr = unsafe {
        thread_info(
            port,
            THREAD_IDENTIFIER_INFO,
            (&mut info) as *mut _ as *mut i32,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return None;
    }
    Some(info.thread_id as u32)
}

/// Strip ARM64e pointer-auth bits. macOS userland addresses fit
/// in 47 bits; PAC sits in the high 17. Mask conservatively so
/// PAC-bearing and PAC-stripped values of the same canonical
/// address compare equal.
#[inline]
pub fn pac_strip(addr: u64) -> u64 {
    addr & 0x0000_007F_FFFF_FFFF
}

#[inline]
pub fn mach_now() -> u64 {
    // SAFETY: leaf libc call.
    unsafe { mach2::mach_time::mach_absolute_time() }
}
