//! Forward `LiveSink` events to `stax-server` over a vox local
//! socket. Async-trait callbacks are intentionally tiny: each one
//! pushes an owned `IngestEvent` into a sync-friendly tokio mpsc
//! and returns immediately. A separate forwarder task drains the
//! mpsc and pumps batches through `vox::Tx::send` at whatever rate
//! the wire allows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use stax_live_proto::{
    IngestBatch, IngestEvent, RunId, RunIngestClient, WireBinaryLoaded, WireBinaryUnloaded,
    WireMachOSymbol, WireOffCpuInterval, WireOnCpuInterval, WireSampleEvent, WireWakeup,
};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::live_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, CpuIntervalEvent, CpuIntervalKind, LiveSink,
    SampleEvent, TargetAttached, ThreadName, WakeupEvent,
};

#[cfg(target_os = "macos")]
use crate::live_sink::MachOByteSource;

const INGEST_CHANNEL_CAPACITY: u32 = 64;
const INGEST_BATCH_MAX_EVENTS: usize = 1024;
const INGEST_BATCH_MAX_DELAY: Duration = Duration::from_millis(5);

/// `LiveSink` impl that drops every event into a channel which a
/// forwarder task drains and pushes into a vox `Tx<IngestBatch>`.
///
/// `stop_requested` flips to `true` when the forwarder sees the
/// vox `Tx` reject a send — typically because stax-server dropped
/// its `Rx<IngestBatch>` after a `RunControl::stop_active`. The
/// recorder loop polls `LiveSink::stop_requested()` to break out
/// of `drive_session` cleanly.
pub struct IngestSink {
    tx: UnboundedSender<IngestEvent>,
    reliable_tx: std::sync::mpsc::Sender<ReliableIngest>,
    stop_requested: Arc<AtomicBool>,
    enqueued: Arc<AtomicU64>,
}

impl IngestSink {
    fn new(
        tx: UnboundedSender<IngestEvent>,
        reliable_tx: std::sync::mpsc::Sender<ReliableIngest>,
        stop_requested: Arc<AtomicBool>,
        enqueued: Arc<AtomicU64>,
    ) -> Self {
        Self {
            tx,
            reliable_tx,
            stop_requested,
            enqueued,
        }
    }

    fn enqueue_event(&self, event: IngestEvent) {
        let kind = ingest_event_kind(&event);
        let queued = self.enqueued.fetch_add(1, Ordering::Relaxed) + 1;
        if queued == 1 {
            tracing::info!(kind, queued, "ingest_sink: first event enqueued");
        } else if queued.is_multiple_of(1024) {
            tracing::debug!(kind, queued, "ingest_sink: events enqueued");
        }
        if let Err(err) = self.tx.send(event) {
            tracing::warn!(
                kind,
                queued,
                error = ?err,
                "ingest_sink: event enqueue failed; forwarder channel is closed"
            );
            self.stop_requested.store(true, Ordering::Relaxed);
        }
    }

    fn enqueue_reliable(&self, msg: ReliableIngestMsg) {
        if self.reliable_tx.send(ReliableIngest { msg }).is_err() {
            self.stop_requested.store(true, Ordering::Relaxed);
        }
    }
}

