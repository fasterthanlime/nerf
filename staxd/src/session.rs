//! Per-session driver: configure kperf + kdebug + kpc, drain
//! `KERN_KDREADTR`, ship batches over the client's `Tx<KdBufBatch>`,
//! tear down on exit.
//!
//! The kperf/kdebug bring-up sequence mirrors `stax-mac-kperf::recorder`
//! exactly — same order of operations, same lightweight-PET dance,
//! same teardown. The only thing that differs is what we do with the
//! drained records: instead of running them through a parser and
//! emitting `Sample`s, we ship them as-is.

use std::time::{Duration, Instant};

use tracing::{info, warn};

use stax_mac_kperf_sys::bindings::{self, Frameworks};
use stax_mac_kperf_sys::kdebug::{self, kdbg_class, kdbg_code, kdbg_func, kdbg_subclass, perf, KdBuf, KdRegtype, DBG_FUNC_END, DBG_FUNC_START, DBG_PERF, KDBG_TIMESTAMP_MASK};
use staxd_proto::{KdBufBatch, KdBufWire, RecordError, RecordSummary, SessionConfig};

use crate::probe::{self, ProbeRequest};

pub async fn run(
    config: SessionConfig,
    records: vox::Tx<KdBufBatch>,
) -> Result<RecordSummary, RecordError> {
    let fw = bindings::load().map_err(map_kperf_err)?;

    // Earliest cheapest root check. Same gate as the rest of the
    // kpc surface so any failure here also predicts everything else.
    let mut force_ctrs: i32 = 0;
    let rc = unsafe { (fw.kpc_force_all_ctrs_get)(&mut force_ctrs) };
    if rc != 0 {
        return Err(RecordError::NotRoot);
    }

    // Wipe stale state from a previous half-finished session — same
    // motivation as recorder.rs:80-89.
    unsafe {
        let _ = (fw.kperf_sample_set)(0);
        let _ = (fw.kperf_reset)();
    }
    let _ = kdebug::set_lightweight_pet(0);
    let _ = kdebug::enable(false);
    let _ = kdebug::reset();

    setup_kperf(&fw, &config)?;
    setup_kdebug(&config)?;

    let result = drain(&fw, &config, records).await;

    teardown(&fw);
    result
}

