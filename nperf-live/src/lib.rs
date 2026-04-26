//! Live serving of nperf samples over a vox WebSocket RPC service.
//!
//! Architecture: the (sync) sampler thread pushes events into an unbounded
//! tokio channel via `LiveSinkImpl`. A drainer task on the tokio side updates
//! a shared `Aggregator` (sample counts) and `BinaryRegistry` (loaded
//! images + symbol tables), which the vox service queries on demand.

use std::sync::Arc;

use eyre::Result;
use parking_lot::RwLock;
use tokio::sync::mpsc;

use nperf_core::live_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, LiveSink, SampleEvent,
};
use nperf_live_proto::{
    AnnotatedLine, AnnotatedView, Profiler, ProfilerDispatcher, TopEntry, TopUpdate,
};

mod aggregator;
mod binaries;
mod disassemble;
mod highlight;

pub use aggregator::Aggregator;
pub use binaries::{BinaryRegistry, LoadedBinary};

/// What the sampler thread pushes into tokio. Owned data so we can move
/// across the thread boundary cheaply.
pub(crate) enum LiveEvent {
    Sample {
        user_addrs: Vec<u64>,
    },
    BinaryLoaded(binaries::LoadedBinary),
    BinaryUnloaded {
        base_avma: u64,
    },
}

#[derive(Clone)]
pub struct LiveSinkImpl {
    tx: mpsc::UnboundedSender<LiveEvent>,
}

impl LiveSink for LiveSinkImpl {
    fn on_sample(&self, event: &SampleEvent) {
        let user_addrs: Vec<u64> = event.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self.tx.send(LiveEvent::Sample { user_addrs });
    }

    fn on_binary_loaded(&self, event: &BinaryLoadedEvent) {
        let symbols: Vec<binaries::LiveSymbolOwned> = event
            .symbols
            .iter()
            .map(|s| binaries::LiveSymbolOwned {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name.to_vec(),
            })
            .collect();
        let loaded = binaries::LoadedBinary {
            path: event.path.to_owned(),
            base_avma: event.base_avma,
            avma_end: event.base_avma + event.vmsize,
            text_svma: event.text_svma,
            arch: event.arch.map(|s| s.to_owned()),
            symbols,
        };
        let _ = self.tx.send(LiveEvent::BinaryLoaded(loaded));
    }

    fn on_binary_unloaded(&self, event: &BinaryUnloadedEvent) {
        let _ = self.tx.send(LiveEvent::BinaryUnloaded {
            base_avma: event.base_avma,
        });
    }
}

#[derive(Clone)]
pub struct LiveServer {
    pub aggregator: Arc<RwLock<Aggregator>>,
    pub binaries: Arc<RwLock<BinaryRegistry>>,
}

impl Profiler for LiveServer {
    async fn top(&self, limit: u32) -> Vec<TopEntry> {
        build_top_entries(&self.aggregator, &self.binaries, limit as usize)
    }

