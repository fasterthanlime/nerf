//! Forward `LiveSink` events to `stax-server` over a vox local
//! socket. Async-trait callbacks are intentionally tiny: each one
//! pushes an owned `IngestEvent` into a sync-friendly tokio mpsc
//! and returns immediately. A separate forwarder task drains the
//! mpsc and pumps events through `vox::Tx::send` at whatever rate
//! the wire allows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stax_live_proto::{
    IngestEvent, RunIngestClient, WireBinaryLoaded, WireMachOSymbol, WireOffCpuInterval,
    WireOnCpuInterval, WireSampleEvent, WireWakeup,
};
use tokio::sync::mpsc::{self, Sender};

use crate::live_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, CpuIntervalEvent, CpuIntervalKind, LiveSink,
    SampleEvent, TargetAttached, ThreadName, WakeupEvent,
};

#[cfg(target_os = "macos")]
use crate::live_sink::MachOByteSource;

/// `LiveSink` impl that drops every event into a channel which a
/// forwarder task drains and pushes into a vox `Tx<IngestEvent>`.
///
/// `stop_requested` flips to `true` when the forwarder sees the
/// vox `Tx` reject a send — typically because stax-server dropped
/// its `Rx<IngestEvent>` after a `RunControl::stop_active`. The
/// recorder loop polls `LiveSink::stop_requested()` to break out
/// of `drive_session` cleanly.
pub struct IngestSink {
    tx: Sender<IngestEvent>,
    stop_requested: Arc<AtomicBool>,
}

impl IngestSink {
    pub fn new(tx: Sender<IngestEvent>, stop_requested: Arc<AtomicBool>) -> Self {
        Self { tx, stop_requested }
    }
}