fn setup_kperf(fw: &Frameworks, config: &SessionConfig) -> Result<(), RecordError> {
    let actionid: u32 = 1;
    let timerid: u32 = 1;

    kperf_call(
        unsafe { (fw.kperf_action_count_set)(bindings::KPERF_ACTION_MAX) },
        "action_count_set",
    )?;
    kperf_call(
        unsafe { (fw.kperf_timer_count_set)(bindings::KPERF_TIMER_MAX) },
        "timer_count_set",
    )?;
    kperf_call(
        unsafe { (fw.kperf_action_samplers_set)(actionid, config.samplers) },
        "action_samplers_set",
    )?;
    kperf_call(
        unsafe { (fw.kperf_action_filter_set_by_pid)(actionid, config.target_pid as i32) },
        "action_filter_set_by_pid",
    )?;

    let period_ns = if config.frequency_hz == 0 {
        1_000_000
    } else {
        1_000_000_000u64 / config.frequency_hz as u64
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

    if !config.pmu_event_configs.is_empty() {
        let mut configs = config.pmu_event_configs.clone();
        kperf_call(
            unsafe { (fw.kpc_set_config)(config.class_mask, configs.as_mut_ptr()) },
            "kpc_set_config",
        )?;
    }
    kperf_call(
        unsafe { (fw.kpc_set_counting)(config.class_mask) },
        "kpc_set_counting",
    )?;
    kperf_call(
        unsafe { (fw.kpc_set_thread_counting)(config.class_mask) },
        "kpc_set_thread_counting",
    )?;

    // Lightweight PET + sample_set must precede kdebug setup so kdebug
    // ops aren't blocked by an exclusive KTRACE_KPERF (recorder.rs:303).
    kdebug::set_lightweight_pet(1).map_err(map_kperf_err)?;
    kperf_call(unsafe { (fw.kperf_sample_set)(1) }, "sample_set")?;
    Ok(())
}

fn setup_kdebug(config: &SessionConfig) -> Result<(), RecordError> {
    kdebug::reset().map_err(map_kperf_err)?;
    kdebug::set_buf_size(config.buf_records as i32).map_err(map_kperf_err)?;
    kdebug::setup().map_err(map_kperf_err)?;

    let mut filter = KdRegtype {
        ty: kdebug::KDBG_RANGETYPE,
        value1: config.filter_range_value1,
        value2: config.filter_range_value2,
        value3: 0,
        value4: 0,
    };
    kdebug::set_filter(&mut filter).map_err(map_kperf_err)?;
    kdebug::enable(true).map_err(map_kperf_err)?;
    Ok(())
}

async fn drain(
    _fw: &Frameworks,
    config: &SessionConfig,
    records: vox::Tx<KdBufBatch>,
) -> Result<RecordSummary, RecordError> {
    let session_start = Instant::now();
    // Match recorder.rs:377: drain at 2x the sample period so the
    // ringbuffer never fills up. 1kHz → 2ms.
    let drain_period = Duration::from_micros(
        ((1_000_000u64 / config.frequency_hz.max(1) as u64) * 2).max(500),
    );

    let mut buf: Vec<KdBuf> = vec![empty_kdbuf(); config.buf_records as usize];
    let mut total_drained: u64 = 0;

    // Race-against-return measurement probe. The probe walks the
    // suspended thread's stack via framehop and ships the result
    // back over a channel for inclusion in the next KdBufBatch —
    // client-side pipeline correlates with the kperf sample by
    // (tid, kperf_ts_mach) and runs the existing symbolicator.
    // If spawn returns None (task_for_pid denied), recording
    // proceeds without probe results.
    let mut probe_chan = probe::spawn(config.target_pid);
    let mut sample_parser = SampleBoundaryParser::default();
    let mut probe_request_drops: u64 = 0;
    let mut probe_results_buf: Vec<staxd_proto::ProbeResultWire> = Vec::new();

    loop {
        tokio::time::sleep(drain_period).await;

        let n = match kdebug::read_trace(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                warn!("KERN_KDREADTR failed: {e}; ending session");
                return Err(map_kperf_err(e));
            }
        };
        total_drained += n as u64;

        // Probe path: queue a request per completed sample, then
        // drain whatever results have come back since last cycle.
        // Costs in the drain loop are bounded — try_send per closed
        // sample, try_recv to drain the back-channel.
        if let Some(chan) = probe_chan.as_mut() {
            for rec in &buf[..n] {
                if let Some(req) = sample_parser.feed(rec) {
                    if chan.requests.try_send(req).is_err() {
                        probe_request_drops += 1;
                    }
                }
            }
            probe_results_buf.clear();
            while let Ok(result) = chan.results.try_recv() {
                probe_results_buf.push(result);
            }
        }

        // Always send a batch, even when n == 0. The send doubles
        // as our detection of "client went away" — without it, the
        // drain loop spins forever holding ktrace ownership long
        // after the client disconnected.
        let batch = KdBufBatch {
            records: buf[..n].iter().map(kdbuf_to_wire).collect(),
            drained_at_unix_ns: unix_ns_now(),
            probe_results: std::mem::take(&mut probe_results_buf),
        };
        if let Err(e) = records.send(batch).await {
            info!(?e, "client closed records channel; ending session");
            break;
        }
    }

    if probe_chan.is_some() {
        info!(
            probe_request_drops,
            "probe: drain loop ended (request_drops = times the probe-request channel was full)"
        );
    }
    drop(probe_chan);

    Ok(RecordSummary {
        records_drained: total_drained,
        session_ns: session_start.elapsed().as_nanos() as u64,
    })
}

/// Tiny inline kdebug parser: just enough to detect the end of
/// each kperf sample and capture the leaf user PC + appended LR
/// for the probe. Mirrors `stax-mac-kperf-parse::parser::Parser`
/// but stripped to the boundary fields we care about and the
/// user-stack record. Lives here so staxd doesn't have to pull
/// the parser crate's heavyweight deps (object/memmap/capture).
#[derive(Default)]
struct SampleBoundaryParser {
    /// `Some` when we've seen a `PERF_GEN_EVENT|FUNC_START` and
    /// not yet the matching FUNC_END.
    current: Option<InProgress>,
}

struct InProgress {
    tid: u32,
    /// kdebug record's mach-tick timestamp from the FUNC_START
    /// boundary record.
    timestamp_mach: u64,
    /// Total user frames the UHDR claims (includes the appended
    /// LR fixup on arm64).
    ustack_expected: u32,
    /// Raw user frames as they arrive in PERF_CS_UDATA chunks. We
    /// only need frame[0] (leaf PC) and the last frame (appended
    /// LR), but it's easier to collect and index than to track a
    /// running counter.
    ustack: Vec<u64>,
    /// Total kernel frames. Needed only for the kstack_depth
    /// stat we write per probe (lets the analyser bucket by
    /// "interrupt-from-kernel" vs "interrupt-from-user").
    kstack_expected: u32,
}

