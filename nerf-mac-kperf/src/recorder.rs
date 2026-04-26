//! Recording driver. Configures kperf's PET (Profile Every Thread)
//! mode for a target PID, enables the kdebug ringbuffer, drains it
//! on a schedule, and emits parsed samples to a [`SampleSink`].
//!
//! On drop, the [`Session`] guard tears down kperf + kdebug state in
//! the same order as mperf's `profiling_cleanup` so the host kernel
//! is left in a clean state even on panic.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use mach2::kern_return::KERN_SUCCESS;
use mach2::port::{mach_port_t, MACH_PORT_NULL};
use mach2::traps::{mach_task_self, task_for_pid};

use nerf_mac_capture::proc_maps::{DyldInfo, DyldInfoManager, Modification};
use nerf_mac_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, SampleEvent, SampleSink,
};

use crate::bindings::{self, sampler, Frameworks};
use crate::error::Error;
use crate::kdebug::{self, kdbg_class, kdbg_code, kdbg_func, kdbg_subclass, KdBuf, KdRegtype, DBG_PERF};
use crate::parser::Parser;

/// Configuration for a kperf-driven recording session.
pub struct RecordOptions {
    /// PID to attach to.
    pub pid: u32,
    /// Sampling frequency in Hz. Translated to a PET timer period.
    pub frequency_hz: u32,
    /// If `Some`, stop recording after this duration.
    pub duration: Option<Duration>,
    /// Number of records the kdebug ringbuffer is sized for. mperf
    /// uses 1_000_000; that's a few tens of MB and is fine for
    /// short-to-medium captures.
    pub kdebug_buf_records: i32,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            pid: 0,
            frequency_hz: 1000,
            duration: None,
            kdebug_buf_records: 1_000_000,
        }
    }
}

/// Sampler bitmask used for stack-only profiling (no PMC values).
/// `TH_INFO` lets us correlate a record back to a tid; `USTACK` /
/// `KSTACK` are the user/kernel callchains we actually want.
const STACK_SAMPLER_BITS: u32 =
    sampler::TH_INFO | sampler::USTACK | sampler::KSTACK;

/// Drive a recording session. Blocks until `should_stop` returns true,
/// the duration elapses, or an unrecoverable error occurs.
pub fn record<S: SampleSink>(
    opts: RecordOptions,
    sink: &mut S,
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), Error> {
    let fw = bindings::load()?;

    // The earliest, cheapest way to confirm we have root: this
    // sysctl is gated on the same privilege check as the rest of
    // the kpc surface.
    let mut force_ctrs: i32 = 0;
    let rc = unsafe { (fw.kpc_force_all_ctrs_get)(&mut force_ctrs) };
    if rc != 0 {
        return Err(Error::NotRoot);
    }

    // Wipe any stale kperf/ktrace state from a previous half-finished
    // run. Without this, `kdebug::reset()` below trips EINVAL when
    // ktrace is still owned by KTRACE_KPERF from a previous session.
    unsafe {
        let _ = (fw.kperf_sample_set)(0);
        let _ = (fw.kperf_reset)();
    }
    let _ = kdebug::set_lightweight_pet(0);
    let _ = kdebug::enable(false);
    let _ = kdebug::reset();

    // Acquire a Mach task port so we can scan the target's loaded
    // dyld images and emit BinaryLoadedEvents into the archive. The
    // kernel walks user stacks for us; we still need to tell the
    // analysis side which dylib each PC came from.
    let task = task_for_pid_existing(opts.pid)?;
    let mut dyld = DyldInfoManager::new(task);

    let t0 = Instant::now();
    apply_dyld_changes(&mut dyld, opts.pid, sink);
    log::info!("initial dyld scan took {:?}", t0.elapsed());

    let t0 = Instant::now();
    let mut session = Session::start(&fw, &opts)?;
    session.enable_kdebug(&opts)?;
    log::info!("kperf+kdebug arming took {:?}", t0.elapsed());

    drain_loop(&fw, &opts, sink, &mut dyld, &mut should_stop)?;

    drop(session);
    Ok(())
}