    async fn subscribe_top(&self, limit: u32, output: vox::Tx<TopUpdate>) {
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            let snapshot = {
                let entries = build_top_entries(&aggregator, &binaries, limit as usize);
                let total_samples = aggregator.read().total_samples();
                TopUpdate {
                    total_samples,
                    entries,
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

    async fn subscribe_annotated(&self, address: u64, output: vox::Tx<AnnotatedView>) {
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            // Build the view in a sync block so neither the parking_lot guards
            // nor the (non-Send) arborium Highlighter cross an await.
            let view = build_annotated_view(&aggregator, &binaries, address);
            if output.send(view).await.is_err() {
                break;
            }
        }
    }
}

fn build_top_entries(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    limit: usize,
) -> Vec<TopEntry> {
    use std::collections::HashMap;

    // Pull *all* per-address counts. We're going to collapse multiple
    // addresses inside one symbol into a single row, so truncating to
    // `limit` here would miss the symbol totals.
    let raw = aggregator.read().top_raw(usize::MAX);
    let binaries = binaries.read();

    // Group key: (function_name, binary_basename). When unresolved (no
    // containing image), each address is its own group (keyed by its
    // hex form so it stays unique).
    struct Agg {
        address: u64,
        /// Self-count of `address` alone — used to decide which address
        /// to keep as the group's representative when a hotter one comes
        /// in. Not what we report; that's `self_total`.
        representative_self: u64,
        self_total: u64,
        total_total: u64,
        function_name: Option<String>,
        binary: Option<String>,
    }
    let mut groups: HashMap<(String, String), Agg> = HashMap::new();
    for e in raw {
        let (fn_name, bin) = match binaries.lookup_symbol(e.address) {
            Some((n, b)) => (Some(n), Some(b)),
            None => (None, None),
        };
        // Unresolved entries get their own group (keyed by address) so
        // we don't collapse different unknown rows into one.
        let key: (String, String) = match (&fn_name, &bin) {
            (Some(n), Some(b)) => (n.clone(), b.clone()),
            _ => (format!("{:#x}", e.address), String::new()),
        };
        groups
            .entry(key)
            .and_modify(|g| {
                g.self_total += e.self_count;
                g.total_total += e.total_count;
                // Keep the hottest address as the representative — clicking
                // it opens the same function's disassembly anyway, but
                // starting at the hottest line is a friendlier default
                // scroll position.
                if e.self_count > g.representative_self {
                    g.address = e.address;
                    g.representative_self = e.self_count;
                }
            })
            .or_insert(Agg {
                address: e.address,
                representative_self: e.self_count,
                self_total: e.self_count,
                total_total: e.total_count,
                function_name: fn_name,
                binary: bin,
            });
    }

    let mut out: Vec<TopEntry> = groups
        .into_values()
        .map(|g| TopEntry {
            address: g.address,
            self_count: g.self_total,
            total_count: g.total_total,
            function_name: g.function_name,
            binary: g.binary,
        })
        .collect();
    out.sort_by(|a, b| b.self_count.cmp(&a.self_count));
    out.truncate(limit);
    out
}

fn build_annotated_view(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    address: u64,
) -> AnnotatedView {
    // Resolve binary + symbol + bytes outside the aggregator lock; the
    // binary registry is its own RwLock and may need to lazily load a
    // CodeImage off disk on first hit.
    let resolved = binaries.write().resolve(address);

    let mut hl = highlight::AsmHighlighter::new();
    let lines: Vec<AnnotatedLine> = match &resolved {
        Some(r) => {
            let agg = aggregator.read();
            disassemble::disassemble(r, &mut hl, |addr| agg.self_count(addr))
        }
        None => Vec::new(),
    };

    let function_name = match &resolved {
        Some(r) => r.function_name.clone(),
        None => format!("(no binary mapped at {:#x})", address),
    };
    let base_address = resolved
        .as_ref()
        .map(|r| r.base_address)
        .unwrap_or(address);
    AnnotatedView {
        function_name,
        base_address,
        queried_address: address,
        lines,
    }
}

/// Spawn the live-serving infrastructure on the current tokio runtime.
///
/// Returns the `LiveSinkImpl` to install on `ProfilingController` and a
/// JoinHandle for the server task.
pub async fn start(addr: &str) -> Result<(LiveSinkImpl, tokio::task::JoinHandle<()>)> {
    let aggregator = Arc::new(RwLock::new(Aggregator::default()));
    let binaries = Arc::new(RwLock::new(BinaryRegistry::new()));
    let (tx, mut rx) = mpsc::unbounded_channel::<LiveEvent>();

    {
        let aggregator = aggregator.clone();
        let binaries = binaries.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    LiveEvent::Sample { user_addrs } => {
                        aggregator.write().record(&user_addrs);
                    }
                    LiveEvent::BinaryLoaded(loaded) => {
                        binaries.write().insert(loaded);
                    }
                    LiveEvent::BinaryUnloaded { base_avma } => {
                        binaries.write().remove(base_avma);
                    }
                }
            }
        });
    }

    let listener = vox::WsListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!("nperf-live listening on ws://{}", local);
    eprintln!("nperf-live: listening on ws://{}", local);

    let server = LiveServer {
        aggregator,
        binaries,
    };
    let dispatcher = ProfilerDispatcher::new(server);
    let handle = tokio::spawn(async move {
        if let Err(e) = vox::serve_listener(listener, dispatcher).await {
            tracing::error!("vox serve_listener exited: {e}");
        }
    });

    Ok((LiveSinkImpl { tx }, handle))
}
