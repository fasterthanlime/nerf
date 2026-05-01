//! Client-side driver for the staxd RPC.
//!
//! Connects to staxd over a vox local socket, asks it to start a
//! kperf+kdebug session for the target pid, and consumes the streaming
//! `KdBufBatch`es it sends back. Each record runs through the shared
//! [`Pipeline`] in `stax-mac-kperf-parse` — the same parser, off-CPU
//! tracker, libproc image / thread scanner, kernel-symbol slide
//! estimator, and jitdump tailer the in-process recorder uses, so
//! the daemon-driven path emits exactly the same `SampleSink` event
//! sequence as the in-process kperf path.

#![cfg(target_os = "macos")]

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use mach2::mach_time::mach_absolute_time;
use stax_mac_capture::SampleSink;
use stax_mac_kperf_parse::pipeline::{Pipeline, PipelineConfig};
use stax_mac_kperf_sys::bindings::sampler;
use stax_mac_kperf_sys::kdebug::{
    self, DBG_FUNC_END, DBG_FUNC_START, DBG_MACH, DBG_MACH_SCHED, DBG_PERF, KDBG_TIMESTAMP_MASK,
    KdBuf, perf,
};
use staxd_proto::{KdBufBatch, STAXD_RECORD_CHANNEL_CAPACITY, SessionConfig, StaxdClient};
use tracing::{info, warn};

/// User-facing options. Mirrors the shape of
/// `stax_mac_kperf::RecordOptions` so plumbing through the existing
/// CLI is mechanical.
#[derive(Clone)]
pub struct RemoteOptions {
    /// `local://` URL or path of the daemon socket. Either
    /// `local:///var/run/staxd.sock` or just `/var/run/staxd.sock`
    /// works; the latter is normalised below.
    pub daemon_socket: String,
    /// Target pid.
    pub pid: u32,
    /// Sampling frequency in Hz.
    pub frequency_hz: u32,
    /// If `Some`, stop after this duration. The daemon's drain loop
    /// continues until we close the records channel; we do that when
    /// the duration elapses or `should_stop` returns `true`.
    pub duration: Option<Duration>,
    /// kdebug ringbuffer size in records. Mirrors the in-process default.
    pub buf_records: u32,

    /// Optional Mach task port for the target process. When set,
    /// the image scanner uses `task_info(TASK_DYLD_INFO)` to detect
    /// dlopen/dlclose without walking proc_pidinfo every tick.
    pub task: Option<mach2::port::mach_port_t>,
}

impl std::fmt::Debug for RemoteOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteOptions")
            .field("daemon_socket", &self.daemon_socket)
            .field("pid", &self.pid)
            .field("frequency_hz", &self.frequency_hz)
            .field("duration", &self.duration)
            .field("buf_records", &self.buf_records)
            .field("task", &self.task.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KperfProbeTriggerTiming {
    /// Kdebug timestamp of the kperf sample start, in mach ticks.
    pub kperf_ts: u64,
    /// mach_absolute_time immediately before staxd called KERN_KDREADTR.
    pub staxd_read_started: u64,
    /// mach_absolute_time immediately after staxd's KERN_KDREADTR returned.
    pub staxd_drained: u64,
    /// mach_absolute_time immediately before staxd queued the batch
    /// for its sender task.
    pub staxd_queued_for_send: u64,
    /// mach_absolute_time immediately before staxd handed the batch to vox.
    pub staxd_send_started: u64,
    /// mach_absolute_time immediately after this client received the batch.
    pub client_received: u64,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            daemon_socket: "/tmp/staxd.sock".into(),
            pid: 0,
            frequency_hz: 1000,
            duration: None,
            buf_records: 1_000_000,
            task: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connecting to staxd at {url}: {source}")]
    Connect {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("staxd record() RPC failed: {0:?}")]
    Rpc(staxd_proto::RecordError),

    #[error("vox call returned an error: {0}")]
    VoxCall(String),

    #[error("parser worker did not stop within {budget:?}")]
    WorkerShutdownTimedOut { budget: Duration },
}

/// Run a remote recording session. Blocks until `should_stop` returns
/// `true`, the duration elapses, or the daemon closes the channel
/// (typically because it errored out, e.g. lost ktrace ownership).
///
/// The caller's `sink` receives the same events the in-process
/// recorder emits — `on_sample`, `on_thread_name`, `on_binary_loaded`,
/// `on_wakeup`, `on_cpu_interval`, `on_jitdump`, `on_kallsyms`, etc. —
/// so live aggregators / archive writers plug in without changes.
pub async fn drive_session<S: SampleSink + Send + 'static>(
    opts: RemoteOptions,
    sink: S,
    should_stop: impl FnMut() -> bool,
) -> Result<(), Error> {
    drive_session_with_hooks(opts, sink, should_stop, || {}, |_, _| {}).await
}