fn task_for_pid_existing(pid: u32) -> Result<mach_port_t, Error> {
    let mut task: mach_port_t = MACH_PORT_NULL;
    let kr = unsafe { task_for_pid(mach_task_self(), pid as i32, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(Error::Kperf {
            op: "task_for_pid",
            code: kr,
        });
    }
    Ok(task)
}

fn apply_dyld_changes<S: SampleSink>(
    dyld: &mut DyldInfoManager,
    pid: u32,
    sink: &mut S,
) {
    let changes = match dyld.check_for_changes() {
        Ok(c) => c,
        Err(err) => {
            log::debug!("DyldInfoManager::check_for_changes failed: {err:?}");
            return;
        }
    };
    for change in changes {
        match change {
            Modification::Added(lib) => emit_binary_loaded(pid, &lib, sink),
            Modification::Removed(lib) => sink.on_binary_unloaded(BinaryUnloadedEvent {
                pid,
                base_avma: lib.base_avma,
                path: &lib.file,
            }),
        }
    }
}

fn emit_binary_loaded<S: SampleSink>(pid: u32, lib: &DyldInfo, sink: &mut S) {
    sink.on_binary_loaded(BinaryLoadedEvent {
        pid,
        base_avma: lib.base_avma,
        vmsize: lib.vmsize,
        text_svma: lib.module_info.base_svma,
        path: &lib.file,
        uuid: lib.uuid,
        arch: lib.arch,
        is_executable: lib.is_executable,
        symbols: &lib.symbols,
    });
}

// ---------------------------------------------------------------------------
// Session: lifecycle guard for kperf + kdebug kernel state
// ---------------------------------------------------------------------------

struct Session<'a> {
    fw: &'a Frameworks,
    #[allow(dead_code)]
    actionid: u32,
    #[allow(dead_code)]
    timerid: u32,
}

impl<'a> Session<'a> {
    /// Configure kperf actions / timers / filter, then arm sampling.
    /// Order matches mperf's `run_with_pet` exactly: lightweight_pet
    /// is set, then `kperf_sample_set(1)`, *before* any kdebug op.
    /// In lightweight-PET mode kperf cooperates with the kdebug
    /// interface rather than taking exclusive ownership, so the
    /// subsequent `KERN_KDREMOVE` etc. are accepted.
    fn start(fw: &'a Frameworks, opts: &RecordOptions) -> Result<Self, Error> {
        // Allocate one action + one timer.
        let actionid: u32 = 1;
        let timerid: u32 = 1;

        kperf_call(unsafe { (fw.kperf_action_count_set)(bindings::KPERF_ACTION_MAX) }, "action_count_set")?;
        kperf_call(unsafe { (fw.kperf_timer_count_set)(bindings::KPERF_TIMER_MAX) }, "timer_count_set")?;

        // Stack samplers — kernel does the FP-walk for us.
        kperf_call(
            unsafe { (fw.kperf_action_samplers_set)(actionid, STACK_SAMPLER_BITS) },
            "action_samplers_set",
        )?;
        kperf_call(
            unsafe {
                (fw.kperf_action_filter_set_by_pid)(actionid, opts.pid as i32)
            },
            "action_filter_set_by_pid",
        )?;

        let period_ns = if opts.frequency_hz == 0 {
            1_000_000
        } else {
            1_000_000_000u64 / opts.frequency_hz as u64
        };
        let ticks = unsafe { (fw.kperf_ns_to_ticks)(period_ns) };
        kperf_call(
            unsafe { (fw.kperf_timer_period_set)(actionid, ticks) },
            "timer_period_set",
        )?;
        kperf_call(
            unsafe { (fw.kperf_timer_action_set)(actionid, timerid) },
            "timer_action_set",
        )?;
        kperf_call(unsafe { (fw.kperf_timer_pet_set)(timerid) }, "timer_pet_set")?;

        // Lightweight PET + sample_set must precede kdebug setup so
        // kdebug ops aren't blocked by an exclusive KTRACE_KPERF.
        kdebug::set_lightweight_pet(1)?;
        kperf_call(unsafe { (fw.kperf_sample_set)(1) }, "sample_set")?;

        Ok(Self { fw, actionid, timerid })
    }