impl SampleBoundaryParser {
    fn feed(&mut self, rec: &KdBuf) -> Option<ProbeRequest> {
        if kdbg_class(rec.debugid) != DBG_PERF {
            return None;
        }
        let subclass = kdbg_subclass(rec.debugid);
        let code = kdbg_code(rec.debugid);
        let func = kdbg_func(rec.debugid);

        match (subclass, code, func) {
            (perf::sc::GENERIC, 0, DBG_FUNC_START) => {
                self.current = Some(InProgress {
                    tid: rec.arg5 as u32,
                    timestamp_mach: rec.timestamp & KDBG_TIMESTAMP_MASK,
                    ustack_expected: 0,
                    ustack: Vec::new(),
                    kstack_expected: 0,
                });
                None
            }
            (perf::sc::GENERIC, 0, DBG_FUNC_END) => {
                let ip = self.current.take()?;
                // Skip samples where kperf didn't manage to walk
                // the user side — no PC means no probe target.
                if ip.ustack.is_empty() {
                    return None;
                }
                Some(ProbeRequest {
                    tid: ip.tid,
                    kperf_ts_mach: ip.timestamp_mach,
                })
            }
            (perf::sc::CALLSTACK, perf::cs::UHDR, _) => {
                if let Some(ip) = self.current.as_mut() {
                    let main = rec.arg2 as u32;
                    let async_n = rec.arg4 as u32;
                    ip.ustack_expected = main.saturating_add(async_n);
                    ip.ustack.reserve(ip.ustack_expected as usize);
                }
                None
            }
            (perf::sc::CALLSTACK, perf::cs::UDATA, _) => {
                if let Some(ip) = self.current.as_mut() {
                    let chunk = [rec.arg1, rec.arg2, rec.arg3, rec.arg4];
                    let remaining = (ip.ustack_expected as usize)
                        .saturating_sub(ip.ustack.len());
                    let take = remaining.min(4);
                    ip.ustack.extend_from_slice(&chunk[..take]);
                }
                None
            }
            (perf::sc::CALLSTACK, perf::cs::KHDR, _) => {
                if let Some(ip) = self.current.as_mut() {
                    let main = rec.arg2 as u32;
                    let async_n = rec.arg4 as u32;
                    ip.kstack_expected = main.saturating_add(async_n);
                }
                None
            }
            _ => None,
        }
    }
}

fn teardown(fw: &Frameworks) {
    // Same order as recorder.rs:337-355. Errors are logged-and-ignored
    // because we want every step to run even if one fails.
    let _ = kdebug::enable(false);
    let _ = kdebug::reset();
    unsafe {
        let _ = (fw.kperf_sample_set)(0);
    }
    let _ = kdebug::set_lightweight_pet(0);
    unsafe {
        let _ = (fw.kpc_set_counting)(0);
        let _ = (fw.kpc_set_thread_counting)(0);
        let _ = (fw.kpc_force_all_ctrs_set)(0);
        let _ = (fw.kperf_reset)();
    }
}

fn kperf_call(rc: i32, op: &'static str) -> Result<(), RecordError> {
    if rc == 0 {
        Ok(())
    } else {
        Err(RecordError::Kperf { op: op.to_string(), code: rc })
    }
}

fn map_kperf_err(e: stax_mac_kperf_sys::Error) -> RecordError {
    use stax_mac_kperf_sys::Error;
    match e {
        Error::NotRoot => RecordError::NotRoot,
        Error::Sysctl { op, source } => RecordError::Sysctl {
            op: op.to_string(),
            message: source.to_string(),
        },
        Error::Kperf { op, code } => RecordError::Kperf { op: op.to_string(), code },
        Error::Kpep { op, code } => {
            RecordError::Kperf { op: format!("kpep:{op}"), code }
        }
        Error::FrameworkLoad { path, msg } => RecordError::Sysctl {
            op: "FrameworkLoad".into(),
            message: format!("{path}: {msg}"),
        },
        Error::SymbolMissing { name, msg } => RecordError::Sysctl {
            op: "SymbolMissing".into(),
            message: format!("{name}: {msg}"),
        },
        Error::UnknownEvent { name } => RecordError::Sysctl {
            op: "UnknownEvent".into(),
            message: name,
        },
        Error::TooManyEvents(n, cap) => RecordError::Sysctl {
            op: "TooManyEvents".into(),
            message: format!("{n} > {cap}"),
        },
    }
}

fn kdbuf_to_wire(rec: &KdBuf) -> KdBufWire {
    KdBufWire {
        timestamp: rec.timestamp,
        arg1: rec.arg1,
        arg2: rec.arg2,
        arg3: rec.arg3,
        arg4: rec.arg4,
        arg5: rec.arg5,
        debugid: rec.debugid,
        cpuid: rec.cpuid,
        unused: rec.unused,
    }
}

fn empty_kdbuf() -> KdBuf {
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
    }
}

fn unix_ns_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