/// Like [`drive_session`], but calls `on_kperf_sample_start` from a
/// dedicated scanner thread as soon as the raw kperf records show that
/// the current sample has a user stack. This hook keeps probe demand
/// tracking off the heavier parser / symbol / image pipeline, and the
/// records receive loop must not scan batches inline.
pub async fn drive_session_with_hooks<S, Stop, FirstBatch, SampleStart>(
    opts: RemoteOptions,
    sink: S,
    mut should_stop: Stop,
    mut on_first_batch: FirstBatch,
    on_kperf_sample_start: SampleStart,
) -> Result<(), Error>
where
    S: SampleSink + Send + 'static,
    Stop: FnMut() -> bool,
    FirstBatch: FnMut(),
    SampleStart: FnMut(u32, KperfProbeTriggerTiming) + Send + 'static,
{
    let url = if opts.daemon_socket.starts_with("local://") {
        opts.daemon_socket.clone()
    } else {
        format!("local://{}", opts.daemon_socket)
    };

    let client_start = Instant::now();
    info!(
        "staxd-client: session starting url={url} pid={} frequency_hz={} buf_records={} duration={:?}",
        opts.pid, opts.frequency_hz, opts.buf_records, opts.duration
    );
    let phase_start = Instant::now();
    info!("staxd-client: connecting to {url} channel_capacity={STAXD_RECORD_CHANNEL_CAPACITY}");
    let observer = stax_vox_observe::VoxObserverLogger::new("staxd-client", "staxd-records")
        .with_pid(opts.pid);

    let client: StaxdClient = match vox::connect(&url)
        .channel_capacity(STAXD_RECORD_CHANNEL_CAPACITY)
        .observer(observer)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            // The "no such file" case dominates — the user forgot to
            // start the daemon. Render an actionable hint instead of
            // bare io::ErrorKind::NotFound so they know what to do.
            let socket_missing = !std::path::Path::new(&opts.daemon_socket).exists()
                && !opts.daemon_socket.starts_with("local://");
            let hint = if socket_missing {
                " (daemon socket doesn't exist — is staxd running? \
                 try `sudo stax setup` to install it as a LaunchDaemon, \
                 or `sudo staxd --socket <path>` for a one-off)"
            } else {
                ""
            };
            return Err(Error::Connect {
                url: format!("{url}{hint}"),
                source: Box::new(e),
            });
        }
    };
    let _debug_registration = stax_vox_observe::register_global_caller(
        "staxd-client",
        "staxd-records",
        "Staxd",
        &client.caller,
    );
    info!(
        "staxd-client: connected to daemon elapsed={:?}",
        phase_start.elapsed()
    );

    let phase_start = Instant::now();
    let status = client
        .status()
        .await
        .map_err(|e| Error::VoxCall(format!("status: {e:?}")))?;
    info!(
        "staxd-client: daemon v{} arch={} state={:?} elapsed={:?}",
        status.version,
        status.host_arch,
        status.state,
        phase_start.elapsed()
    );

    // Pipeline owns the parser + off-CPU tracker + image scanner +
    // thread name cache + kernel image + slide estimator + jitdump
    // tailer. Its periodic scans (image rescan, thread-name rescan)
    // are *synchronous* and do disk IO + symbol-table parsing —
    // running them on the same task as `rx.recv()` would (and did)
    // wedge the recv path during scans and fill the kdebug ring on
    // the daemon side.
    //
    // Move the pipeline + sink onto a dedicated OS worker thread.
    // This recv loop becomes pure I/O: receive, hand off, repeat.
    // The worker drives its own tick cadence and never touches
    // anything async.
    let shared_cache: Option<Arc<stax_mac_shared_cache::SharedCache>> =
        stax_mac_shared_cache::SharedCache::for_host().map(Arc::new);
    let pipeline_config = PipelineConfig {
        pid: opts.pid,
        frequency_hz: opts.frequency_hz,
        pmc_idx_l1d: None,
        pmc_idx_brmiss: None,
        task: opts.task,
    };

    let parser_queue = ParserQueue::new(WORKER_QUEUE_CAPACITY);
    let worker_rx = parser_queue.receiver();
    let worker_counters = parser_queue.counters();
    let scanner_queue = ProbeScannerQueue::new(PROBE_SCANNER_QUEUE_CAPACITY);
    let scanner_rx = scanner_queue.receiver();
    let scanner_counters = scanner_queue.counters();
    let abort_worker_backlog = Arc::new(AtomicBool::new(false));
    let worker_abort = abort_worker_backlog.clone();
    let probe_trigger_count = Arc::new(AtomicU64::new(0));
    let phase_start = Instant::now();
    let worker_handle = std::thread::Builder::new()
        .name("staxd-client-worker".to_owned())
        .spawn(move || {
            worker_thread(
                pipeline_config,
                shared_cache,
                sink,
                worker_abort,
                worker_rx,
                worker_counters,
            )
        })
        .map_err(|e| Error::VoxCall(format!("spawn worker thread: {e}")))?;
    info!(
        "staxd-client: parser worker spawned elapsed={:?}",
        phase_start.elapsed()
    );

    // Use the broad range filter again while validating the live
    // profiler path; this keeps MACH_SCHED records present in
    // correlation mode.
    let session_config = SessionConfig {
        target_pid: opts.pid,
        frequency_hz: opts.frequency_hz,
        buf_records: opts.buf_records,
        samplers: sampler::TH_INFO | sampler::USTACK | sampler::KSTACK | sampler::PMC_THREAD,
        // v0: no configurable PMU events. Daemon falls back to FIXED.
        pmu_event_configs: Vec::new(),
        class_mask: stax_mac_kperf_sys::bindings::KPC_CLASS_FIXED_MASK,
        filter_range_value1: kdebug::kdbg_eventid(DBG_MACH, DBG_MACH_SCHED, 0),
        filter_range_value2: kdebug::kdbg_eventid(DBG_PERF, 0xff, 0x3fff),
        typefilter_cscs: Vec::new(),
    };

    // Server→client streaming: we construct the channel, hand `tx` to
    // the RPC, drain `rx` here. The RPC future doesn't resolve until
    // the daemon's record() returns (clean stop or error).
    let (tx, mut rx) = vox::channel::<KdBufBatch>();
    let record_rpc_start = Instant::now();
    let scanner_probe_trigger_count = probe_trigger_count.clone();
    let scanner_handle = std::thread::Builder::new()
        .name("staxd-client-probe-scanner".to_owned())
        .spawn(move || {
            probe_scanner_thread(
                scanner_rx,
                scanner_counters,
                scanner_probe_trigger_count,
                client_start,
                record_rpc_start,
                on_kperf_sample_start,
            )
        })
        .map_err(|e| Error::VoxCall(format!("spawn probe scanner thread: {e}")))?;
    info!("staxd-client: spawning record RPC");
    let record_fut = tokio::spawn({
        let client = client.clone();
        async move { client.record(session_config, tx).await }
    });

    let session_start = Instant::now();
    let mut total_drained: u64 = 0;
    let mut seen_first_batch = false;
    let mut seen_first_nonempty_batch = false;
    loop {
        if should_stop() {
            info!("staxd-client: stop requested");
            let _ = client.stop_recording().await;
            break;
        }
        if let Some(d) = opts.duration
            && session_start.elapsed() >= d
        {
            info!("staxd-client: duration elapsed");
            let _ = client.stop_recording().await;
            break;
        }

        // Recv loop: pure I/O. Short timeout so we re-check
        // should_stop / duration even on idle targets; no work
        // happens here.
        let recv_timeout = Duration::from_millis(50);
        let recv_started = Instant::now();
        let batch_sref = match tokio::time::timeout(recv_timeout, rx.recv()).await {
            Ok(Ok(Some(value))) => value,
            Ok(Ok(None)) => {
                info!("staxd-client: daemon closed records channel");
                break;
            }
            Ok(Err(e)) => {
                warn!("staxd-client: recv error: {e:?}");
                break;
            }
            Err(_) => continue,
        };

        let batch_handle_started = Instant::now();
        let client_received_mach_ticks = mach_ticks_now();
        let client_received_unix_ns = unix_ns_now();
        let sref_map_started = Instant::now();
        let _ = batch_sref.map(|batch| {
            if !seen_first_batch {
                seen_first_batch = true;
                info!(
                    "staxd-client: first batch arrived records={} since_client_start={:?} since_record_rpc_spawn={:?} since_recv_loop_start={:?} drained_at_unix_ns={} received_at_unix_ns={} read_started_mach={} drained_mach={} queued_for_send_mach={} send_started_mach={} client_received_mach={}",
                    batch.records.len(),
                    client_start.elapsed(),
                    record_rpc_start.elapsed(),
                    session_start.elapsed(),
                    batch.drained_at_unix_ns,
                    client_received_unix_ns,
                    batch.read_started_mach_ticks,
                    batch.drained_mach_ticks,
                    batch.queued_for_send_mach_ticks,
                    batch.send_started_mach_ticks,
                    client_received_mach_ticks
                );
                on_first_batch();
            }
            if !batch.records.is_empty() && !seen_first_nonempty_batch {
                seen_first_nonempty_batch = true;
                info!(
                    "staxd-client: first non-empty batch arrived records={} first_ts={} last_ts={} drained_at_unix_ns={} received_at_unix_ns={} read_started_mach={} drained_mach={} queued_for_send_mach={} send_started_mach={} client_received_mach={} since_client_start={:?} since_record_rpc_spawn={:?}",
                    batch.records.len(),
                    batch.records.first().map(|rec| rec.timestamp).unwrap_or(0),
                    batch.records.last().map(|rec| rec.timestamp).unwrap_or(0),
                    batch.drained_at_unix_ns,
                    client_received_unix_ns,
                    batch.read_started_mach_ticks,
                    batch.drained_mach_ticks,
                    batch.queued_for_send_mach_ticks,
                    batch.send_started_mach_ticks,
                    client_received_mach_ticks,
                    client_start.elapsed(),
                    record_rpc_start.elapsed()
                );
            }
            total_drained += batch.records.len() as u64;
            let records = Arc::new(batch.records);
            let scanner_enqueue_started = Instant::now();
            scanner_queue.enqueue_batch(ProbeScannerBatch {
                records: records.clone(),
                timing: KperfProbeTriggerTiming {
                    kperf_ts: 0,
                    staxd_read_started: batch.read_started_mach_ticks,
                    staxd_drained: batch.drained_mach_ticks,
                    staxd_queued_for_send: batch.queued_for_send_mach_ticks,
                    staxd_send_started: batch.send_started_mach_ticks,
                    client_received: client_received_mach_ticks,
                },
            });

            let parser_enqueue_started = Instant::now();
            parser_queue.enqueue_records(records);

        });
    }

    // Drop our `Rx`. With vox propagating per-channel close to the
    // server's Tx::send (since the upstream patch), the daemon's
    // next send fails with Transport, its drain loop breaks, kperf
    // teardown runs, record() returns with a RecordSummary or error.
    drop(rx);
    // Tell the worker we're done by closing the sender. The worker
    // gets first shot at draining the bounded backlog; we only ask it
    // to drop queued parser work if shutdown exceeds the budget.
    let queue_counters = parser_queue.counters();
    drop(parser_queue);
    let scanner_queue_counters = scanner_queue.counters();
    drop(scanner_queue);

    let rpc_result = record_fut
        .await
        .map_err(|e| Error::VoxCall(format!("join: {e:?}")))?;
    match rpc_result {
        Ok(summary) => info!(
            "staxd-client: session ended cleanly, daemon drained {} records ({:?} session)",
            summary.records_drained,
            Duration::from_nanos(summary.session_ns)
        ),
        Err(vox::VoxError::User(e)) => {
            warn!("staxd-client: daemon returned error: {e:?}");
            let _ = scanner_handle.join();
            let _ = join_worker_with_deadline(worker_handle, abort_worker_backlog).await;
            return Err(Error::Rpc(e));
        }
        Err(e) => {
            let _ = scanner_handle.join();
            let _ = join_worker_with_deadline(worker_handle, abort_worker_backlog).await;
            return Err(Error::VoxCall(format!("record rpc: {e:?}")));
        }
    }

    let _ = scanner_handle.join();
    if !join_worker_with_deadline(worker_handle, abort_worker_backlog).await {
        let queue_stats = queue_counters.stats();
        log_parser_queue_stats(queue_stats);
        return Err(Error::WorkerShutdownTimedOut {
            budget: WORKER_SHUTDOWN_BUDGET,
        });
    }

    info!("staxd-client: locally drained {total_drained} records");
    let queue_stats = queue_counters.stats();
    log_parser_queue_stats(queue_stats);
    info!(
        "staxd-client: session finished total_elapsed={:?} records={} probe_triggers={} parser_dropped_chunks={} parser_dropped_records={} parser_dropped_kperf_samples={} parser_max_queue_depth={} parser_max_queue_age_ns={}",
        client_start.elapsed(),
        total_drained,
        probe_trigger_count.load(Ordering::Relaxed),
        queue_stats.dropped_chunks,
        queue_stats.dropped_records,
        queue_stats.dropped_kperf_samples,
        queue_stats.max_depth,
        queue_stats.max_age_ns
    );
    let scanner_stats = scanner_queue_counters.stats();
    if scanner_stats.dropped_records > 0 || scanner_stats.dropped_batches > 0 {
        warn!(
            "staxd-client-probe-scanner: lost data batches={} records={} kperf_samples={} max_depth={}",
            scanner_stats.dropped_batches,
            scanner_stats.dropped_records,
            scanner_stats.dropped_kperf_samples,
            scanner_stats.max_depth
        );
    }
    Ok(())
}

