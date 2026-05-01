//! Per-session driver: configure kperf + kdebug + kpc, drain
//! `KERN_KDREADTR`, ship batches over the client's `Tx<KdBufBatch>`,
//! tear down on exit.
//!
//! The kperf/kdebug bring-up sequence mirrors `stax-mac-kperf::recorder`
//! exactly — same order of operations, same lightweight-PET dance,
//! same teardown. The only thing that differs is what we do with the
//! drained records: instead of running them through a parser and
//! emitting `Sample`s, we ship them as-is.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use mach2::mach_time::{mach_absolute_time, mach_timebase_info};
use tracing::{info, warn};

use stax_mac_kperf_sys::bindings::{self, Frameworks};
use stax_mac_kperf_sys::kdebug::{
    self, DBG_FUNC_END, DBG_FUNC_START, DBG_PERF, KDBG_TIMESTAMP_MASK, KdBuf, KdRegtype, perf,
};
use staxd_proto::{KdBufBatch, RecordError, RecordSummary, SessionConfig};

pub async fn run(
    config: SessionConfig,
    records: vox::Tx<KdBufBatch>,
    cancel: Arc<AtomicBool>,
) -> Result<RecordSummary, RecordError> {
    let run_start = Instant::now();
    info!(
        pid = config.target_pid,
        frequency_hz = config.frequency_hz,
        buf_records = config.buf_records,
        samplers = config.samplers,
        class_mask = config.class_mask,
        "staxd session starting"
    );

    let phase_start = Instant::now();
    let fw = bindings::load().map_err(map_kperf_err)?;
    info!(elapsed = ?phase_start.elapsed(), "staxd loaded kperf frameworks");

    // Earliest cheapest root check. Same gate as the rest of the
    // kpc surface so any failure here also predicts everything else.
    let phase_start = Instant::now();
    let mut force_ctrs: i32 = 0;
    let rc = unsafe { (fw.kpc_force_all_ctrs_get)(&mut force_ctrs) };
    if rc != 0 {
        return Err(RecordError::NotRoot);
    }
    info!(
        elapsed = ?phase_start.elapsed(),
        force_ctrs,
        "staxd root/kpc gate passed"
    );

    // Wipe stale state from a previous half-finished session — same
    // motivation as recorder.rs:80-89.
    let phase_start = Instant::now();
    unsafe {
        let _ = (fw.kperf_sample_set)(0);
        let _ = (fw.kperf_reset)();
    }
    let _ = kdebug::set_lightweight_pet(0);
    let _ = kdebug::enable(false);
    let _ = kdebug::reset();
    info!(elapsed = ?phase_start.elapsed(), "staxd cleared stale kperf/kdebug state");

    let phase_start = Instant::now();
    setup_kperf(&fw, &config)?;
    info!(elapsed = ?phase_start.elapsed(), "staxd configured kperf");

    let phase_start = Instant::now();
    setup_kdebug(&config)?;
    info!(elapsed = ?phase_start.elapsed(), "staxd configured/enabled kdebug");

    let result = drain(&fw, &config, records, cancel).await;

    let phase_start = Instant::now();
    teardown(&fw);
    info!(
        elapsed = ?phase_start.elapsed(),
        total_elapsed = ?run_start.elapsed(),
        "staxd session torn down"
    );
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
    kperf_call(
        unsafe { (fw.kperf_timer_pet_set)(timerid) },
        "timer_pet_set",
    )?;

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

    if config.typefilter_cscs.is_empty() {
        let mut filter = KdRegtype {
            ty: kdebug::KDBG_RANGETYPE,
            value1: config.filter_range_value1,
            value2: config.filter_range_value2,
            value3: 0,
            value4: 0,
        };
        kdebug::set_filter(&mut filter).map_err(map_kperf_err)?;
    } else {
        kdebug::set_typefilter(config.typefilter_cscs.iter().copied()).map_err(map_kperf_err)?;
    }
    kdebug::enable(true).map_err(map_kperf_err)?;
    Ok(())
}

const SEND_QUEUE_CAPACITY: usize = 256;
const SEND_FLUSH_BUDGET: Duration = Duration::from_secs(2);
const SLOW_SEND_WAIT: Duration = Duration::from_millis(10);
const MIN_DRAIN_READ_BATCH_RECORDS: usize = 4_096;
const MAX_DRAIN_READ_BATCH_RECORDS: usize = 16_384;
const IDLE_SPIN_READS: u32 = 8;
const IDLE_YIELD_READS: u32 = 32;
const KDEBUG_BUFWAIT_TIMEOUT: Duration = Duration::from_millis(1);
const EMPTY_HEARTBEAT_PERIOD: Duration = Duration::from_millis(100);

