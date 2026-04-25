//! Recording driver. Configures kperf's PET (Profile Every Thread)
//! mode for a target PID, enables the kdebug ringbuffer, drains it
//! on a schedule, and emits parsed samples to a [`SampleSink`].
//!
//! On drop, the [`Session`] guard tears down kperf + kdebug state in
//! the same order as mperf's `profiling_cleanup` so the host kernel
//! is left in a clean state even on panic.

use std::time::{Duration, Instant};

use nerf_mac_capture::SampleSink;

use crate::bindings::{self, sampler, Frameworks};
use crate::error::Error;
use crate::kdebug::{self, KdBuf, KdRegtype};

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

    let mut session = Session::start(&fw, &opts)?;
    session.attach_to_pid(opts.pid)?;
    session.enable_kdebug(&opts)?;

    drain_loop(&fw, &opts, sink, &mut should_stop)?;

    drop(session);
    Ok(())
}

// ---------------------------------------------------------------------------
// Session: lifecycle guard for kperf + kdebug kernel state
// ---------------------------------------------------------------------------

struct Session<'a> {
    fw: &'a Frameworks,
    actionid: u32,
    #[allow(dead_code)]
    timerid: u32,
}

impl<'a> Session<'a> {
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

        Ok(Self { fw, actionid, timerid })
    }

    fn attach_to_pid(&mut self, pid: u32) -> Result<(), Error> {
        kperf_call(
            unsafe {
                (self.fw.kperf_action_filter_set_by_pid)(self.actionid, pid as i32)
            },
            "action_filter_set_by_pid",
        )?;
        Ok(())
    }

    fn enable_kdebug(&mut self, opts: &RecordOptions) -> Result<(), Error> {
        // Wipe any leftover state.
        kdebug::reset()?;
        kdebug::set_buf_size(opts.kdebug_buf_records)?;
        kdebug::setup()?;

        // Range-filter to the DBG_PERF class so we don't drown in
        // unrelated kernel events. We'll narrow further to the
        // specific PERF_STK_* codes once those are wired up; for
        // now class-level keeps the parser simple.
        let mut filter = KdRegtype {
            ty: kdebug::KDBG_RANGETYPE,
            value1: kdebug::kdbg_eventid(kdebug::DBG_PERF, 0, 0),
            value2: kdebug::kdbg_eventid(kdebug::DBG_PERF, 0xff, 0x3fff),
            value3: 0,
            value4: 0,
        };
        kdebug::set_filter(&mut filter)?;

        kperf_call(unsafe { (self.fw.kperf_sample_set)(1) }, "sample_set")?;
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
    _sink: &mut S,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    let start = Instant::now();
    let drain_period = Duration::from_micros(
        ((1_000_000 / opts.frequency_hz.max(1)) * 2).into(),
    );

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

        let n = kdebug::read_trace(&mut buf)?;
        if n == 0 {
            continue;
        }

        // TODO(kperf-stack-parser): assemble SampleEvent from the
        // sequence:
        //   PERF_PET_SAMPLE_THREAD | DBG_FUNC_START   (sample begins)
        //     PERF_TI_DATA                            (thread info)
        //     PERF_CS_UHDR  arg1=flags arg2=nframes-async arg3=async_idx arg4=async_nframes
        //     PERF_CS_UDATA arg1..arg4 = 4 frames     (repeats ceil(nframes/4) times)
        //     PERF_CS_KHDR  + PERF_CS_KDATA  (same shape, kernel side)
        //     PERF_CS_ERROR (optional)
        //   PERF_PET_SAMPLE_THREAD | DBG_FUNC_END     (sample ends)
        // tid is in kd_buf.arg5 of every record, set by the kernel
        // when the kperf handler ran. See xnu osfmk/kperf/{buffer.h,
        // callstack.c, pet.c}.
        log::trace!("drained {n} kdebug records (parser TODO)");
    }

    Ok(())
}

fn kperf_call(rc: i32, op: &'static str) -> Result<(), Error> {
    if rc != 0 {
        return Err(Error::Kperf { op, code: rc });
    }
    Ok(())
}