const WORKER_QUEUE_CAPACITY: usize = 16;
const PROBE_SCANNER_QUEUE_CAPACITY: usize = 64;
const WORKER_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
const WORKER_ABORT_GRACE: Duration = Duration::from_millis(250);
#[derive(Clone, Copy, Default)]
struct ParserQueueStats {
    dropped_chunks: u64,
    dropped_records: u64,
    dropped_kperf_samples: u64,
    max_depth: u64,
    max_age_ns: u64,
}

#[derive(Clone, Copy, Default)]
struct ParserDropSummary {
    chunks: u64,
    records: u64,
    kperf_samples: u64,
}

impl ParserDropSummary {
    fn add(&mut self, other: Self) {
        self.chunks = self.chunks.saturating_add(other.chunks);
        self.records = self.records.saturating_add(other.records);
        self.kperf_samples = self.kperf_samples.saturating_add(other.kperf_samples);
    }

    fn is_empty(&self) -> bool {
        self.records == 0 && self.chunks == 0 && self.kperf_samples == 0
    }
}

#[derive(Default)]
struct ParserQueueCounters {
    dropped_chunks: AtomicU64,
    dropped_records: AtomicU64,
    dropped_kperf_samples: AtomicU64,
    max_depth: AtomicU64,
    max_age_ns: AtomicU64,
}