#[cfg(target_os = "macos")]
const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(
        qos_class: libc::c_uint,
        relative_priority: libc::c_int,
    ) -> libc::c_int;
}

#[derive(Clone, Copy, Debug, Default)]
struct SendDropStats {
    dropped_batches: u64,
    dropped_empty_batches: u64,
    dropped_records: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct SendSummary {
    sent_batches: u64,
    sent_records: u64,
    max_send_wait_ns: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct DrainThreadSummary {
    records_drained: u64,
    batches_queued: u64,
    drops: SendDropStats,
    session_ns: u64,
}

#[derive(Default)]
struct KperfSampleAgeScanner {
    pending: Option<PendingKperfSample>,
}

struct PendingKperfSample {
    timestamp: u64,
    triggered: bool,
}

impl KperfSampleAgeScanner {
    fn feed(&mut self, rec: &KdBuf) -> Option<u64> {
        if kdebug::kdbg_class(rec.debugid) != DBG_PERF {
            return None;
        }

        let subclass = kdebug::kdbg_subclass(rec.debugid);
        let code = kdebug::kdbg_code(rec.debugid);
        let func = kdebug::kdbg_func(rec.debugid);

        match (subclass, code, func) {
            (perf::sc::GENERIC, 0, DBG_FUNC_START) => {
                self.pending = Some(PendingKperfSample {
                    timestamp: rec.timestamp & KDBG_TIMESTAMP_MASK,
                    triggered: false,
                });
                None
            }
            (perf::sc::GENERIC, 0, DBG_FUNC_END) => {
                self.pending = None;
                None
            }
            (perf::sc::CALLSTACK, perf::cs::UHDR, _) => {
                let pending = self.pending.as_mut()?;
                if pending.triggered {
                    return None;
                }
                let user_frames = (rec.arg2 as u32).saturating_add(rec.arg4 as u32);
                if user_frames == 0 {
                    return None;
                }
                pending.triggered = true;
                Some(pending.timestamp)
            }
            _ => None,
        }
    }
}

struct BatchSendQueue {
    tx: flume::Sender<KdBufBatch>,
    drop_rx: flume::Receiver<KdBufBatch>,
    drops: SendDropStats,
}

impl BatchSendQueue {
    fn new(capacity: usize) -> Self {
        let (tx, drop_rx) = flume::bounded(capacity);
        Self {
            tx,
            drop_rx,
            drops: SendDropStats::default(),
        }
    }

    fn receiver(&self) -> flume::Receiver<KdBufBatch> {
        self.drop_rx.clone()
    }

    fn stats(&self) -> SendDropStats {
        self.drops
    }

    fn enqueue_drop_oldest(&mut self, mut batch: KdBufBatch) -> bool {
        loop {
            match self.tx.try_send(batch) {
                Ok(()) => return true,
                Err(flume::TrySendError::Full(returned)) => {
                    batch = returned;
                    if batch.records.is_empty() {
                        self.drops.dropped_empty_batches =
                            self.drops.dropped_empty_batches.saturating_add(1);
                        return true;
                    }
                    // flume does not have drop-oldest send. This queue owns
                    // both ends so kdebug draining can evict one queued batch
                    // and retry without blocking on downstream Vox credit.
                    match self.drop_rx.try_recv() {
                        Ok(dropped) => self.record_drop(dropped),
                        Err(flume::TryRecvError::Empty) => continue,
                        Err(flume::TryRecvError::Disconnected) => return false,
                    }
                }
                Err(flume::TrySendError::Disconnected(_)) => return false,
            }
        }
    }

    fn record_drop(&mut self, batch: KdBufBatch) {
        if batch.records.is_empty() {
            self.drops.dropped_empty_batches = self.drops.dropped_empty_batches.saturating_add(1);
            return;
        }
        self.drops.dropped_batches = self.drops.dropped_batches.saturating_add(1);
        self.drops.dropped_records = self
            .drops
            .dropped_records
            .saturating_add(batch.records.len() as u64);
    }
}

async fn send_batches(
    records: vox::Tx<KdBufBatch>,
    rx: flume::Receiver<KdBufBatch>,
    cancel: Arc<AtomicBool>,
) -> SendSummary {
    let mut summary = SendSummary::default();
    while let Ok(mut batch) = rx.recv_async().await {
        let records_in_batch = batch.records.len() as u64;
        batch.send_started_mach_ticks = mach_ticks_now();
        let send_result = records.send(batch).await;
        if let Err(e) = send_result {
            info!(?e, "client closed records channel; ending staxd sender");
            cancel.store(true, Ordering::Relaxed);
            break;
        }
        summary.sent_batches = summary.sent_batches.saturating_add(1);
        summary.sent_records = summary.sent_records.saturating_add(records_in_batch);
    }
    summary
}

async fn drain(
    _fw: &Frameworks,
    config: &SessionConfig,
    records: vox::Tx<KdBufBatch>,
    cancel: Arc<AtomicBool>,
) -> Result<RecordSummary, RecordError> {
    let session_start = Instant::now();
    let mut send_queue = BatchSendQueue::new(SEND_QUEUE_CAPACITY);
    let mut sender_task =
        tokio::spawn(send_batches(records, send_queue.receiver(), cancel.clone()));

    info!(
        buf_records = config.buf_records,
        min_read_batch_records = MIN_DRAIN_READ_BATCH_RECORDS
            .min(config.buf_records as usize)
            .max(1),
        max_read_batch_records = MAX_DRAIN_READ_BATCH_RECORDS
            .min(config.buf_records as usize)
            .max(1),
        send_queue_capacity = SEND_QUEUE_CAPACITY,
        "staxd drain supervisor starting"
    );

    let sent_ready_at_unix_ns = unix_ns_now();
    let sent_ready_mach_ticks = mach_ticks_now();
    if !send_queue.enqueue_drop_oldest(KdBufBatch {
        records: Vec::new(),
        read_started_mach_ticks: sent_ready_mach_ticks,
        drained_mach_ticks: sent_ready_mach_ticks,
        queued_for_send_mach_ticks: sent_ready_mach_ticks,
        send_started_mach_ticks: 0,
        drained_at_unix_ns: sent_ready_at_unix_ns,
    }) {
        info!("records sender closed before ready batch");
        return Ok(RecordSummary {
            records_drained: 0,
            session_ns: session_start.elapsed().as_nanos() as u64,
        });
    }
    info!(
        elapsed = ?session_start.elapsed(),
        drained_at_unix_ns = sent_ready_at_unix_ns,
        mach_ticks = sent_ready_mach_ticks,
        "staxd sent ready batch"
    );

    let thread_config = config.clone();
    let thread_cancel = cancel.clone();
    let drain_thread = thread::Builder::new()
        .name("staxd-kdebug-drain".to_string())
        .spawn(move || {
            drain_kdebug_loop(
                thread_config,
                send_queue,
                thread_cancel,
                session_start,
                sent_ready_at_unix_ns,
                1,
            )
        })
        .map_err(|e| RecordError::Sysctl {
            op: "spawn staxd-kdebug-drain".to_string(),
            message: e.to_string(),
        })?;

    let drain_result = match tokio::task::spawn_blocking(move || drain_thread.join()).await {
        Ok(Ok(result)) => result,
        Ok(Err(_panic)) => Err(RecordError::Sysctl {
            op: "staxd-kdebug-drain".to_string(),
            message: "thread panicked".to_string(),
        }),
        Err(e) => Err(RecordError::Sysctl {
            op: "join staxd-kdebug-drain".to_string(),
            message: e.to_string(),
        }),
    };

    let send_summary = match tokio::time::timeout(SEND_FLUSH_BUDGET, &mut sender_task).await {
        Ok(Ok(summary)) => summary,
        Ok(Err(e)) => {
            warn!("staxd sender task join failed: {e}");
            SendSummary::default()
        }
        Err(_) => {
            sender_task.abort();
            warn!(
                budget = ?SEND_FLUSH_BUDGET,
                "staxd sender did not flush before teardown; aborting"
            );
            SendSummary::default()
        }
    };

    let drain_summary = drain_result?;
    info!(
        records_drained = drain_summary.records_drained,
        batches_queued = drain_summary.batches_queued,
        sent_batches = send_summary.sent_batches,
        sent_records = send_summary.sent_records,
        max_send_wait_ns = send_summary.max_send_wait_ns,
        dropped_batches = drain_summary.drops.dropped_batches,
        dropped_empty_batches = drain_summary.drops.dropped_empty_batches,
        dropped_records = drain_summary.drops.dropped_records,
        elapsed_ns = drain_summary.session_ns,
        "staxd drain loop ended"
    );

    Ok(RecordSummary {
        records_drained: drain_summary.records_drained,
        session_ns: drain_summary.session_ns,
    })
}

fn drain_kdebug_loop(
    config: SessionConfig,
    mut send_queue: BatchSendQueue,
    cancel: Arc<AtomicBool>,
    session_start: Instant,
    sent_ready_at_unix_ns: u64,
    mut send_count: u64,
) -> Result<DrainThreadSummary, RecordError> {
    set_drain_thread_qos();

    let min_read_batch_records = MIN_DRAIN_READ_BATCH_RECORDS
        .min(config.buf_records as usize)
        .max(1);
    let max_read_batch_records = MAX_DRAIN_READ_BATCH_RECORDS
        .min(config.buf_records as usize)
        .max(min_read_batch_records);
    let mut read_batch_records = min_read_batch_records;
    let mut buf: Vec<KdBuf> = vec![empty_kdbuf(); max_read_batch_records];
    let mut total_drained: u64 = 0;
    let mut first_nonempty_logged = false;
    let mut last_read_started_mach_ticks = 0u64;
    let mut consecutive_empty_reads = 0u32;
    let mut last_empty_heartbeat = Instant::now();
    let mut last_drop_log = Instant::now();
    let mut last_slow_drain_log = Instant::now() - Duration::from_secs(1);
    let mut last_logged_drops = SendDropStats::default();
    let mut logged_bufwait_error = false;

    let mut kperf_age_scanner = KperfSampleAgeScanner::default();

    info!(
        min_read_batch_records,
        max_read_batch_records,
        idle_spin_reads = IDLE_SPIN_READS,
        idle_yield_reads = IDLE_YIELD_READS,
        bufwait_timeout = ?KDEBUG_BUFWAIT_TIMEOUT,
        empty_heartbeat_period = ?EMPTY_HEARTBEAT_PERIOD,
        "staxd kdebug drain thread starting"
    );

    loop {
        if cancel.load(Ordering::Relaxed) {
            info!("stop requested; ending kdebug drain");
            break;
        }

        if consecutive_empty_reads > 0 {
            if consecutive_empty_reads <= IDLE_SPIN_READS {
                std::hint::spin_loop();
            } else if consecutive_empty_reads <= IDLE_YIELD_READS {
                thread::yield_now();
            } else {
                let wait_started = Instant::now();
                match kdebug::wait_for_buffer(KDEBUG_BUFWAIT_TIMEOUT) {
                    Ok(true) => {}
                    Ok(false) => {}
                    Err(e) => {
                        if !logged_bufwait_error {
                            warn!("KERN_KDBUFWAIT failed: {e}; falling back to thread sleep");
                            logged_bufwait_error = true;
                        }
                        thread::sleep(KDEBUG_BUFWAIT_TIMEOUT);
                    }
                }
            }
        }

        if cancel.load(Ordering::Relaxed) {
            info!("stop requested; ending kdebug drain");
            break;
        }

        let read_started_mach_ticks = mach_ticks_now();
        let read_gap_ns = if last_read_started_mach_ticks == 0 {
            0
        } else {
            elapsed_ticks_to_ns(read_started_mach_ticks, last_read_started_mach_ticks)
        };
        last_read_started_mach_ticks = read_started_mach_ticks;

        let n = match kdebug::read_trace(&mut buf[..read_batch_records]) {
            Ok(n) => n,
            Err(e) => {
                warn!("KERN_KDREADTR failed: {e}; ending session");
                return Err(map_kperf_err(e));
            }
        };
        let drained_mach_ticks = mach_ticks_now();
        let drained_at_unix_ns = unix_ns_now();
        if n == 0 {
            read_batch_records = min_read_batch_records;
            consecutive_empty_reads = consecutive_empty_reads.saturating_add(1);
            if last_empty_heartbeat.elapsed() >= EMPTY_HEARTBEAT_PERIOD {
                if !send_queue.enqueue_drop_oldest(KdBufBatch {
                    records: Vec::new(),
                    read_started_mach_ticks,
                    drained_mach_ticks,
                    queued_for_send_mach_ticks: mach_ticks_now(),
                    send_started_mach_ticks: 0,
                    drained_at_unix_ns,
                }) {
                    info!("records sender closed; ending session");
                    break;
                }
                send_count = send_count.saturating_add(1);
                last_empty_heartbeat = Instant::now();
            }
            continue;
        }

        if n == read_batch_records && read_batch_records < max_read_batch_records {
            read_batch_records = read_batch_records
                .saturating_mul(2)
                .min(max_read_batch_records);
        } else if n < read_batch_records / 4 && read_batch_records > min_read_batch_records {
            read_batch_records = (read_batch_records / 2).max(min_read_batch_records);
        }

        consecutive_empty_reads = 0;
        last_empty_heartbeat = Instant::now();
        total_drained = total_drained.saturating_add(n as u64);
        let oldest_record_age_ns = oldest_record_age_ns(&buf, n, read_started_mach_ticks);
        let newest_record_age_ns = newest_record_age_ns(&buf, n, read_started_mach_ticks);
        for rec in &buf[..n] {
            let _ = kperf_age_scanner.feed(rec);
        }

        if !first_nonempty_logged {
            first_nonempty_logged = true;
            info!(
                elapsed = ?session_start.elapsed(),
                records = n,
                first_ts = buf.first().map(|rec| rec.timestamp).unwrap_or(0),
                last_ts = buf.get(n.saturating_sub(1)).map(|rec| rec.timestamp).unwrap_or(0),
                drained_at_unix_ns,
                read_started_mach_ticks,
                drained_mach_ticks,
                read_gap_ns,
                oldest_record_age_ns,
                newest_record_age_ns,
                ready_to_first_nonempty_ns = drained_at_unix_ns.saturating_sub(sent_ready_at_unix_ns),
                "staxd first non-empty kdebug drain"
            );
        }

        let records_vec = buf[..n].to_vec();
        let queued_for_send_mach_ticks = mach_ticks_now();
        let batch = KdBufBatch {
            records: records_vec,
            read_started_mach_ticks,
            drained_mach_ticks,
            queued_for_send_mach_ticks,
            send_started_mach_ticks: 0,
            drained_at_unix_ns,
        };
        if !send_queue.enqueue_drop_oldest(batch) {
            info!("records sender closed; ending session");
            break;
        }
        send_count = send_count.saturating_add(1);

        let drops = send_queue.stats();
        if drops.dropped_records != last_logged_drops.dropped_records
            && last_drop_log.elapsed() >= Duration::from_secs(1)
        {
            warn!(
                dropped_batches = drops.dropped_batches,
                dropped_empty_batches = drops.dropped_empty_batches,
                dropped_records = drops.dropped_records,
                "staxd sender queue dropping batches"
            );
            last_logged_drops = drops;
            last_drop_log = Instant::now();
        }
        if (oldest_record_age_ns >= 100_000_000 || read_gap_ns >= 100_000_000)
            && last_slow_drain_log.elapsed() >= Duration::from_secs(1)
        {
            info!(
                records = n,
                read_batch_records,
                read_gap_ns,
                oldest_record_age_ns,
                newest_record_age_ns,
                "staxd slow drain cycle"
            );
            last_slow_drain_log = Instant::now();
        }
    }

    let final_drops = send_queue.stats();
    drop(send_queue);
    Ok(DrainThreadSummary {
        records_drained: total_drained,
        batches_queued: send_count,
        drops: final_drops,
        session_ns: session_start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
    })
}

fn set_drain_thread_qos() {
    #[cfg(target_os = "macos")]
    unsafe {
        let rc = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        if rc != 0 {
            warn!(errno = rc, "failed to raise staxd kdebug drain thread QoS");
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
        Err(RecordError::Kperf {
            op: op.to_string(),
            code: rc,
        })
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
        Error::Kperf { op, code } => RecordError::Kperf {
            op: op.to_string(),
            code,
        },
        Error::Kpep { op, code } => RecordError::Kperf {
            op: format!("kpep:{op}"),
            code,
        },
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

#[inline]
fn mach_ticks_now() -> u64 {
    unsafe { mach_absolute_time() }
}

fn elapsed_ticks_to_ns(later: u64, earlier: u64) -> u64 {
    if later < earlier {
        return 0;
    }
    let mut info = mach_timebase_info { numer: 0, denom: 0 };
    let rc = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
    if rc != 0 || info.denom == 0 {
        return later - earlier;
    }
    (((later - earlier) as u128) * info.numer as u128 / info.denom as u128).min(u64::MAX as u128)
        as u64
}

fn oldest_record_age_ns(buf: &[KdBuf], n: usize, read_started_mach_ticks: u64) -> u64 {
    buf.first()
        .filter(|_| n > 0)
        .map(|rec| elapsed_ticks_to_ns(read_started_mach_ticks, rec.timestamp))
        .unwrap_or(0)
}

fn newest_record_age_ns(buf: &[KdBuf], n: usize, read_started_mach_ticks: u64) -> u64 {
    n.checked_sub(1)
        .and_then(|idx| buf.get(idx))
        .map(|rec| elapsed_ticks_to_ns(read_started_mach_ticks, rec.timestamp))
        .unwrap_or(0)
}
