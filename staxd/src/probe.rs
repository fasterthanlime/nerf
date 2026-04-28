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
use tokio::sync::mpsc;
use tracing::{info, warn};

/// One sample-completed snapshot the drain loop hands to the
/// probe worker. Owns the kperf-walked user backtrace so the
/// worker can compare it against an FP-walked stack from the
/// suspended thread.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    pub tid: u32,
    /// Timestamp on the kdebug record that closed the sample
    /// (`PERF_GEN_EVENT | DBG_FUNC_END`), in mach ticks.
    pub kperf_ts_mach: u64,
    /// Mach ticks at the moment staxd's `read_trace` returned —
    /// drift from kperf_ts to this is the kernel→userspace pipe
    /// latency.
    pub drained_at_mach: u64,
    /// Full user backtrace as kperf observed it: index 0 is the
    /// leaf PC, indices 1..N-1 are FP-walked return addresses, the
    /// final index is the LR appended by `callstack_fixup_user`.
    /// Frames carry their raw (PAC-bearing) values; the worker
    /// strips before comparison.
    pub kperf_user_backtrace: Vec<u64>,
    /// Number of frames kperf collected for the kernel side. 0
    /// means the sample was either user-mode at PMI (kperf still
    /// emits a near-empty kstack via the trap entry) or the walk
    /// failed. Lets us split the rate by "interrupt-from-user" vs
    /// "interrupt-from-kernel" downstream.
    pub kperf_kstack_depth: u32,
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
    /// Mach absolute-time numer/denom for converting ticks → ns
    /// when we write the log.
    timebase: TimebaseInfo,
    stats: ProbeStats,
}

struct ProbeStats {
    probes_attempted: u64,
    /// Probes that returned KERN_SUCCESS for both
    /// `thread_get_state` and `thread_resume`.
    probes_ok: u64,
    /// PC matched (after PAC strip).
    pc_match: u64,
    /// LR matched (after PAC strip).
    lr_match: u64,
    /// Per-bucket sample counts split by interrupt-from-kernel
    /// (kperf_kstack_depth > some threshold) vs from-user. The
    /// "interesting" stitch is the kernel bucket.
    samples_kstack: u64,
    samples_no_kstack: u64,
    /// Distribution of "longest common suffix" length between
    /// kperf's user backtrace and our FP-walked stack from the
    /// suspended state. Index N = "exactly N frames matched
    /// counting from the deepest frame inward". Tells us how much
    /// of the *parent* call chain survives the drift even when the
    /// leaf PC has moved.
    common_suffix_len_hist: [u64; 33],
    /// Number of probes where the FP walk yielded zero frames
    /// (target memory unreadable, FP=0, PAC-stripped FP misaligned).
    fp_walk_failed: u64,
    /// Number of side-by-side comparison rows we've already logged.
    /// Capped (see `MAX_COMPARISON_ROWS`) so a long session doesn't
    /// flood oslog with full-stack dumps.
    comparison_rows_emitted: u64,
}

impl Default for ProbeStats {
    fn default() -> Self {
        Self {
            probes_attempted: 0,
            probes_ok: 0,
            pc_match: 0,
            lr_match: 0,
            samples_kstack: 0,
            samples_no_kstack: 0,
            common_suffix_len_hist: [0; 33],
            fp_walk_failed: 0,
            comparison_rows_emitted: 0,
        }
    }
}

/// Cap on per-session full-stack comparison rows. Aggregates still
/// cover every sample; this only limits the verbose side-by-side
/// dump to oslog.
const MAX_COMPARISON_ROWS: u64 = 32;
/// Cap on FP-walk depth. kperf's MAX_CALLSTACK_FRAMES is similar.
const MAX_WALK_FRAMES: usize = 32;

#[derive(Copy, Clone, Default)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

impl TimebaseInfo {
    fn now() -> Self {
        // SAFETY: mach_timebase_info reads two u32s into the
        // out-pointer; safe with a zeroed local.
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        let _ = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        Self {
            numer: info.numer,
            denom: info.denom,
        }
    }

    fn ticks_to_ns(&self, ticks: i128) -> i128 {
        if self.denom == 0 {
            ticks
        } else {
            ticks * (self.numer as i128) / (self.denom as i128)
        }
    }
}

