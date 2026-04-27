//! Recording driver. Configures kperf's PET (Profile Every Thread)
//! mode for a target PID, enables the kdebug ringbuffer, drains it
//! on a schedule, and emits parsed samples to a [`SampleSink`].
//!
//! On drop, the [`Session`] guard tears down kperf + kdebug state in
//! the same order as mperf's `profiling_cleanup` so the host kernel
//! is left in a clean state even on panic.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use nerf_mac_capture::proc_maps::MachOSymbol;
use nerf_mac_capture::recorder::ThreadNameCache;
use nerf_mac_capture::{
    BinaryLoadedEvent, JitdumpEvent, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};

use nerf_mac_kperf_parse::image_scan::ImageScanner;
use nerf_mac_kperf_parse::jitdump_tail::JitdumpTailer;
use nerf_mac_kperf_parse::kernel_symbols::{KernelImage, SlideEstimator};
use nerf_mac_kperf_parse::libproc;
use nerf_mac_kperf_parse::offcpu::CpuIntervalTracker;
use nerf_mac_kperf_parse::parser::Parser;
use nerf_mac_kperf_sys::bindings::{self, sampler, Frameworks};
use nerf_mac_kperf_sys::error::Error;
use nerf_mac_kperf_sys::kdebug::{self, kdbg_class, kdbg_code, kdbg_func, kdbg_subclass, KdBuf, KdRegtype, DBG_PERF};
use nerf_mac_kperf_sys::pmu_events::{self, ConfiguredPmu, PmuSlot};

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