impl ParserQueueCounters {
    fn record_drop(&self, drop: ParserDropSummary) {
        self.dropped_chunks
            .fetch_add(drop.chunks, Ordering::Relaxed);
        self.dropped_records
            .fetch_add(drop.records, Ordering::Relaxed);
        self.dropped_kperf_samples
            .fetch_add(drop.kperf_samples, Ordering::Relaxed);
    }

    fn update_max_depth(&self, depth: usize) {
        update_max(&self.max_depth, u64::try_from(depth).unwrap_or(u64::MAX));
    }

    fn update_max_age(&self, age: Duration) {
        update_max(&self.max_age_ns, age.as_nanos() as u64);
    }

    fn stats(&self) -> ParserQueueStats {
        ParserQueueStats {
            dropped_chunks: self.dropped_chunks.load(Ordering::Relaxed),
            dropped_records: self.dropped_records.load(Ordering::Relaxed),
            dropped_kperf_samples: self.dropped_kperf_samples.load(Ordering::Relaxed),
            max_depth: self.max_depth.load(Ordering::Relaxed),
            max_age_ns: self.max_age_ns.load(Ordering::Relaxed),
        }
    }
}

struct ParserQueue {
    tx: flume::Sender<WorkerMsg>,
    drop_rx: flume::Receiver<WorkerMsg>,
    counters: Arc<ParserQueueCounters>,
}