#[async_trait::async_trait]
impl LiveSink for IngestSink {
    fn stop_flag(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop_requested.clone())
    }

    async fn on_sample(&self, ev: &SampleEvent) {
        let user_backtrace = ev.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self
            .tx
            .send(IngestEvent::Sample(WireSampleEvent {
                timestamp_ns: ev.timestamp,
                pid: ev.pid,
                tid: ev.tid,
                kernel_backtrace: ev.kernel_backtrace.to_vec(),
                user_backtrace,
                cycles: ev.cycles,
                instructions: ev.instructions,
                l1d_misses: ev.l1d_misses,
                branch_mispreds: ev.branch_mispreds,
            }))
            .await;
    }

    async fn on_target_attached(&self, ev: &TargetAttached) {
        let _ = self
            .tx
            .send(IngestEvent::TargetAttached {
                pid: ev.pid,
                task_port: ev.task_port,
            })
            .await;
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
        let _ = self
            .tx
            .send(IngestEvent::BinaryLoaded(WireBinaryLoaded {
                path: ev.path.to_owned(),
                base_avma: ev.base_avma,
                vmsize: ev.vmsize,
                text_svma: ev.text_svma,
                arch: ev.arch.map(|s| s.to_owned()),
                is_executable: ev.is_executable,
                symbols,
                text_bytes: ev.text_bytes.map(|b| b.to_vec()),
            }))
            .await;
    }

    async fn on_binary_unloaded(&self, ev: &BinaryUnloadedEvent) {
        let _ = self
            .tx
            .send(IngestEvent::BinaryUnloaded {
                path: ev.path.to_owned(),
                base_avma: ev.base_avma,
            })
            .await;
    }

    async fn on_thread_name(&self, ev: &ThreadName) {
        let _ = self
            .tx
            .send(IngestEvent::ThreadName {
                pid: ev.pid,
                tid: ev.tid,
                name: ev.name.to_owned(),
            })
            .await;
    }

    async fn on_wakeup(&self, ev: &WakeupEvent) {
        let _ = self
            .tx
            .send(IngestEvent::Wakeup(WireWakeup {
                timestamp_ns: ev.timestamp,
                waker_tid: ev.waker_tid,
                wakee_tid: ev.wakee_tid,
                waker_user_stack: ev.waker_user_stack.to_vec(),
                waker_kernel_stack: ev.waker_kernel_stack.to_vec(),
            }))
            .await;
    }

    async fn on_probe_result<'a>(&self, ev: &crate::live_sink::ProbeResultEvent<'a>) {
        let _ = self
            .tx
            .send(IngestEvent::ProbeResult(stax_live_proto::WireProbeResult {
                tid: ev.tid,
                kperf_ts_ns: ev.kperf_ts,
                probe_done_ns: ev.probe_done_ns,
                mach_pc: ev.mach_pc,
                mach_lr: ev.mach_lr,
                mach_fp: ev.mach_fp,
                mach_sp: ev.mach_sp,
                mach_walked: ev.mach_walked.to_vec(),
                used_framehop: ev.used_framehop,
            }))
            .await;
    }

    async fn on_cpu_interval(&self, ev: &CpuIntervalEvent) {
        match &ev.kind {
            CpuIntervalKind::OnCpu => {
                let _ = self
                    .tx
                    .send(IngestEvent::OnCpuInterval(WireOnCpuInterval {
                        tid: ev.tid,
                        start_ns: ev.start_ns,
                        end_ns: ev.end_ns,
                    }))
                    .await;
            }
            CpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                let _ = self
                    .tx
                    .send(IngestEvent::OffCpuInterval(WireOffCpuInterval {
                        tid: ev.tid,
                        start_ns: ev.start_ns,
                        end_ns: ev.end_ns,
                        stack: stack.iter().map(|f| f.address).collect(),
                        waker_tid: *waker_tid,
                        waker_user_stack: waker_user_stack.map(|s| s.to_vec()),
                    }))
                    .await;
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

/// Connect to stax-server, register a run, return:
///   - the assigned `RunId`
///   - a `LiveSink` to hand to the recorder
///   - a join handle that resolves once the forwarder task drains
///     the channel and closes the vox Tx.
pub async fn connect_and_register(
    server_socket: &str,
    config: stax_live_proto::RunConfig,
) -> eyre::Result<(stax_live_proto::RunId, IngestSink, tokio::task::JoinHandle<()>)> {
    let url = format!("local://{server_socket}");
    let client: RunIngestClient = vox::connect(&url).await?;

    let (vox_tx, vox_rx) = vox::channel::<IngestEvent>();
    let run_id = match client.start_run(config, vox_rx).await {
        Ok(id) => id,
        Err(vox::VoxError::User(msg)) => {
            return Err(eyre::eyre!("server rejected start_run: {msg}"));
        }
        Err(e) => return Err(eyre::eyre!("vox start_run failed: {e:?}")),
    };

    // Bounded so the worker thread feels real backpressure when
    // the server falls behind. Unbounded was the source of the
    // multi-second "flushing samples to stax-server" wait at
    // end-of-recording: we'd buffer ~1M IngestEvents in here and
    // then have to drain them all serially after the recording
    // had stopped. With a cap, the worker's block_on(send) blocks
    // when the server is the bottleneck, recording slows to match
    // server throughput, and the end-of-recording flush is bounded
    // by INGEST_QUEUE_CAP × per-event vox time.
    const INGEST_QUEUE_CAP: usize = 16_384;
    let (sync_tx, mut sync_rx) = mpsc::channel::<IngestEvent>(INGEST_QUEUE_CAP);
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_for_forwarder = stop_requested.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = sync_rx.recv().await {
            if vox_tx.send(event).await.is_err() {
                // Server dropped its Rx (most likely
                // stop_active fired). Tell the recorder loop to
                // bail out via LiveSink::stop_requested.
                stop_for_forwarder.store(true, Ordering::Relaxed);
                break;
            }
        }
        let _ = vox_tx.close(Default::default()).await;
        // Cover the cases where sync_rx closed for any other
        // reason — the recorder should stop either way.
        stop_for_forwarder.store(true, Ordering::Relaxed);
    });

    Ok((run_id, IngestSink::new(sync_tx, stop_requested), forwarder))
}
