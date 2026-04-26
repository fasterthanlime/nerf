//! Recording driver. Configures kperf's PET (Profile Every Thread)
//! mode for a target PID, enables the kdebug ringbuffer, drains it
//! on a schedule, and emits parsed samples to a [`SampleSink`].
//!
//! On drop, the [`Session`] guard tears down kperf + kdebug state in
//! the same order as mperf's `profiling_cleanup` so the host kernel
//! is left in a clean state even on panic.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use nerf_mac_capture::recorder::ThreadNameCache;
use nerf_mac_capture::{JitdumpEvent, SampleEvent, SampleSink, ThreadNameEvent};

use crate::bindings::{self, sampler, Frameworks};
use crate::error::Error;
use crate::image_scan::ImageScanner;
use crate::kdebug::{self, kdbg_class, kdbg_code, kdbg_func, kdbg_subclass, KdBuf, KdRegtype, DBG_PERF};
use crate::kernel_symbols::{KernelImage, SlideEstimator};
use crate::libproc;
use crate::offcpu::OffCpuTracker;
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

    // No `task_for_pid` here on purpose: AMFI denies it from root
    // against a privilege-dropped child on Apple Silicon, and we
    // don't actually need a Mach task port. libproc gives us the
    // image regions and thread names by PID, with read-permission
    // gating instead of task-port policy.
    let mut images = ImageScanner::new();
    let mut thread_names = ThreadNameCache::new();

    let t0 = Instant::now();
    images.rescan(opts.pid, sink);
    log::info!("initial image scan took {:?}", t0.elapsed());

    // Load the on-disk kernel binary. We can't get the KASLR slide
    // without an Apple-private entitlement, so we feed kernel frame
    // addresses observed during sampling into a constraint-based
    // estimator (see kernel_symbols::SlideEstimator) and emit
    // `/proc/kallsyms` with the derived slide at the end.
    let kernel_image = match KernelImage::load() {
        Ok(img) => img,
        Err(err) => {
            log::warn!("kernel image load failed: {err:?}");
            None
        }
    };
    let mut slide_est = kernel_image
        .as_ref()
        .map(|img| SlideEstimator::new(img.exec_segments.clone()));

    scan_thread_names(opts.pid, sink, &mut thread_names);

    let t0 = Instant::now();
    let mut session = Session::start(&fw, &opts)?;
    session.enable_kdebug(&opts)?;
    log::info!("kperf+kdebug arming took {:?}", t0.elapsed());

    drain_loop(
        &fw,
        &opts,
        sink,
        &mut images,
        &mut thread_names,
        slide_est.as_mut(),
        &mut should_stop,
    )?;

    drop(session);

    // Recording is over; finalize the slide and emit kallsyms.
    if let (Some(image), Some(est)) = (kernel_image, slide_est) {
        match est.finalize() {
            Some((slide, support)) => {
                log::info!(
                    "kernel slide derived: {slide:#x} (support {:.1}% \
                     over {} sampled kernel addresses)",
                    support * 100.0,
                    est.observed_count(),
                );
                let kallsyms = image.format_kallsyms(slide);
                sink.on_kallsyms(&kallsyms);
            }
            None => log::warn!(
                "kernel slide estimator collected no votes; skipping kallsyms"
            ),
        }
    }
    Ok(())
}