impl ParserQueue {
    fn new(capacity: usize) -> Self {
        let (tx, drop_rx) = flume::bounded(capacity);
        Self {
            tx,
            drop_rx,
            counters: Arc::new(ParserQueueCounters::default()),
        }
    }

    fn receiver(&self) -> flume::Receiver<WorkerMsg> {
        self.drop_rx.clone()
    }

    fn counters(&self) -> Arc<ParserQueueCounters> {
        self.counters.clone()
    }

    fn enqueue_records(&self, records: Arc<Vec<KdBuf>>) {
        self.enqueue_chunk_drop_oldest(WorkerMsg::Batch(OwnedBatch {
            enqueued_at: Instant::now(),
            records,
        }));
    }

    fn enqueue_chunk_drop_oldest(&self, mut msg: WorkerMsg) {
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => {
                    self.counters.update_max_depth(self.drop_rx.len());
                    return;
                }
                Err(flume::TrySendError::Full(returned)) => {
                    msg = returned;
                    match self.drop_rx.try_recv() {
                        Ok(dropped) => self.record_drop(dropped),
                        Err(flume::TryRecvError::Empty) => continue,
                        Err(flume::TryRecvError::Disconnected) => return,
                    }
                }
                Err(flume::TrySendError::Disconnected(_)) => return,
            }
        }
    }

    fn record_drop(&self, msg: WorkerMsg) {
        self.counters.record_drop(drop_summary(msg));
    }
}

fn log_parser_queue_stats(stats: ParserQueueStats) {
    if stats.dropped_records > 0 || stats.dropped_chunks > 0 {
        warn!(
            "staxd-client-worker: parser queue lost data chunks={} records={} kperf_samples={} max_depth={} max_age_ns={}",
            stats.dropped_chunks,
            stats.dropped_records,
            stats.dropped_kperf_samples,
            stats.max_depth,
            stats.max_age_ns
        );
    }
}