/// Spawn a probe worker bound to `target_pid`. Returns a sender
/// the drain loop can shove `ProbeRequest`s into. Channel is
/// bounded so a stuck worker doesn't unbound-grow memory; the
/// drain loop uses `try_send` and counts drops.
pub fn spawn(target_pid: u32) -> Option<mpsc::Sender<ProbeRequest>> {
    let task = match task_for_pid_root(target_pid) {
        Ok(t) => t,
        Err(kr) => {
            warn!(target_pid, kr, "probe: task_for_pid failed; skipping probe");
            return None;
        }
    };

    info!(target_pid, "probe: race-against-return logging started (per-probe events under staxd::probe target)");

    let (tx, mut rx) = mpsc::channel::<ProbeRequest>(4096);

    let mut worker = ProbeWorker {
        target_pid,
        task,
        tid_cache: HashMap::new(),
        timebase: TimebaseInfo::now(),
        stats: ProbeStats::default(),
    };

    tokio::spawn(async move {
        // Refresh the tid cache periodically — threads come and
        // go in any non-trivial app. Cheap, takes microseconds.
        let mut refresh = tokio::time::interval(Duration::from_secs(1));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                req = rx.recv() => match req {
                    Some(r) => worker.probe_one(r),
                    None => break,
                },
                _ = refresh.tick() => worker.refresh_tid_cache(),
            }
        }

        worker.flush_and_log_summary();
    });

    Some(tx)
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
        if req.kperf_kstack_depth > 0 {
            self.stats.samples_kstack += 1;
        } else {
            self.stats.samples_no_kstack += 1;
        }

        // Cache miss → refresh once. Avoids paying task_threads
        // every probe but tolerates churning thread sets.
        if !self.tid_cache.contains_key(&req.tid) {
            self.refresh_tid_cache();
        }
        let port = match self.tid_cache.get(&req.tid).copied() {
            Some(p) => p,
            None => {
                self.write_row(&req, ProbeOutcome::CacheMiss, &[]);
                return;
            }
        };

        // SAFETY: port came from task_threads; valid until task exit.
        let kr_susp = unsafe { thread_suspend(port) };
        if kr_susp != KERN_SUCCESS {
            self.tid_cache.remove(&req.tid);
            self.write_row(&req, ProbeOutcome::SuspendFailed { kr: kr_susp }, &[]);
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

        // Walk the FP chain *before* resume, while the thread is
        // still frozen — guarantees a consistent stack image. The
        // walk reads target memory with mach_vm_read_overwrite, so
        // the thread doesn't need to be running for it to work.
        let mach_walked: Vec<u64> = if kr_get == KERN_SUCCESS {
            fp_walk(self.task, state.__fp, MAX_WALK_FRAMES)
        } else {
            Vec::new()
        };

        let probe_done_mach = mach_now();

        // SAFETY: paired resume.
        let _ = unsafe { thread_resume(port) };

        if kr_get != KERN_SUCCESS {
            self.write_row(&req, ProbeOutcome::GetStateFailed { kr: kr_get }, &[]);
            return;
        }

        if mach_walked.is_empty() && state.__fp != 0 {
            self.stats.fp_walk_failed += 1;
        }

        // Compare against the FP-walked portion of kperf's user
        // backtrace. The kperf record is structured as
        // [leaf_pc, walked_ret0, walked_ret1, ..., appended_lr].
        // Strip leaf and appended-LR before comparing.
        let kperf_walked: Vec<u64> = match req.kperf_user_backtrace.len() {
            0 | 1 => Vec::new(),
            n => req.kperf_user_backtrace[1..n - 1]
                .iter()
                .copied()
                .map(pac_strip)
                .collect(),
        };
        // mach_walked already PAC-stripped inside fp_walk.

        let common_suffix = longest_common_suffix(&kperf_walked, &mach_walked);
        let bucket = common_suffix.min(self.stats.common_suffix_len_hist.len() - 1);
        self.stats.common_suffix_len_hist[bucket] += 1;

        let mach_pc = pac_strip(state.__pc);
        let mach_lr = pac_strip(state.__lr);
        let kperf_pc = req
            .kperf_user_backtrace
            .first()
            .copied()
            .map(pac_strip)
            .unwrap_or(0);
        let kperf_lr = req
            .kperf_user_backtrace
            .last()
            .copied()
            .map(pac_strip)
            .unwrap_or(0);

        let pc_match = mach_pc == kperf_pc;
        let lr_match = mach_lr == kperf_lr;

        self.stats.probes_ok += 1;
        if pc_match {
            self.stats.pc_match += 1;
        }
        if lr_match {
            self.stats.lr_match += 1;
        }

        // First N samples per session: emit a verbose side-by-side
        // dump so a human can eyeball what's actually happening.
        if self.stats.comparison_rows_emitted < MAX_COMPARISON_ROWS {
            self.stats.comparison_rows_emitted += 1;
            self.emit_comparison_row(
                &req,
                mach_pc,
                mach_lr,
                state.__fp,
                &mach_walked,
                common_suffix,
            );
        }

        self.write_row(
            &req,
            ProbeOutcome::Ok {
                probe_done_mach,
                mach_pc,
                mach_lr,
                pc_match,
                lr_match,
                common_suffix,
                mach_walked_depth: mach_walked.len() as u32,
            },
            &mach_walked,
        );
    }

    /// Verbose dump of one probe: kperf's full user_backtrace and
    /// our FP-walked stack from the suspended thread, side by
    /// side. Capped per session by `MAX_COMPARISON_ROWS`.
    fn emit_comparison_row(
        &self,
        req: &ProbeRequest,
        mach_pc: u64,
        mach_lr: u64,
        mach_fp: u64,
        mach_walked: &[u64],
        common_suffix: usize,
    ) {
        let kperf_str = stack_to_hex(&req.kperf_user_backtrace, true);
        let mach_walked_str = stack_to_hex(mach_walked, false);
        tracing::info!(
            target: "staxd::probe",
            tid = req.tid,
            kperf_ts_mach = req.kperf_ts_mach,
            kperf_stack = %kperf_str,
            mach_pc = format!("{mach_pc:#x}"),
            mach_fp = format!("{mach_fp:#x}"),
            mach_lr = format!("{mach_lr:#x}"),
            mach_walked = %mach_walked_str,
            common_suffix,
            kperf_walked_depth = req.kperf_user_backtrace.len().saturating_sub(2),
            mach_walked_depth = mach_walked.len(),
            "probe: stack comparison"
        );
    }

    fn write_row(&mut self, req: &ProbeRequest, out: ProbeOutcome, _mach_walked: &[u64]) {
        let drained_drift_ticks = (req.drained_at_mach as i128) - (req.kperf_ts_mach as i128);
        let drained_drift_ns = self.timebase.ticks_to_ns(drained_drift_ticks) as i64;
        let probe_drift_ns: i64 = match out {
            ProbeOutcome::Ok { probe_done_mach, .. } => {
                let d = (probe_done_mach as i128) - (req.kperf_ts_mach as i128);
                self.timebase.ticks_to_ns(d) as i64
            }
            _ => 0,
        };

        let (kind, mach_pc, mach_lr, pc_match, lr_match, common_suffix, mach_walked_depth, kr) =
            match out {
                ProbeOutcome::Ok {
                    mach_pc: pc,
                    mach_lr: lr,
                    pc_match: pcm,
                    lr_match: lrm,
                    common_suffix: cs,
                    mach_walked_depth: mwd,
                    ..
                } => ("ok", pc, lr, pcm, lrm, cs, mwd, 0),
                ProbeOutcome::CacheMiss => ("cache_miss", 0, 0, false, false, 0, 0, 0),
                ProbeOutcome::SuspendFailed { kr } => {
                    ("suspend_failed", 0, 0, false, false, 0, 0, kr)
                }
                ProbeOutcome::GetStateFailed { kr } => {
                    ("get_state_failed", 0, 0, false, false, 0, 0, kr)
                }
            };

        let kperf_walked_depth = req.kperf_user_backtrace.len().saturating_sub(2) as u32;

        tracing::info!(
            target: "staxd::probe",
            tid = req.tid,
            kperf_ts_mach = req.kperf_ts_mach,
            drained_drift_ns,
            probe_drift_ns,
            mach_pc,
            mach_lr,
            pc_match,
            lr_match,
            common_suffix,
            kperf_walked_depth,
            mach_walked_depth,
            kstack_depth = req.kperf_kstack_depth,
            ustack_depth = req.kperf_user_backtrace.len() as u32,
            kind,
            kr,
        );
    }

    fn flush_and_log_summary(&mut self) {
        let total = self.stats.probes_attempted.max(1);
        let ok = self.stats.probes_ok;
        let pc = self.stats.pc_match;
        let lr = self.stats.lr_match;

        // Render the common-suffix histogram as one comma-separated
        // string ("0:1234,1:567,2:111,...") so the analyzer can
        // pick it apart with awk.
        let mut hist_parts = Vec::new();
        for (i, n) in self.stats.common_suffix_len_hist.iter().enumerate() {
            if *n > 0 {
                hist_parts.push(format!("{i}:{n}"));
            }
        }
        let hist = hist_parts.join(",");

        info!(
            target_pid = self.target_pid,
            attempted = total,
            ok,
            ok_rate_pct = (ok * 100) / total,
            pc_match = pc,
            pc_match_rate_pct = (pc * 100) / total,
            lr_match = lr,
            samples_kstack = self.stats.samples_kstack,
            samples_no_kstack = self.stats.samples_no_kstack,
            fp_walk_failed = self.stats.fp_walk_failed,
            common_suffix_hist = hist,
            "probe: race-against-return summary"
        );
    }
}

enum ProbeOutcome {
    Ok {
        probe_done_mach: u64,
        mach_pc: u64,
        mach_lr: u64,
        pc_match: bool,
        lr_match: bool,
        common_suffix: usize,
        mach_walked_depth: u32,
    },
    CacheMiss,
    SuspendFailed { kr: i32 },
    GetStateFailed { kr: i32 },
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
fn longest_common_suffix(a: &[u64], b: &[u64]) -> usize {
    let mut k = 0;
    let max_k = a.len().min(b.len());
    while k < max_k {
        if a[a.len() - 1 - k] != b[b.len() - 1 - k] {
            break;
        }
        k += 1;
    }
    k
}

/// Render a stack of u64 addresses as `[0x123,0x456,...]`. Pre-PAC
/// when `raw=true` (so the caller sees what kperf actually emitted).
fn stack_to_hex(frames: &[u64], raw: bool) -> String {
    let mut s = String::with_capacity(frames.len() * 14 + 2);
    s.push('[');
    for (i, f) in frames.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let v = if raw { *f } else { pac_strip(*f) };
        use std::fmt::Write;
        let _ = write!(s, "{v:#x}");
    }
    s.push(']');
    s
}

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