    fn enable_kdebug(&mut self, opts: &RecordOptions) -> Result<(), Error> {
        kdebug::reset()?;
        kdebug::set_buf_size(opts.kdebug_buf_records)?;
        kdebug::setup()?;

        // Range-filter to the DBG_PERF class so we don't drown in
        // unrelated kernel events.
        let mut filter = KdRegtype {
            ty: kdebug::KDBG_RANGETYPE,
            value1: kdebug::kdbg_eventid(kdebug::DBG_PERF, 0, 0),
            value2: kdebug::kdbg_eventid(kdebug::DBG_PERF, 0xff, 0x3fff),
            value3: 0,
            value4: 0,
        };
        kdebug::set_filter(&mut filter)?;
        kdebug::enable(true)?;
        Ok(())
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // Same order as mperf's profiling_cleanup. Errors are
        // logged, not propagated — we want the rest of the cleanup
        // to run even if one step fails.
        let _ = kdebug::enable(false);
        let _ = kdebug::reset();
        unsafe {
            let _ = (self.fw.kperf_sample_set)(0);
        }
        let _ = kdebug::set_lightweight_pet(0);
        unsafe {
            let _ = (self.fw.kpc_set_counting)(0);
            let _ = (self.fw.kpc_set_thread_counting)(0);
            let _ = (self.fw.kpc_force_all_ctrs_set)(0);
            let _ = (self.fw.kperf_reset)();
        }
    }
}

// ---------------------------------------------------------------------------
// Drain loop
// ---------------------------------------------------------------------------

fn drain_loop<S: SampleSink>(
    _fw: &Frameworks,
    opts: &RecordOptions,
    sink: &mut S,
    dyld: &mut DyldInfoManager,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    let start = Instant::now();
    let drain_period = Duration::from_micros(
        ((1_000_000 / opts.frequency_hz.max(1)) * 2).into(),
    );

    // Re-scan dyld at most a few times per second; the timestamp
    // check inside DyldInfoManager already short-circuits when the
    // image table hasn't moved.
    let dyld_period = Duration::from_millis(250);
    let mut next_dyld = Instant::now() + dyld_period;

    let mut buf: Vec<KdBuf> = vec![
        KdBuf {
            timestamp: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            debugid: 0,
            cpuid: 0,
            unused: 0,
        };
        opts.kdebug_buf_records as usize
    ];

    let mut parser = Parser::new();
    // (subclass, code, func) -> count, for diagnostics.
    let mut histogram: BTreeMap<(u8, u16, u32), u64> = BTreeMap::new();
    let mut total_drained: u64 = 0;

    loop {
        if should_stop() {
            break;
        }
        if let Some(d) = opts.duration {
            if start.elapsed() >= d {
                break;
            }
        }

        std::thread::sleep(drain_period);

        if Instant::now() >= next_dyld {
            apply_dyld_changes(dyld, opts.pid, sink);
            next_dyld = Instant::now() + dyld_period;
        }

        let n = kdebug::read_trace(&mut buf)?;
        if n == 0 {
            continue;
        }
        total_drained += n as u64;

        for rec in &buf[..n] {
            if kdbg_class(rec.debugid) == DBG_PERF {
                let key = (
                    kdbg_subclass(rec.debugid),
                    kdbg_code(rec.debugid),
                    kdbg_func(rec.debugid),
                );
                *histogram.entry(key).or_insert(0) += 1;
            }
            parser.feed(rec, |sample| {
                let backtrace: Vec<u64> =
                    sample.user_backtrace.iter().copied().collect();
                sink.on_sample(SampleEvent {
                    timestamp_ns: sample.timestamp_ns,
                    pid: opts.pid,
                    tid: sample.tid,
                    backtrace: &backtrace,
                });
            });
        }
    }

    log_session_summary(total_drained, &parser, &histogram);
    Ok(())
}

fn log_session_summary(
    total: u64,
    parser: &Parser,
    histogram: &BTreeMap<(u8, u16, u32), u64>,
) {
    let s = &parser.stats;
    log::info!(
        "kdebug records drained: {total}, samples \
         started/emitted/orphaned: {}/{}/{}, walk errors u/k: {}/{}",
        s.samples_started,
        s.samples_emitted,
        s.samples_orphaned,
        s.user_walk_errors,
        s.kernel_walk_errors,
    );
    log::info!("DBG_PERF histogram (subclass, code, func) -> count:");
    for ((sc, code, func), count) in histogram {
        log::info!("  ({sc:>2}, {code:>3}, {func}) -> {count}");
    }
}

fn kperf_call(rc: i32, op: &'static str) -> Result<(), Error> {
    if rc != 0 {
        return Err(Error::Kperf { op, code: rc });
    }
    Ok(())
}