/// Enumerate threads via libproc and emit a `ThreadNameEvent` for
/// each (tid, name) binding the cache hasn't seen yet. No
/// task_for_pid -- `proc_pidinfo` works under read permission alone.
fn scan_thread_names<S: SampleSink>(
    pid: u32,
    sink: &mut S,
    cache: &mut ThreadNameCache,
) {
    let tids = match libproc::list_thread_ids(pid) {
        Ok(t) => t,
        Err(err) => {
            log::debug!("libproc::list_thread_ids(pid={pid}) failed: {err}");
            return;
        }
    };
    let mut named = 0u32;
    let mut nameless = 0u32;
    for tid64 in tids {
        // Truncating to u32: nerf's archive format keeps tids as u32
        // and macOS thread ids practically never overflow that.
        let tid = tid64 as u32;
        match libproc::thread_name(tid64) {
            Ok(Some(name)) => {
                named += 1;
                if cache.note_thread(tid, &name) {
                    sink.on_thread_name(ThreadNameEvent { pid, tid, name: &name });
                }
            }
            Ok(None) => nameless += 1,
            Err(err) => log::trace!("libproc::thread_name({tid64}) failed: {err}"),
        }
    }
    log::debug!(
        "scan_thread_names: pid={pid} named={named} nameless={nameless}"
    );
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

        // Range filter covers DBG_MACH (class 1, where MACH_SCHED
        // context-switch events live) through DBG_PERF (class 37,
        // where kperf samples live). The filter is single-range so
        // we sweep up everything in between (DBG_NETWORK, DBG_BSD,
        // ...); the drain loop drops anything that isn't DBG_PERF
        // or DBG_MACH_SCHED before parsing. In practice the kdebug
        // ring buffer (1M records) holds several seconds of traffic
        // even on busy systems, and we drain every few ms.
        let mut filter = KdRegtype {
            ty: kdebug::KDBG_RANGETYPE,
            value1: kdebug::kdbg_eventid(kdebug::DBG_MACH, kdebug::DBG_MACH_SCHED, 0),
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

#[allow(clippy::too_many_arguments)]
fn drain_loop<S: SampleSink>(
    _fw: &Frameworks,
    opts: &RecordOptions,
    sink: &mut S,
    images: &mut ImageScanner,
    thread_names: &mut ThreadNameCache,
    mut slide_est: Option<&mut SlideEstimator>,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    let start = Instant::now();
    let drain_period = Duration::from_micros(
        ((1_000_000 / opts.frequency_hz.max(1)) * 2).into(),
    );

    // Re-scan loaded images a few times per second. libproc walks
    // every region every time (no kernel-side change-counter to
    // short-circuit on like dyld_all_image_infos has), so this is the
    // dominant cost outside of sample drain.
    let image_period = Duration::from_millis(250);
    let mut next_image = Instant::now() + image_period;
    // Thread-name scan is cheap (one PROC_PIDLISTTHREADS + one
    // PROC_PIDTHREADINFO per thread); we run it often so short-lived
    // TaskGroup-style worker threads get a name before they die.
    // ~50ms is empirically a good balance.
    let thread_period = Duration::from_millis(50);
    let mut next_thread = Instant::now() + thread_period;

    // Poll for the JIT-runtime convention: V8/Node, Cranelift's
    // `--jitdump` output, perf-map-agent, and friends all drop a
    // `jit-<pid>.dump` (or open one with that basename via $TMPDIR).
    // We probe `/tmp` -- the most common location -- and stop once
    // it shows up so we don't stat() forever in the steady state.
    let jitdump_path = std::path::PathBuf::from(format!("/tmp/jit-{}.dump", opts.pid));
    let jitdump_period = Duration::from_millis(500);
    let mut next_jitdump = Instant::now() + jitdump_period;
    let mut jitdump_emitted = false;

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
    let mut offcpu = OffCpuTracker::new();

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

        if Instant::now() >= next_image {
            images.rescan(opts.pid, sink);
            next_image = Instant::now() + image_period;
        }
        if Instant::now() >= next_thread {
            scan_thread_names(opts.pid, sink, thread_names);
            next_thread = Instant::now() + thread_period;
        }
        if !jitdump_emitted && Instant::now() >= next_jitdump {
            if jitdump_path.exists() {
                sink.on_jitdump(JitdumpEvent {
                    pid: opts.pid,
                    path: &jitdump_path,
                });
                jitdump_emitted = true;
            } else {
                next_jitdump = Instant::now() + jitdump_period;
            }
        }

        let n = kdebug::read_trace(&mut buf)?;
        if n == 0 {
            continue;
        }
        total_drained += n as u64;

        for rec in &buf[..n] {
            let class = kdbg_class(rec.debugid);
            if class == DBG_PERF {
                let key = (
                    kdbg_subclass(rec.debugid),
                    kdbg_code(rec.debugid),
                    kdbg_func(rec.debugid),
                );
                *histogram.entry(key).or_insert(0) += 1;
            } else if class == kdebug::DBG_MACH
                && kdbg_subclass(rec.debugid) == kdebug::DBG_MACH_SCHED
            {
                offcpu.feed(rec);
                continue;
            }
            parser.feed(rec, |sample| {
                if let Some(ref mut est) = slide_est {
                    // The deepest kernel frame (last in callee-most-first
                    // order) is the most stable point of entry, but any
                    // kernel-text PC works as a constraint.
                    for &avma in sample.kernel_backtrace {
                        est.observe(avma);
                    }
                }
                offcpu.note_sample(
                    sample.tid,
                    sample.user_backtrace,
                    sample.kernel_backtrace,
                );
                sink.on_sample(SampleEvent {
                    timestamp_ns: sample.timestamp_ns,
                    pid: opts.pid,
                    tid: sample.tid,
                    backtrace: sample.user_backtrace,
                    kernel_backtrace: sample.kernel_backtrace,
                });
            });
        }

        // Expand any off-CPU intervals that closed in this batch
        // into synthetic wall-clock samples spaced at the PET sample
        // period. The stack is frozen from the last on-CPU sample
        // for the thread, so a thread blocked deep inside
        // `mach_msg_overwrite_trap` lights up that frame for the
        // entire blocked interval rather than disappearing from the
        // flame graph the way it would in a CPU-only profiler.
        let period_ns = (1_000_000_000u64 / opts.frequency_hz.max(1) as u64).max(1);
        for interval in offcpu.drain_pending() {
            let mut ts = interval.off_ns;
            while ts < interval.on_ns {
                sink.on_sample(SampleEvent {
                    timestamp_ns: ts,
                    pid: opts.pid,
                    tid: interval.tid,
                    backtrace: &interval.user_stack,
                    kernel_backtrace: &interval.kernel_stack,
                });
                ts = ts.saturating_add(period_ns);
            }
        }
    }

    log_session_summary(total_drained, &parser, &histogram, &offcpu);
    Ok(())
}

fn log_session_summary(
    total: u64,
    parser: &Parser,
    histogram: &BTreeMap<(u8, u16, u32), u64>,
    offcpu: &OffCpuTracker,
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
    offcpu.log_summary();
}

fn kperf_call(rc: i32, op: &'static str) -> Result<(), Error> {
    if rc != 0 {
        return Err(Error::Kperf { op, code: rc });
    }
    Ok(())
}