/// Sampler bitmask. `TH_INFO` lets us correlate a record back to a
/// tid; `USTACK`/`KSTACK` are the user/kernel callchains; `PMC_THREAD`
/// asks kperf to read per-thread CPU performance counters at each
/// PET tick (cycles + instructions retired on Apple Silicon's fixed
/// counters).
const STACK_SAMPLER_BITS: u32 =
    sampler::TH_INFO | sampler::USTACK | sampler::KSTACK | sampler::PMC_THREAD;

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
    //
    // Open the shared cache once and share it via `Arc` -- the
    // image scanner needs it for symbol enumeration, and the live
    // sink needs the same parsed cache as a `MachOByteSource` so
    // the binary registry can disassemble system code without
    // re-parsing. Single parse, two consumers.
    let shared_cache: Option<std::sync::Arc<nperf_mac_shared_cache::SharedCache>> =
        nperf_mac_shared_cache::SharedCache::for_host().map(std::sync::Arc::new);
    if let Some(sc) = shared_cache.clone() {
        sink.on_macho_byte_source(sc);
    }
    let mut images = ImageScanner::new(shared_cache);
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

    // Configure additional PMU events (cache misses, branch
    // mispredicts) before session start so kpc_set_config sees the
    // configurable counter requests. Falls back gracefully to the
    // FIXED-only path if the lookups don't resolve on this chip.
    let configured_pmu = pmu_events::configure(&fw);

    let t0 = Instant::now();
    let mut session = Session::start(&fw, &opts, configured_pmu.as_ref())?;
    session.enable_kdebug(&opts)?;
    session.arm()?;
    log::info!("kperf+kdebug arming took {:?}", t0.elapsed());

    drain_loop(
        &fw,
        &opts,
        sink,
        &mut images,
        &mut thread_names,
        slide_est.as_mut(),
        configured_pmu.as_ref(),
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
            log::warn!("libproc::list_thread_ids(pid={pid}) failed: {err}");
            return;
        }
    };
    let mut named = 0u32;
    let mut nameless = 0u32;
    let mut errored = 0u32;
    for tid64 in tids {
        // Truncating to u32: nerf's archive format keeps tids as u32
        // and macOS thread ids practically never overflow that.
        let tid = tid64 as u32;
        match libproc::thread_name(pid, tid64) {
            Ok(Some(name)) => {
                named += 1;
                if cache.note_thread(tid, &name) {
                    sink.on_thread_name(ThreadNameEvent { pid, tid, name: &name });
                }
            }
            Ok(None) => nameless += 1,
            Err(err) => {
                errored += 1;
                log::debug!("libproc::thread_name({tid64}) failed: {err}");
            }
        }
    }
    if errored > 0 {
        log::warn!(
            "scan_thread_names: pid={pid} named={named} nameless={nameless} errored={errored} \
             -- non-zero errored count usually means proc_pidinfo(PROC_PIDTHREADINFO) is being \
             denied; the thread switcher will only show [tid] labels for those threads"
        );
    } else {
        log::debug!(
            "scan_thread_names: pid={pid} named={named} nameless={nameless}"
        );
    }
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
    /// Configure kperf actions / timers / filter. Does NOT call
    /// `kperf_sample_set(1)` -- that's deferred to `arm()` so we can
    /// finish kdebug setup first. Once `kperf_sample_set(1)` runs,
    /// kperf takes exclusive ownership of ktrace and reset/set_buf_size
    /// would fail; doing kdebug init first sidesteps that. We also
    /// leave `kperf.lightweight_pet=0` (the post-cleanup default), so
    /// PET walks user/kernel callstacks on every tick instead of just
    /// when its rate-limiter happens to fire (lightweight=1 is for
    /// counter-stat tools like mperf, not profilers).
    fn start(
        fw: &'a Frameworks,
        opts: &RecordOptions,
        configured_pmu: Option<&ConfiguredPmu>,
    ) -> Result<Self, Error> {
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

        // Enable PMU counter classes. Apple Silicon's FIXED class
        // exposes cycles + instructions retired with no per-event
        // config; the CONFIGURABLE class lets us program ~8 counters
        // for events like L1D misses or branch mispredicts. We always
        // turn FIXED on; if `configured_pmu` resolved a configurable
        // event we extend the class mask + push the event encodings
        // via kpc_set_config.
        let class_mask = configured_pmu
            .map(|c| c.class_mask)
            .unwrap_or(bindings::KPC_CLASS_FIXED_MASK);
        if let Some(c) = configured_pmu {
            // `kpc_set_config` writes an array of u64 event
            // configs into the kernel; the FIXED class needs zero
            // entries (it's pre-determined), so the array length we
            // pass is whatever the kpep_config built.
            let mut configs = c.configs.clone();
            kperf_call(
                unsafe { (fw.kpc_set_config)(class_mask, configs.as_mut_ptr()) },
                "kpc_set_config(FIXED+CONFIGURABLE)",
            )?;
        }
        kperf_call(
            unsafe { (fw.kpc_set_counting)(class_mask) },
            "kpc_set_counting",
        )?;
        kperf_call(
            unsafe { (fw.kpc_set_thread_counting)(class_mask) },
            "kpc_set_thread_counting",
        )?;

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

    /// Arm kperf sampling. Must be called *after* `enable_kdebug` --
    /// `kperf_sample_set(1)` takes exclusive ownership of the ktrace
    /// subsystem, after which `kdebug::reset` and friends would EBUSY.
    /// The exclusive lock doesn't block reads (`KERN_KDREADTR`), so the
    /// drain loop keeps working.
    ///
    /// `lightweight_pet=1` is essential: in that mode PET samples only
    /// threads that are *actually running on a CPU at the moment of
    /// the tick*. Without it (lightweight_pet=0, the "heavy PET"
    /// path), kperf walks every thread in the target -- including
    /// parked ones -- and emits a sample with the thread's frozen
    /// last user PC. Those parked-thread samples sit in the syscall
    /// stub of whatever made the thread block (`__psynch_cvwait`,
    /// `mach_msg2_trap`, ...). When we then weight every sample by
    /// the sampling period, the on-CPU view shows 27s of "cvwait
    /// time" for a thread that never actually ran cvwait for 27s --
    /// it just got caught parked there 27,000 times. Off-CPU has its
    /// own real-interval channel (MACH_SCHED records); we don't need
    /// PET to also fake it.
    fn arm(&mut self) -> Result<(), Error> {
        kdebug::set_lightweight_pet(1)?;
        kperf_call(unsafe { (self.fw.kperf_sample_set)(1) }, "sample_set")?;
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
    configured_pmu: Option<&ConfiguredPmu>,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    let pmc_idx_l1d = configured_pmu
        .and_then(|c| c.slot_indices[PmuSlot::L1DCacheMissLoad as usize]);
    let pmc_idx_brmiss = configured_pmu
        .and_then(|c| c.slot_indices[PmuSlot::BranchMispredict as usize]);
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
    // Once the jitdump file appears we tail it incrementally and
    // emit a synthetic BinaryLoadedEvent per `CodeLoad` record so
    // JIT'd functions show up in the live UI by name. The tailer
    // gets re-ticked alongside the existence-check polling.
    let mut jitdump_tailer: Option<JitdumpTailer> = None;

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
    let mut offcpu = CpuIntervalTracker::new();
    // Wall-clock duration represented by one PET tick. Samples and
    // off-CPU intervals are weighted by ns of wall-clock time, so
    // the aggregator works in "duration of activity" rather than
    // "count of samples".
    let pet_period_ns: u64 =
        (1_000_000_000u64 / opts.frequency_hz.max(1) as u64).max(1);
    // Sums of per-thread fixed counter deltas across every sample.
    // Apple Silicon: pmc[0] = cycles, pmc[1] = instructions retired.
    // Empty-slice samples (no PMC_THREAD record arrived) contribute
    // nothing.
    let mut pmu_total_cycles: u64 = 0;
    let mut pmu_total_insns: u64 = 0;
    let mut pmu_samples: u64 = 0;

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
                match JitdumpTailer::open(&jitdump_path) {
                    Ok(t) => {
                        log::info!(
                            "jitdump_tail: opened {} for live tailing",
                            jitdump_path.display()
                        );
                        jitdump_tailer = Some(t);
                    }
                    Err(err) => {
                        log::warn!(
                            "jitdump_tail: failed to open {}: {err}",
                            jitdump_path.display()
                        );
                    }
                }
            } else {
                next_jitdump = Instant::now() + jitdump_period;
            }
        }
        if let Some(t) = jitdump_tailer.as_mut() {
            match t.tick() {
                Ok(records) => {
                    for r in records {
                        // Emit one synthetic image per JIT'd
                        // function. base_avma == text_svma since
                        // JIT code has no relocatable layout
                        // (vma is already the runtime address).
                        let path = format!("[jit] {}", r.name);
                        let symbols = vec![MachOSymbol {
                            start_svma: r.avma,
                            end_svma: r.avma + r.code_size,
                            name: r.name.into_bytes(),
                        }];
                        sink.on_binary_loaded(BinaryLoadedEvent {
                            pid: opts.pid,
                            base_avma: r.avma,
                            vmsize: r.code_size,
                            text_svma: r.avma,
                            path: &path,
                            uuid: None,
                            arch: host_arch_str(),
                            is_executable: false,
                            symbols: &symbols,
                            // Bytes from the jitdump CodeLoad
                            // record. Lets the live UI disassemble
                            // JIT'd functions without needing
                            // task_for_pid + mach_vm_read.
                            text_bytes: Some(&r.code),
                        });
                    }
                }
                Err(err) => log::warn!("jitdump_tail tick: {err}"),
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
                if !sample.pmc.is_empty() {
                    pmu_samples += 1;
                    if let Some(&c) = sample.pmc.first() {
                        pmu_total_cycles = pmu_total_cycles.saturating_add(c);
                    }
                    if let Some(&i) = sample.pmc.get(1) {
                        pmu_total_insns = pmu_total_insns.saturating_add(i);
                    }
                }
                offcpu.note_sample(
                    sample.tid,
                    sample.user_backtrace,
                    sample.kernel_backtrace,
                );
                // With full PET (lightweight_pet=0), kperf emits a
                // sample bracket for every thread it considers on
                // every tick — including ones that are blocked or
                // sleeping. Those have an empty user backtrace
                // because there's no live user PC to start from.
                // Forwarding them to the aggregator just inflates
                // `total_samples` and grows the "(in-kernel / no user
                // stack)" residue until it dominates the flame. Drop
                // them at the source: anything we keep should have a
                // user frame to attribute time to.
                if sample.user_backtrace.is_empty() {
                    return;
                }
                let cycles = sample.pmc.first().copied().unwrap_or(0);
                let instructions = sample.pmc.get(1).copied().unwrap_or(0);
                let l1d_misses = pmc_idx_l1d
                    .and_then(|i| sample.pmc.get(i).copied())
                    .unwrap_or(0);
                let branch_mispreds = pmc_idx_brmiss
                    .and_then(|i| sample.pmc.get(i).copied())
                    .unwrap_or(0);
                sink.on_sample(SampleEvent {
                    timestamp_ns: sample.timestamp_ns,
                    pid: opts.pid,
                    tid: sample.tid,
                    backtrace: sample.user_backtrace,
                    kernel_backtrace: sample.kernel_backtrace,
                    cycles,
                    instructions,
                    l1d_misses,
                    branch_mispreds,
                });
            });
        }
        // pet_period_ns left here for the on-CPU interval emission
        // path below; suppress dead-code warning when not used.
        let _ = pet_period_ns;

        // Emit any wakeup events captured this batch. The waker's
        // stack is borrowed from its last PET tick, so the sink gets
        // "thread X woke thread Y at time T from this stack". The
        // live aggregator builds a "who woke me?" panel from these.
        for w in offcpu.drain_wakeups() {
            sink.on_wakeup(WakeupEvent {
                timestamp_ns: w.timestamp_ns,
                pid: opts.pid,
                waker_tid: w.waker_tid,
                wakee_tid: w.wakee_tid,
                waker_user_stack: &w.waker_user_stack,
                waker_kernel_stack: &w.waker_kernel_stack,
            });
        }

        // Forward every closed CPU interval (on-CPU and off-CPU) to
        // the sink. Both are SCHED-derived; durations are ground
        // truth. The aggregator distributes on-CPU interval time
        // across the PET samples that fell inside it, and credits
        // off-CPU interval time in full to the cached blocking
        // stack (classified by leaf into an OffCpuReason).
        for interval in offcpu.drain_pending() {
            match interval.kind {
                crate::offcpu::PendingKind::OnCpu => {
                    sink.on_cpu_interval(nerf_mac_capture::sample_sink::CpuIntervalEvent {
                        pid: opts.pid,
                        tid: interval.tid,
                        start_ns: interval.start_ns,
                        end_ns: interval.end_ns,
                        kind: nerf_mac_capture::sample_sink::CpuIntervalKind::OnCpu,
                    });
                }
                crate::offcpu::PendingKind::OffCpu {
                    user_stack,
                    kernel_stack: _,
                    waker_tid,
                    waker_user_stack,
                } => {
                    sink.on_cpu_interval(nerf_mac_capture::sample_sink::CpuIntervalEvent {
                        pid: opts.pid,
                        tid: interval.tid,
                        start_ns: interval.start_ns,
                        end_ns: interval.end_ns,
                        kind: nerf_mac_capture::sample_sink::CpuIntervalKind::OffCpu {
                            stack: &user_stack,
                            waker_tid,
                            waker_user_stack: waker_user_stack.as_deref(),
                        },
                    });
                }
            }
        }
    }

    log_session_summary(total_drained, &parser, &histogram, &offcpu);
    if pmu_samples > 0 {
        let ipc = if pmu_total_cycles > 0 {
            pmu_total_insns as f64 / pmu_total_cycles as f64
        } else {
            0.0
        };
        log::info!(
            "PMU (fixed counters across {pmu_samples} samples): \
             cycles={pmu_total_cycles} insns={pmu_total_insns} avg_ipc={ipc:.3}"
        );
    } else {
        log::warn!(
            "PMU: no per-sample counter records observed -- \
             kperf likely didn't emit DBG_PERF/PERF_KPC/DATA_THREAD \
             (kpc_set_thread_counting may need different gating, or PMC_THREAD \
             might require kpc_force_all_ctrs_set permission this run lacks)"
        );
    }
    Ok(())
}

fn log_session_summary(
    total: u64,
    parser: &Parser,
    histogram: &BTreeMap<(u8, u16, u32), u64>,
    offcpu: &CpuIntervalTracker,
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

/// Architecture string for synthetic JIT images. Matches what
/// nwind expects for selecting the disassembler.
fn host_arch_str() -> Option<&'static str> {
    if cfg!(target_arch = "aarch64") {
        Some("aarch64")
    } else if cfg!(target_arch = "x86_64") {
        Some("x86_64")
    } else {
        None
    }
}