fn drain_parser_backlog_for_abort(
    rx: &flume::Receiver<WorkerMsg>,
    counters: &ParserQueueCounters,
) -> ParserDropSummary {
    let mut summary = ParserDropSummary::default();
    while let Ok(msg) = rx.try_recv() {
        summary.add(drop_summary(msg));
    }
    if !summary.is_empty() {
        counters.record_drop(summary);
    }
    summary
}

fn drop_summary(msg: WorkerMsg) -> ParserDropSummary {
    match msg {
        WorkerMsg::Batch(batch) => ParserDropSummary {
            chunks: 1,
            records: batch.records.len() as u64,
            kperf_samples: 0,
        },
    }
}

#[derive(Clone, Copy, Default)]
struct ProbeScannerQueueStats {
    dropped_batches: u64,
    dropped_records: u64,
    dropped_kperf_samples: u64,
    max_depth: u64,
}

#[derive(Default)]
struct ProbeScannerQueueCounters {
    dropped_batches: AtomicU64,
    dropped_records: AtomicU64,
    dropped_kperf_samples: AtomicU64,
    max_depth: AtomicU64,
}

impl ProbeScannerQueueCounters {
    fn record_drop(&self, batch: &ProbeScannerBatch) {
        self.dropped_batches.fetch_add(1, Ordering::Relaxed);
        self.dropped_records
            .fetch_add(batch.records.len() as u64, Ordering::Relaxed);
    }

    fn update_max_depth(&self, depth: usize) {
        update_max(&self.max_depth, u64::try_from(depth).unwrap_or(u64::MAX));
    }

    fn stats(&self) -> ProbeScannerQueueStats {
        ProbeScannerQueueStats {
            dropped_batches: self.dropped_batches.load(Ordering::Relaxed),
            dropped_records: self.dropped_records.load(Ordering::Relaxed),
            dropped_kperf_samples: self.dropped_kperf_samples.load(Ordering::Relaxed),
            max_depth: self.max_depth.load(Ordering::Relaxed),
        }
    }
}

struct ProbeScannerQueue {
    tx: flume::Sender<ProbeScannerBatch>,
    drop_rx: flume::Receiver<ProbeScannerBatch>,
    counters: Arc<ProbeScannerQueueCounters>,
}

impl ProbeScannerQueue {
    fn new(capacity: usize) -> Self {
        let (tx, drop_rx) = flume::bounded(capacity);
        Self {
            tx,
            drop_rx,
            counters: Arc::new(ProbeScannerQueueCounters::default()),
        }
    }

    fn receiver(&self) -> flume::Receiver<ProbeScannerBatch> {
        self.drop_rx.clone()
    }

    fn counters(&self) -> Arc<ProbeScannerQueueCounters> {
        self.counters.clone()
    }