#[async_trait::async_trait]
impl LiveSink for IngestSink {
    fn stop_flag(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop_requested.clone())
    }

    async fn on_sample(&self, ev: &SampleEvent) {
        let user_backtrace = ev.user_backtrace.iter().map(|f| f.address).collect();
        self.enqueue_event(IngestEvent::Sample(WireSampleEvent {
            timestamp_ns: ev.timestamp,
            pid: ev.pid,
            tid: ev.tid,
            kernel_backtrace: ev.kernel_backtrace.to_vec(),
            user_backtrace,
            cycles: ev.cycles,
            instructions: ev.instructions,
            l1d_misses: ev.l1d_misses,
            branch_mispreds: ev.branch_mispreds,
        }));
    }

    async fn on_target_attached(&self, ev: &TargetAttached) {
        self.enqueue_reliable(ReliableIngestMsg::TargetAttached {
            pid: ev.pid,
            task_port: ev.task_port,
        });
    }

    async fn on_binary_loaded(&self, ev: &BinaryLoadedEvent) {
        let symbols = ev
            .symbols
            .iter()
            .map(|s| WireMachOSymbol {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name.to_vec(),
            })
            .collect();
        self.enqueue_reliable(ReliableIngestMsg::BinaryLoaded(WireBinaryLoaded {
            path: ev.path.to_owned(),
            base_avma: ev.base_avma,
            vmsize: ev.vmsize,
            text_svma: ev.text_svma,
            arch: ev.arch.map(|s| s.to_owned()),
            is_executable: ev.is_executable,
            symbols,
            text_bytes: ev.text_bytes.map(|b| b.to_vec()),
        }));
    }

    async fn on_binary_unloaded(&self, ev: &BinaryUnloadedEvent) {
        self.enqueue_reliable(ReliableIngestMsg::BinaryUnloaded(WireBinaryUnloaded {
            path: ev.path.to_owned(),
            base_avma: ev.base_avma,
        }));
    }

    async fn on_thread_name(&self, ev: &ThreadName) {
        self.enqueue_event(IngestEvent::ThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: ev.name.to_owned(),
        });
    }

    async fn on_wakeup(&self, ev: &WakeupEvent) {
        self.enqueue_event(IngestEvent::Wakeup(WireWakeup {
            timestamp_ns: ev.timestamp,
            waker_tid: ev.waker_tid,
            wakee_tid: ev.wakee_tid,
            waker_user_stack: ev.waker_user_stack.to_vec(),
            waker_kernel_stack: ev.waker_kernel_stack.to_vec(),
        }));
    }

    async fn on_probe_result<'a>(&self, ev: &crate::live_sink::ProbeResultEvent<'a>) {
        self.enqueue_event(IngestEvent::ProbeResult(stax_live_proto::WireProbeResult {
            tid: ev.tid,
            timing: ev.timing.into(),
            queue: ev.queue.into(),
            mach_pc: ev.mach_pc,
            mach_lr: ev.mach_lr,
            mach_fp: ev.mach_fp,
            mach_sp: ev.mach_sp,
            mach_walked: ev.mach_walked.to_vec(),
            compact_walked: ev.compact_walked.to_vec(),
            compact_dwarf_walked: ev.compact_dwarf_walked.to_vec(),
            dwarf_walked: ev.dwarf_walked.to_vec(),
            used_framehop: ev.used_framehop,
        }));
    }

    async fn on_cpu_interval(&self, ev: &CpuIntervalEvent) {
        match &ev.kind {
            CpuIntervalKind::OnCpu => {
                self.enqueue_event(IngestEvent::OnCpuInterval(WireOnCpuInterval {
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                }));
            }
            CpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                self.enqueue_event(IngestEvent::OffCpuInterval(WireOffCpuInterval {
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    stack: stack.iter().map(|f| f.address).collect(),
                    waker_tid: *waker_tid,
                    waker_user_stack: waker_user_stack.map(|s| s.to_vec()),
                }));
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn on_macho_byte_source(&self, _source: Arc<dyn MachOByteSource>) {
        // The shared-cache mmap can't cross the vox boundary as an
        // Arc<dyn Trait>; the server will open it itself by path
        // (follow-up). For now, drop silently.
        let _ = _source;
    }
}

impl From<crate::live_sink::ProbeTiming> for stax_live_proto::ProbeTiming {
    fn from(t: crate::live_sink::ProbeTiming) -> Self {
        Self {
            kperf_ts: t.kperf_ts,
            staxd_read_started: t.staxd_read_started,
            staxd_drained: t.staxd_drained,
            staxd_queued_for_send: t.staxd_queued_for_send,
            staxd_send_started: t.staxd_send_started,
            client_received: t.client_received,
            enqueued: t.enqueued,
            worker_started: t.worker_started,
            thread_lookup_done: t.thread_lookup_done,
            state_done: t.state_done,
            resume_done: t.resume_done,
            walk_done: t.walk_done,
        }
    }
}

impl From<crate::live_sink::ProbeQueueStats> for stax_live_proto::ProbeQueueStats {
    fn from(q: crate::live_sink::ProbeQueueStats) -> Self {
        Self {
            coalesced_requests: q.coalesced_requests,
            worker_batch_len: q.worker_batch_len,
        }
    }
}

/// Connect to stax-server, register a run, return:
///   - the assigned `RunId`
///   - a `LiveSink` to hand to the recorder
///   - a join handle that resolves once the forwarder task drains
///     the channel and closes the vox Tx.
pub async fn connect_and_register(
    server_socket: &str,
    config: stax_live_proto::RunConfig,
) -> eyre::Result<(
    stax_live_proto::RunId,
    IngestSink,
    tokio::task::JoinHandle<()>,
)> {
    connect_and_register_with_telemetry(server_socket, config, None).await
}

pub async fn connect_and_register_with_telemetry(
    server_socket: &str,
    config: stax_live_proto::RunConfig,
    telemetry: Option<metrix::TelemetryRegistry>,
) -> eyre::Result<(
    stax_live_proto::RunId,
    IngestSink,
    tokio::task::JoinHandle<()>,
)> {
    let url = format!("local://{server_socket}");
    let mut observer = stax_vox_observe::VoxObserverLogger::new("ingest-sink", "start_run");
    if let Some(telemetry) = telemetry {
        observer = observer.with_telemetry(telemetry);
    }
    let client: RunIngestClient = vox::connect(&url)
        .channel_capacity(INGEST_CHANNEL_CAPACITY)
        .observer(observer)
        .await?;
    let client = client.with_middleware(vox::ClientLogging::default());

    let (vox_tx, vox_rx) = vox::channel::<IngestBatch>();
    let run_id = match client.start_run(config, vox_rx).await {
        Ok(id) => id,
        Err(vox::VoxError::User(err)) => {
            return Err(eyre::eyre!("server rejected start_run: {err:?}"));
        }
        Err(e) => return Err(eyre::eyre!("vox start_run failed: {e:?}")),
    };

    let (sync_tx, sync_rx) = mpsc::unbounded_channel::<IngestEvent>();
    let (reliable_tx, reliable_rx) = std::sync::mpsc::channel::<ReliableIngest>();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let enqueued = Arc::new(AtomicU64::new(0));
    let forwarder = spawn_forwarders(
        client,
        "start_run",
        run_id,
        vox_tx,
        sync_rx,
        reliable_rx,
        stop_requested.clone(),
        enqueued.clone(),
    );

    Ok((
        run_id,
        IngestSink::new(sync_tx, reliable_tx, stop_requested, enqueued),
        forwarder,
    ))
}

/// Connect an ingest channel to a run that stax-server already
/// created via RunControl. Used by stax-shade in the server-owned
/// lifecycle path.
pub async fn connect_to_existing_run(
    server_socket: &str,
    run_id: stax_live_proto::RunId,
) -> eyre::Result<(IngestSink, tokio::task::JoinHandle<()>)> {
    connect_to_existing_run_with_telemetry(server_socket, run_id, None).await
}

pub async fn connect_to_existing_run_with_telemetry(
    server_socket: &str,
    run_id: stax_live_proto::RunId,
    telemetry: Option<metrix::TelemetryRegistry>,
) -> eyre::Result<(IngestSink, tokio::task::JoinHandle<()>)> {
    let url = format!("local://{server_socket}");
    let mut observer = stax_vox_observe::VoxObserverLogger::new("ingest-sink", "attach_run");
    if let Some(telemetry) = telemetry {
        observer = observer.with_telemetry(telemetry);
    }
    let client: RunIngestClient = vox::connect(&url)
        .channel_capacity(INGEST_CHANNEL_CAPACITY)
        .observer(observer)
        .await?;
    let client = client.with_middleware(vox::ClientLogging::default());

    let (vox_tx, vox_rx) = vox::channel::<IngestBatch>();
    match client.attach_run(run_id, vox_rx).await {
        Ok(()) => {}
        Err(vox::VoxError::User(err)) => {
            return Err(eyre::eyre!("server rejected attach_run: {err:?}"));
        }
        Err(e) => return Err(eyre::eyre!("vox attach_run failed: {e:?}")),
    }

    let (sync_tx, sync_rx) = mpsc::unbounded_channel::<IngestEvent>();
    let (reliable_tx, reliable_rx) = std::sync::mpsc::channel::<ReliableIngest>();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let enqueued = Arc::new(AtomicU64::new(0));
    let forwarder = spawn_forwarders(
        client,
        "attach_run",
        run_id,
        vox_tx,
        sync_rx,
        reliable_rx,
        stop_requested.clone(),
        enqueued.clone(),
    );

    Ok((
        IngestSink::new(sync_tx, reliable_tx, stop_requested, enqueued),
        forwarder,
    ))
}

fn spawn_forwarders(
    client: RunIngestClient,
    surface: &'static str,
    run_id: RunId,
    vox_tx: vox::Tx<IngestBatch>,
    sync_rx: mpsc::UnboundedReceiver<IngestEvent>,
    reliable_rx: std::sync::mpsc::Receiver<ReliableIngest>,
    stop_requested: Arc<AtomicBool>,
    enqueued: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    let debug_registration = stax_vox_observe::register_global_caller(
        "ingest-sink",
        surface,
        "RunIngest",
        &client.caller,
    );
    let event_stop = stop_requested.clone();
    let event_forwarder = tokio::spawn(forward_events(vox_tx, sync_rx, event_stop, enqueued));
    let reliable_stop = stop_requested.clone();
    let reliable_forwarder = tokio::task::spawn_blocking(move || {
        tracing::info!(run_id = run_id.0, "ingest_sink: reliable forwarder started");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("reliable ingest runtime");
        for request in reliable_rx {
            let result = rt.block_on(async {
                match request.msg {
                    ReliableIngestMsg::TargetAttached { pid, task_port } => client
                        .publish_target_attached(run_id, pid, task_port)
                        .await
                        .map_err(|e| format!("{e:?}"))?,
                    ReliableIngestMsg::BinaryLoaded(binary) => client
                        .publish_binaries_loaded(run_id, vec![binary])
                        .await
                        .map_err(|e| format!("{e:?}"))?,
                    ReliableIngestMsg::BinaryUnloaded(binary) => client
                        .publish_binaries_unloaded(run_id, vec![binary])
                        .await
                        .map_err(|e| format!("{e:?}"))?,
                }
                Ok::<(), String>(())
            });
            if result.is_err() {
                tracing::warn!(
                    run_id = run_id.0,
                    error = ?result,
                    "ingest_sink: reliable ingest call failed"
                );
                reliable_stop.store(true, Ordering::Relaxed);
            }
        }
        tracing::info!(run_id = run_id.0, "ingest_sink: reliable forwarder exiting");
    });
    tokio::spawn(async move {
        let _debug_registration = debug_registration;
        let _ = event_forwarder.await;
        let _ = reliable_forwarder.await;
    })
}

async fn forward_events(
    vox_tx: vox::Tx<IngestBatch>,
    mut sync_rx: mpsc::UnboundedReceiver<IngestEvent>,
    stop_requested: Arc<AtomicBool>,
    enqueued: Arc<AtomicU64>,
) {
    let mut forwarded: u64 = 0;
    let mut batches: u64 = 0;
    let mut counts = ForwardCounts::default();
    let mut last_log = Instant::now();
    tracing::info!("ingest_sink: event forwarder started");
    'events: while let Some(first_event) = sync_rx.recv().await {
        let first_kind = ingest_event_kind(&first_event);
        let queued_total = enqueued.load(Ordering::Relaxed);
        if forwarded == 0 {
            tracing::info!(
                kind = first_kind,
                queued = sync_rx.len(),
                queued_total,
                "ingest_sink: forwarder received first event"
            );
        }
        let mut batch = Vec::with_capacity(INGEST_BATCH_MAX_EVENTS.min(sync_rx.len() + 1));
        batch.push(first_event);
        let batch_started = Instant::now();
        let mut input_closed = false;
        while batch.len() < INGEST_BATCH_MAX_EVENTS {
            match sync_rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    input_closed = true;
                    break;
                }
            }
        }
        while batch.len() < INGEST_BATCH_MAX_EVENTS {
            let Some(remaining) = INGEST_BATCH_MAX_DELAY.checked_sub(batch_started.elapsed())
            else {
                break;
            };
            match tokio::time::timeout(remaining, sync_rx.recv()).await {
                Ok(Some(event)) => {
                    batch.push(event);
                    while batch.len() < INGEST_BATCH_MAX_EVENTS {
                        match sync_rx.try_recv() {
                            Ok(event) => batch.push(event),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                input_closed = true;
                                break;
                            }
                        }
                    }
                    if input_closed {
                        break;
                    }
                }
                Ok(None) => {
                    input_closed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        let mut batch_counts = ForwardCounts::default();
        for event in &batch {
            batch_counts.record(event);
        }
        let batch_len = batch.len() as u64;
        let send_start = Instant::now();
        match vox_tx.send(IngestBatch { events: batch }).await {
            Ok(()) => {
                forwarded = forwarded.saturating_add(batch_len);
                batches = batches.saturating_add(1);
                counts.merge(&batch_counts);
                let send_elapsed = send_start.elapsed();
                if batches == 1 {
                    tracing::info!(
                        kind = first_kind,
                        forwarded,
                        batches,
                        batch_len,
                        queued = sync_rx.len(),
                        queued_total,
                        elapsed = ?send_elapsed,
                        "ingest_sink: forwarder sent first batch"
                    );
                } else if send_elapsed >= Duration::from_millis(10) {
                    tracing::warn!(
                        kind = first_kind,
                        forwarded,
                        batches,
                        batch_len,
                        queued = sync_rx.len(),
                        queued_total,
                        elapsed = ?send_elapsed,
                        "ingest_sink: slow vox batch send"
                    );
                }
                if last_log.elapsed() >= Duration::from_secs(2) {
                    tracing::info!(
                        forwarded,
                        batches,
                        queued = sync_rx.len(),
                        queued_total,
                        counts = %counts.summary(),
                        "ingest_sink: forwarder progress: forwarded={} batches={} queued={} {}",
                        forwarded,
                        batches,
                        sync_rx.len(),
                        counts.summary(),
                    );
                    last_log = std::time::Instant::now();
                }
                if input_closed {
                    break 'events;
                }
            }
            Err(e) => {
                tracing::warn!(
                    kind = first_kind,
                    forwarded,
                    batches,
                    batch_len,
                    queued = sync_rx.len(),
                    queued_total,
                    error = ?e,
                    "ingest_sink: vox batch send failed (server dropped Rx?) after forwarded={} batches={} queued={} err={:?}",
                    forwarded,
                    batches,
                    sync_rx.len(),
                    e
                );
                stop_requested.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    tracing::info!(
        forwarded,
        batches,
        queued_total = enqueued.load(Ordering::Relaxed),
        counts = %counts.summary(),
        "ingest_sink: forwarder exiting (sync_rx closed) after forwarded={} batches={} {}; flushing vox",
        forwarded,
        batches,
        counts.summary(),
    );
    let _ = vox_tx.close(Default::default()).await;
    stop_requested.store(true, Ordering::Relaxed);
}

fn ingest_event_kind(event: &IngestEvent) -> &'static str {
    match event {
        IngestEvent::Sample(_) => "sample",
        IngestEvent::ProbeResult(_) => "probe_result",
        IngestEvent::OnCpuInterval(_) => "on_cpu",
        IngestEvent::OffCpuInterval(_) => "off_cpu",
        IngestEvent::BinaryLoaded(_) => "binary_loaded",
        IngestEvent::BinaryUnloaded(_) => "binary_unloaded",
        IngestEvent::TargetAttached { .. } => "target_attached",
        IngestEvent::ThreadName { .. } => "thread_name",
        IngestEvent::Wakeup(_) => "wakeup",
    }
}

struct ReliableIngest {
    msg: ReliableIngestMsg,
}

enum ReliableIngestMsg {
    TargetAttached { pid: u32, task_port: u64 },
    BinaryLoaded(WireBinaryLoaded),
    BinaryUnloaded(WireBinaryUnloaded),
}

#[derive(Default)]
struct ForwardCounts {
    samples: u64,
    probe_results: u64,
    on_cpu: u64,
    off_cpu: u64,
    binaries_loaded: u64,
    binaries_unloaded: u64,
    target_attached: u64,
    thread_names: u64,
    wakeups: u64,
}

impl ForwardCounts {
    fn merge(&mut self, other: &Self) {
        self.samples += other.samples;
        self.probe_results += other.probe_results;
        self.on_cpu += other.on_cpu;
        self.off_cpu += other.off_cpu;
        self.binaries_loaded += other.binaries_loaded;
        self.binaries_unloaded += other.binaries_unloaded;
        self.target_attached += other.target_attached;
        self.thread_names += other.thread_names;
        self.wakeups += other.wakeups;
    }

    fn record(&mut self, event: &IngestEvent) {
        match event {
            IngestEvent::Sample(_) => self.samples += 1,
            IngestEvent::ProbeResult(_) => self.probe_results += 1,
            IngestEvent::OnCpuInterval(_) => self.on_cpu += 1,
            IngestEvent::OffCpuInterval(_) => self.off_cpu += 1,
            IngestEvent::BinaryLoaded(_) => self.binaries_loaded += 1,
            IngestEvent::BinaryUnloaded(_) => self.binaries_unloaded += 1,
            IngestEvent::TargetAttached { .. } => self.target_attached += 1,
            IngestEvent::ThreadName { .. } => self.thread_names += 1,
            IngestEvent::Wakeup(_) => self.wakeups += 1,
        }
    }

    fn summary(&self) -> String {
        format!(
            "samples={} probes={} on_cpu={} off_cpu={} bin_load={} bin_unload={} target={} threads={} wakeups={}",
            self.samples,
            self.probe_results,
            self.on_cpu,
            self.off_cpu,
            self.binaries_loaded,
            self.binaries_unloaded,
            self.target_attached,
            self.thread_names,
            self.wakeups,
        )
    }
}
