//! Forward `LiveSink` events to `stax-server` over a vox local
//! socket. The recorder's `LiveSink` trait is sync (callbacks fire
//! from the privileged-side drain loop); vox `Tx::send` is async, so
//! we bridge through a tokio mpsc::UnboundedSender that a forwarder
//! task drains.

use std::sync::Arc;

use stax_live_proto::{
    IngestEvent, RunIngestClient, WireBinaryLoaded, WireMachOSymbol, WireOffCpuInterval,
    WireOnCpuInterval, WireSampleEvent, WireWakeup,
};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::live_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, CpuIntervalEvent, CpuIntervalKind, LiveSink,
    SampleEvent, TargetAttached, ThreadName, WakeupEvent,
};

#[cfg(target_os = "macos")]
use crate::live_sink::MachOByteSource;

/// `LiveSink` impl that drops every event into a channel which a
/// forwarder task drains and pushes into a vox `Tx<IngestEvent>`.
pub struct IngestSink {
    tx: UnboundedSender<IngestEvent>,
}

impl IngestSink {
    pub fn new(tx: UnboundedSender<IngestEvent>) -> Self {
        Self { tx }
    }
}

impl LiveSink for IngestSink {
    fn on_sample(&self, ev: &SampleEvent) {
        let user_backtrace = ev.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self.tx.send(IngestEvent::Sample(WireSampleEvent {
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

    fn on_target_attached(&self, ev: &TargetAttached) {
        let _ = self.tx.send(IngestEvent::TargetAttached {
            pid: ev.pid,
            task_port: ev.task_port,
        });
    }

    fn on_binary_loaded(&self, ev: &BinaryLoadedEvent) {
        let symbols = ev
            .symbols
            .iter()
            .map(|s| WireMachOSymbol {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name.to_vec(),
            })
            .collect();
        let _ = self.tx.send(IngestEvent::BinaryLoaded(WireBinaryLoaded {
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

    fn on_binary_unloaded(&self, ev: &BinaryUnloadedEvent) {
        let _ = self.tx.send(IngestEvent::BinaryUnloaded {
            path: ev.path.to_owned(),
            base_avma: ev.base_avma,
        });
    }

    fn on_thread_name(&self, ev: &ThreadName) {
        let _ = self.tx.send(IngestEvent::ThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: ev.name.to_owned(),
        });
    }

    fn on_wakeup(&self, ev: &WakeupEvent) {
        let _ = self.tx.send(IngestEvent::Wakeup(WireWakeup {
            timestamp_ns: ev.timestamp,
            waker_tid: ev.waker_tid,
            wakee_tid: ev.wakee_tid,
            waker_user_stack: ev.waker_user_stack.to_vec(),
            waker_kernel_stack: ev.waker_kernel_stack.to_vec(),
        }));
    }

    fn on_cpu_interval(&self, ev: &CpuIntervalEvent) {
        match &ev.kind {
            CpuIntervalKind::OnCpu => {
                let _ = self.tx.send(IngestEvent::OnCpuInterval(WireOnCpuInterval {
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
                let _ = self.tx.send(IngestEvent::OffCpuInterval(WireOffCpuInterval {
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
    fn on_macho_byte_source(&self, _source: Arc<dyn MachOByteSource>) {
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

    let (sync_tx, mut sync_rx) = mpsc::unbounded_channel::<IngestEvent>();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = sync_rx.recv().await {
            if vox_tx.send(event).await.is_err() {
                break;
            }
        }
        let _ = vox_tx.close(Default::default()).await;
    });

    Ok((run_id, IngestSink::new(sync_tx), forwarder))
}
