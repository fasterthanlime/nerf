//! Live serving of nperf samples over a vox WebSocket RPC service.
//!
//! Architecture: the (sync) sampler thread pushes events into an unbounded
//! tokio channel via `LiveSinkImpl`. A drainer task on the tokio side updates
//! a shared `Aggregator`, which the vox service queries.

use std::sync::Arc;

use eyre::Result;
use facet::Facet;
use parking_lot::RwLock;
use tokio::sync::mpsc;

use nperf_core::live_sink::{LiveSink, SampleEvent};

mod aggregator;

pub use aggregator::Aggregator;

#[derive(Clone, Debug, Facet)]
pub struct TopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopUpdate {
    pub total_samples: u64,
    pub entries: Vec<TopEntry>,
}

#[vox::service]
pub trait Profiler {
    /// Snapshot of the top-N functions by self time.
    async fn top(&self, limit: u32) -> Vec<TopEntry>;

    /// Stream periodic top-N updates to the client.
    async fn subscribe_top(&self, limit: u32, output: vox::Tx<TopUpdate>);

    /// Total number of samples observed since the server started.
    async fn total_samples(&self) -> u64;
}

/// What the sampler thread pushes into tokio. Owned data so we can move
/// across the thread boundary cheaply.
pub(crate) struct OwnedSample {
    pub user_addrs: Vec<u64>,
}

#[derive(Clone)]
pub struct LiveSinkImpl {
    tx: mpsc::UnboundedSender<OwnedSample>,
}

impl LiveSink for LiveSinkImpl {
    fn on_sample(&self, event: &SampleEvent) {
        let user_addrs: Vec<u64> = event
            .user_backtrace
            .iter()
            .map(|f| f.address)
            .collect();
        let _ = self.tx.send(OwnedSample { user_addrs });
    }
}

#[derive(Clone)]
pub struct LiveServer {
    pub aggregator: Arc<RwLock<Aggregator>>,
}

impl Profiler for LiveServer {
    async fn top(&self, limit: u32) -> Vec<TopEntry> {
        self.aggregator.read().top(limit as usize)
    }

    async fn subscribe_top(&self, limit: u32, output: vox::Tx<TopUpdate>) {
        let aggregator = self.aggregator.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            let snapshot = {
                let agg = aggregator.read();
                TopUpdate {
                    total_samples: agg.total_samples(),
                    entries: agg.top(limit as usize),
                }
            };
            if output.send(snapshot).await.is_err() {
                break;
            }
        }
    }

    async fn total_samples(&self) -> u64 {
        self.aggregator.read().total_samples()
    }
}

/// Spawn the live-serving infrastructure on the current tokio runtime.
///
/// Returns the `LiveSinkImpl` to install on `ProfilingController` and a
/// JoinHandle for the server task.
pub async fn start(addr: &str) -> Result<(LiveSinkImpl, tokio::task::JoinHandle<()>)> {
    let aggregator = Arc::new(RwLock::new(Aggregator::default()));
    let (tx, mut rx) = mpsc::unbounded_channel::<OwnedSample>();

    // Drainer task: pull from the sampler and update the aggregator.
    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move {
            while let Some(sample) = rx.recv().await {
                aggregator.write().record(&sample.user_addrs);
            }
        });
    }

    let listener = vox::WsListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!("nperf-live listening on ws://{}", local);
    eprintln!("nperf-live: listening on ws://{}", local);

    let server = LiveServer { aggregator };
    let dispatcher = ProfilerDispatcher::new(server);
    let handle = tokio::spawn(async move {
        if let Err(e) = vox::serve_listener(listener, dispatcher).await {
            tracing::error!("vox serve_listener exited: {e}");
        }
    });

    Ok((LiveSinkImpl { tx }, handle))
}