    fn enqueue_batch(&self, mut batch: ProbeScannerBatch) {
        loop {
            match self.tx.try_send(batch) {
                Ok(()) => {
                    self.counters.update_max_depth(self.drop_rx.len());
                    return;
                }
                Err(flume::TrySendError::Full(returned)) => {
                    batch = returned;
                    match self.drop_rx.try_recv() {
                        Ok(dropped) => self.counters.record_drop(&dropped),
                        Err(flume::TryRecvError::Empty) => continue,
                        Err(flume::TryRecvError::Disconnected) => return,
                    }
                }
                Err(flume::TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

struct ProbeScannerBatch {
    records: Arc<Vec<KdBuf>>,
    timing: KperfProbeTriggerTiming,
}

fn probe_scanner_thread<SampleStart>(
    rx: flume::Receiver<ProbeScannerBatch>,
    counters: Arc<ProbeScannerQueueCounters>,
    probe_trigger_count: Arc<AtomicU64>,
    client_start: Instant,
    record_rpc_start: Instant,
    mut on_kperf_sample_start: SampleStart,
) where
    SampleStart: FnMut(u32, KperfProbeTriggerTiming),
{
    info!("staxd-client-probe-scanner: started");
    let mut scanner = KperfProbeTriggerScanner::default();
    let mut first_probe_trigger_logged = false;
    while let Ok(batch) = rx.recv() {
        for rec in batch.records.iter() {
            let Some((tid, ts)) = scanner.feed(rec) else {
                continue;
            };
            let trigger_count = probe_trigger_count
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if !first_probe_trigger_logged {
                first_probe_trigger_logged = true;
                info!(
                    "staxd-client: first kperf probe trigger tid={tid} kperf_ts={ts} trigger_count={} since_client_start={:?} since_record_rpc_spawn={:?}",
                    trigger_count,
                    client_start.elapsed(),
                    record_rpc_start.elapsed()
                );
            }
            let mut timing = batch.timing;
            timing.kperf_ts = ts;
            on_kperf_sample_start(tid, timing);
        }
    }
    let stats = counters.stats();
    info!(
        "staxd-client-probe-scanner: exiting triggers={} dropped_batches={} dropped_records={} dropped_kperf_samples={} max_depth={}",
        probe_trigger_count.load(Ordering::Relaxed),
        stats.dropped_batches,
        stats.dropped_records,
        stats.dropped_kperf_samples,
        stats.max_depth
    );
}

fn update_max(max: &AtomicU64, value: u64) {
    let mut old = max.load(Ordering::Relaxed);
    while value > old {
        match max.compare_exchange_weak(old, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => old = next,
        }
    }
}

async fn join_worker_with_deadline(
    worker_handle: std::thread::JoinHandle<()>,
    abort_worker_backlog: Arc<AtomicBool>,
) -> bool {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("staxd-client-worker-join".to_owned())
        .spawn(move || {
            let result = worker_handle.join();
            let _ = done_tx.send(result);
        });

    if wait_for_worker_join(&done_rx, WORKER_SHUTDOWN_BUDGET).await {
        info!(
            "staxd-client-worker: shutdown drained within budget budget={:?}",
            WORKER_SHUTDOWN_BUDGET
        );
        return true;
    }

    warn!(
        "staxd-client-worker: shutdown exceeded {:?}; requesting backlog drop",
        WORKER_SHUTDOWN_BUDGET
    );
    abort_worker_backlog.store(true, Ordering::Release);

    if wait_for_worker_join(&done_rx, WORKER_ABORT_GRACE).await {
        info!(
            "staxd-client-worker: shutdown completed after abort grace grace={:?}",
            WORKER_ABORT_GRACE
        );
        true
    } else {
        warn!(
            "staxd-client-worker: still blocked after abort grace {:?}; detaching worker so recorder can exit",
            WORKER_ABORT_GRACE
        );
        false
    }
}

async fn wait_for_worker_join(
    done_rx: &std::sync::mpsc::Receiver<std::thread::Result<()>>,
    budget: Duration,
) -> bool {
    let started = Instant::now();
    loop {
        match done_rx.try_recv() {
            Ok(Ok(())) => return true,
            Ok(Err(_panic)) => {
                warn!("staxd-client-worker: parser worker panicked during shutdown");
                return true;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if started.elapsed() >= budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Default)]
struct KperfProbeTriggerScanner {
    pending: Option<PendingKperfSample>,
}

struct PendingKperfSample {
    tid: u32,
    timestamp: u64,
    triggered: bool,
}

impl KperfProbeTriggerScanner {
    fn feed(&mut self, rec: &KdBuf) -> Option<(u32, u64)> {
        if kdebug::kdbg_class(rec.debugid) != DBG_PERF {
            return None;
        }

        let subclass = kdebug::kdbg_subclass(rec.debugid);
        let code = kdebug::kdbg_code(rec.debugid);
        let func = kdebug::kdbg_func(rec.debugid);

        match (subclass, code, func) {
            (perf::sc::GENERIC, 0, DBG_FUNC_START) => {
                self.pending = Some(PendingKperfSample {
                    tid: rec.arg5 as u32,
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
                Some((pending.tid, pending.timestamp))
            }
            _ => None,
        }
    }
}

/// Owned, thread-Send'able mirror of `KdBufBatch`. We can't move
/// the SelfRef across thread boundaries, so the recv loop pulls
/// the records out and ships them to the worker via this struct.
struct OwnedBatch {
    enqueued_at: Instant,
    records: Arc<Vec<KdBuf>>,
}

enum WorkerMsg {
    Batch(OwnedBatch),
}

/// Dedicated OS thread that owns the Pipeline + Sink. Drives all
/// the synchronous work: parser, periodic libproc scans (with
/// their fs::read + Mach-O parsing). Loops on `recv_timeout` so
/// periodic ticks fire even when no batches are arriving.
fn worker_thread<S: SampleSink>(
    config: PipelineConfig,
    shared_cache: Option<Arc<stax_mac_shared_cache::SharedCache>>,
    mut sink: S,
    abort_backlog: Arc<AtomicBool>,
    rx: flume::Receiver<WorkerMsg>,
    counters: Arc<ParserQueueCounters>,
) {
    let worker_start = Instant::now();
    info!(
        "staxd-client-worker: starting parser pipeline pid={} frequency_hz={} has_shared_cache={}",
        config.pid,
        config.frequency_hz,
        shared_cache.is_some()
    );
    let phase_start = Instant::now();
    let mut pipeline = Pipeline::new(config, shared_cache, &mut sink);
    info!(
        "staxd-client-worker: parser pipeline ready elapsed={:?}",
        phase_start.elapsed()
    );

    // Tick cadence — drives image / thread-name / jitdump
    // rescans. Lives here, not on the recv loop.
    const TICK_INTERVAL: Duration = Duration::from_millis(50);
    const SLOW_PROCESS_RECORDS: Duration = Duration::from_millis(10);
    const SLOW_TICK: Duration = Duration::from_millis(10);
    let mut processed_batches: u64 = 0;
    let mut processed_records: u64 = 0;
    let mut first_batch_logged = false;
    let mut aborted = false;

    loop {
        if abort_backlog.load(Ordering::Acquire) {
            let dropped = drain_parser_backlog_for_abort(&rx, &counters);
            info!(
                "staxd-client-worker: shutdown requested; dropped queued parser backlog chunks={} records={} kperf_samples={}",
                dropped.chunks, dropped.records, dropped.kperf_samples
            );
            aborted = true;
            break;
        }
        match rx.recv_timeout(TICK_INTERVAL) {
            Ok(WorkerMsg::Batch(batch)) => {
                if abort_backlog.load(Ordering::Acquire) {
                    let dropped = drain_parser_backlog_for_abort(&rx, &counters);
                    info!(
                        "staxd-client-worker: shutdown requested; dropped queued parser backlog chunks={} records={} kperf_samples={}",
                        dropped.chunks, dropped.records, dropped.kperf_samples
                    );
                    aborted = true;
                    break;
                }
                counters.update_max_age(batch.enqueued_at.elapsed());
                let kdbufs = batch.records;
                if !first_batch_logged {
                    first_batch_logged = true;
                    info!(
                        "staxd-client-worker: first kdebug batch queued to parser records={} first_ts={} last_ts={} since_worker_start={:?}",
                        kdbufs.len(),
                        kdbufs.first().map(|rec| rec.timestamp).unwrap_or(0),
                        kdbufs.last().map(|rec| rec.timestamp).unwrap_or(0),
                        worker_start.elapsed()
                    );
                }
                let phase_start = Instant::now();
                pipeline.process_records(&kdbufs, &mut sink);
                let elapsed = phase_start.elapsed();
                processed_batches += 1;
                processed_records += kdbufs.len() as u64;
                if elapsed >= SLOW_PROCESS_RECORDS {
                    info!(
                        "staxd-client-worker: slow process_records elapsed={elapsed:?} records={} processed_batches={} processed_records={}",
                        kdbufs.len(),
                        processed_batches,
                        processed_records
                    );
                }
            }
            Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
        // Periodic libproc scans — sync, may do disk IO. That's
        // fine *here*: this thread is dedicated, so a slow scan
        // can't block recv or vox.
        let phase_start = Instant::now();
        pipeline.tick(&mut sink);
        let elapsed = phase_start.elapsed();
        if elapsed >= SLOW_TICK {
            info!(
                "staxd-client-worker: slow pipeline tick elapsed={elapsed:?} processed_batches={} processed_records={}",
                processed_batches, processed_records
            );
        }
    }

    if aborted {
        info!(
            "staxd-client-worker: parser pipeline aborted without finish total_elapsed={:?} processed_batches={} processed_records={}",
            worker_start.elapsed(),
            processed_batches,
            processed_records
        );
        return;
    }

    if abort_backlog.load(Ordering::Acquire) {
        let dropped = drain_parser_backlog_for_abort(&rx, &counters);
        info!(
            "staxd-client-worker: shutdown requested before finish; skipped finish chunks={} records={} kperf_samples={}",
            dropped.chunks, dropped.records, dropped.kperf_samples
        );
        return;
    }

    let phase_start = Instant::now();
    pipeline.finish(&mut sink);
    info!(
        "staxd-client-worker: parser pipeline finished elapsed={:?} total_elapsed={:?} processed_batches={} processed_records={}",
        phase_start.elapsed(),
        worker_start.elapsed(),
        processed_batches,
        processed_records
    );
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
    if later <= earlier {
        return 0;
    }
    let (numer, denom) = mach_timebase_numer_denom();
    (((later - earlier) as u128) * u128::from(numer) / u128::from(denom)).min(u128::from(u64::MAX))
        as u64
}

fn mach_timebase_numer_denom() -> (u32, u32) {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        let rc = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        if rc == 0 && info.numer != 0 && info.denom != 0 {
            (info.numer, info.denom)
        } else {
            (1, 1)
        }
    })
}
