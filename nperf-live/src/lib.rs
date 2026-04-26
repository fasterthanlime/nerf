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
    BinaryLoadedEvent, BinaryUnloadedEvent, LiveSink, SampleEvent, TargetAttached, ThreadName,
};
use nperf_live_proto::{
    AnnotatedLine, AnnotatedView, FlameNode, FlamegraphUpdate, NeighborsUpdate, Profiler,
    ProfilerDispatcher, ThreadInfo, ThreadsUpdate, TimelineBucket, TimelineUpdate, TopEntry,
    TopSort, TopUpdate,
};

mod aggregator;
mod binaries;
mod disassemble;
mod highlight;
mod source;

pub use aggregator::Aggregator;
pub use binaries::{BinaryRegistry, LoadedBinary};

/// What the sampler thread pushes into tokio. Owned data so we can move
/// across the thread boundary cheaply.
pub(crate) enum LiveEvent {
    Sample {
        tid: u32,
        timestamp_ns: u64,
        user_addrs: Vec<u64>,
    },
    BinaryLoaded(binaries::LoadedBinary),
    BinaryUnloaded {
        base_avma: u64,
    },
    TargetAttached {
        pid: u32,
        task_port: u64,
    },
    ThreadName {
        tid: u32,
        name: String,
    },
}

#[derive(Clone)]
pub struct LiveSinkImpl {
    tx: mpsc::UnboundedSender<LiveEvent>,
}

impl LiveSink for LiveSinkImpl {
    fn on_sample(&self, event: &SampleEvent) {
        let user_addrs: Vec<u64> = event.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self.tx.send(LiveEvent::Sample {
            tid: event.tid,
            timestamp_ns: event.timestamp,
            user_addrs,
        });
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
            is_executable: event.is_executable,
            symbols,
        };
        let _ = self.tx.send(LiveEvent::BinaryLoaded(loaded));
    }

    fn on_binary_unloaded(&self, event: &BinaryUnloadedEvent) {
        let _ = self.tx.send(LiveEvent::BinaryUnloaded {
            base_avma: event.base_avma,
        });
    }

    fn on_target_attached(&self, event: &TargetAttached) {
        let _ = self.tx.send(LiveEvent::TargetAttached {
            pid: event.pid,
            task_port: event.task_port,
        });
    }

    fn on_thread_name(&self, event: &ThreadName) {
        let _ = self.tx.send(LiveEvent::ThreadName {
            tid: event.tid,
            name: event.name.to_owned(),
        });
    }
}

#[derive(Clone)]
pub struct LiveServer {
    pub aggregator: Arc<RwLock<Aggregator>>,
    pub binaries: Arc<RwLock<BinaryRegistry>>,
    /// One source resolver per server. addr2line `Context` isn't `Sync`
    /// (interior `LazyCell`s), so we use a `Mutex` rather than `RwLock`.
    /// Be careful not to hold this guard across `.await`.
    pub source: Arc<parking_lot::Mutex<source::SourceResolver>>,
}

impl Profiler for LiveServer {
    async fn top(&self, limit: u32, sort: TopSort, tid: Option<u32>) -> Vec<TopEntry> {
        build_top_entries(&self.aggregator, &self.binaries, limit as usize, sort, tid)
    }

    async fn subscribe_top(
        &self,
        limit: u32,
        sort: TopSort,
        tid: Option<u32>,
        output: vox::Tx<TopUpdate>,
    ) {
        tracing::info!(?sort, ?tid, limit, "subscribe_top: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                let snapshot = {
                    let entries =
                        build_top_entries(&aggregator, &binaries, limit as usize, sort, tid);
                    let total_samples = aggregator.read().total_samples(tid);
                    TopUpdate {
                        total_samples,
                        entries,
                    }
                };
                if let Err(e) = output.send(snapshot).await {
                    tracing::info!(?tid, "subscribe_top: stream ended: {e:?}");
                    break;
                }
                tracing::info!(?tid, "subscribe_top: sent!");
            }
        });
    }

    async fn total_samples(&self) -> u64 {
        self.aggregator.read().total_samples(None)
    }

    async fn subscribe_annotated(
        &self,
        address: u64,
        tid: Option<u32>,
        output: vox::Tx<AnnotatedView>,
    ) {
        tracing::info!(
            address = format!("{:#x}", address),
            ?tid,
            "subscribe_annotated: starting stream"
        );
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let source = self.source.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                let view = build_annotated_view(&aggregator, &binaries, &source, address, tid);
                if let Err(e) = output.send(view).await {
                    tracing::info!(
                        address = format!("{:#x}", address),
                        ?tid,
                        "subscribe_annotated: stream ended: {e:?}"
                    );
                    break;
                }
            }
        });
    }

    async fn subscribe_flamegraph(&self, tid: Option<u32>, output: vox::Tx<FlamegraphUpdate>) {
        tracing::info!(?tid, "subscribe_flamegraph: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = build_flame_update(&aggregator, &binaries, tid);
                if let Err(e) = output.send(update).await {
                    tracing::info!(?tid, "subscribe_flamegraph: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn subscribe_neighbors(
        &self,
        address: u64,
        tid: Option<u32>,
        output: vox::Tx<NeighborsUpdate>,
    ) {
        tracing::info!(
            address = format!("{:#x}", address),
            ?tid,
            "subscribe_neighbors: starting stream"
        );
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = build_neighbors_update(&aggregator, &binaries, address, tid);
                if let Err(e) = output.send(update).await {
                    tracing::info!(
                        address = format!("{:#x}", address),
                        ?tid,
                        "subscribe_neighbors: stream ended: {e:?}"
                    );
                    break;
                }
            }
        });
    }

    async fn subscribe_timeline(
        &self,
        tid: Option<u32>,
        output: vox::Tx<TimelineUpdate>,
    ) {
        tracing::info!(?tid, "subscribe_timeline: starting stream");
        let aggregator = self.aggregator.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = build_timeline_update(&aggregator, tid);
                if let Err(e) = output.send(update).await {
                    tracing::info!(?tid, "subscribe_timeline: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn subscribe_threads(&self, output: vox::Tx<ThreadsUpdate>) {
        tracing::info!("subscribe_threads: starting stream");
        let aggregator = self.aggregator.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = {
                    let agg = aggregator.read();
                    let mut threads: Vec<ThreadInfo> = agg
                        .iter_threads()
                        .map(|(tid, sample_count)| ThreadInfo {
                            tid,
                            name: agg.thread_name(tid).map(|s| s.to_owned()),
                            sample_count,
                        })
                        .collect();
                    threads.sort_by(|a, b| b.sample_count.cmp(&a.sample_count));
                    ThreadsUpdate { threads }
                };
                if let Err(e) = output.send(update).await {
                    tracing::info!("subscribe_threads: stream ended: {e:?}");
                    break;
                }
            }
        });
    }
}

fn build_top_entries(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    limit: usize,
    sort: TopSort,
    tid: Option<u32>,
) -> Vec<TopEntry> {
    use std::collections::HashMap;

    // Pull *all* per-address counts. We're going to collapse multiple
    // addresses inside one symbol into a single row, so truncating to
    // `limit` here would miss the symbol totals.
    let raw = aggregator.read().top_raw(usize::MAX, tid);
    let binaries = binaries.read();

    // Group key: (function_name, binary_basename). When unresolved (no
    // containing image), each address is its own group (keyed by its
    // hex form so it stays unique).
    struct Agg {
        address: u64,
        representative_self: u64,
        self_total: u64,
        total_total: u64,
        function_name: Option<String>,
        binary: Option<String>,
        is_main: bool,
    }
    let mut groups: HashMap<(String, String), Agg> = HashMap::new();
    for e in raw {
        let resolved = binaries.lookup_symbol(e.address);
        let (fn_name, bin, is_main) = match resolved {
            Some(r) => (Some(r.function_name), Some(r.binary), r.is_main),
            None => (None, None, false),
        };
        let key: (String, String) = match (&fn_name, &bin) {
            (Some(n), Some(b)) => (n.clone(), b.clone()),
            _ => (format!("{:#x}", e.address), String::new()),
        };
        groups
            .entry(key)
            .and_modify(|g| {
                g.self_total += e.self_count;
                g.total_total += e.total_count;
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
                is_main,
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
            is_main: g.is_main,
        })
        .collect();
    out.sort_by(|a, b| match sort {
        TopSort::BySelf => b
            .self_count
            .cmp(&a.self_count)
            .then_with(|| b.total_count.cmp(&a.total_count)),
        TopSort::ByTotal => b
            .total_count
            .cmp(&a.total_count)
            .then_with(|| b.self_count.cmp(&a.self_count)),
    });
    out.truncate(limit);
    out
}

/// Build a sample-density timeline from the per-thread raw sample
/// log. Bucket size is chosen so we stay around `TARGET_BUCKETS`
/// regardless of recording duration, with a sensible minimum so we
/// don't over-quantize a 1-second recording.
fn build_timeline_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    tid: Option<u32>,
) -> TimelineUpdate {
    const TARGET_BUCKETS: u64 = 200;
    const MIN_BUCKET_NS: u64 = 50_000_000; // 50 ms

    let agg = aggregator.read();
    let start = agg.session_start_ns().unwrap_or(0);
    let last = agg.last_sample_ns().unwrap_or(start);
    let duration = last.saturating_sub(start);

    let bucket_size_ns = if duration == 0 {
        MIN_BUCKET_NS
    } else {
        (duration / TARGET_BUCKETS).max(MIN_BUCKET_NS)
    };
    let n_buckets = ((duration / bucket_size_ns) + 1) as usize;
    let mut counts: Vec<u64> = vec![0; n_buckets.max(1)];

    let mut total: u64 = 0;
    for (_tid, sample) in agg.iter_samples(tid) {
        let rel = sample.timestamp_ns.saturating_sub(start);
        let idx = (rel / bucket_size_ns) as usize;
        if idx < counts.len() {
            counts[idx] += 1;
            total += 1;
        }
    }

    let buckets: Vec<TimelineBucket> = counts
        .into_iter()
        .enumerate()
        .map(|(i, count)| TimelineBucket {
            start_ns: i as u64 * bucket_size_ns,
            count,
        })
        .collect();

    TimelineUpdate {
        bucket_size_ns,
        duration_ns: duration,
        total_samples: total,
        buckets,
    }
}

/// Build the kcachegrind-style "family tree" view of a symbol.
///
/// We walk the call tree once and, for every node whose resolved
/// symbol matches the target, do two things:
///   1. Merge the entire ancestor chain (parent → grandparent → …)
///      into `callers_tree`, growing outward toward `main`.
///   2. Merge the entire descendant subtree into `callees_tree`,
///      keyed by symbol (so recursion + multiple call sites collapse).
fn build_neighbors_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    target_address: u64,
    tid: Option<u32>,
) -> NeighborsUpdate {
    use std::collections::HashMap;

    type SymbolKey = (Option<String>, Option<String>);

    #[derive(Default)]
    struct SymbolNode {
        count: u64,
        rep_address: u64,
        rep_self: u64,
        is_main: bool,
        children: HashMap<SymbolKey, SymbolNode>,
    }

    fn classify(
        addr: u64,
        bins: &BinaryRegistry,
    ) -> (SymbolKey, bool) {
        match bins.lookup_symbol(addr) {
            Some(r) => ((Some(r.function_name), Some(r.binary)), r.is_main),
            None => ((None, None), false),
        }
    }

    /// Insert one (addr, count) sample into `node`, merging by
    /// SymbolKey-keyed children. `delta` is added to this node's count;
    /// `rep_self` is the largest single contribution we've seen so we
    /// can pick the hottest address as the click-through representative.
    fn accumulate(
        node: &mut SymbolNode,
        addr: u64,
        delta: u64,
        is_main: bool,
    ) {
        node.count += delta;
        if delta > node.rep_self {
            node.rep_address = addr;
            node.rep_self = delta;
            node.is_main = is_main;
        }
    }

    fn merge_descendants(
        dst: &mut SymbolNode,
        src: &aggregator::StackNode,
        bins: &BinaryRegistry,
    ) {
        for (caddr, child) in &src.children {
            let (key, is_main) = classify(*caddr, bins);
            let entry = dst.children.entry(key).or_default();
            accumulate(entry, *caddr, child.count, is_main);
            merge_descendants(entry, child, bins);
        }
    }

    fn walk(
        node: &aggregator::StackNode,
        node_addr: u64,
        ancestors: &mut Vec<u64>,
        target_key: &SymbolKey,
        bins: &BinaryRegistry,
        callers: &mut SymbolNode,
        callees: &mut SymbolNode,
        own_count: &mut u64,
    ) {
        if node_addr != 0 {
            let (key, _is_main) = classify(node_addr, bins);
            if &key == target_key {
                *own_count += node.count;
                // Insert ancestor chain into callers_tree, innermost-first.
                let mut cur = &mut *callers;
                for &caller_addr in ancestors.iter().rev() {
                    let (ckey, cmain) = classify(caller_addr, bins);
                    let entry = cur.children.entry(ckey).or_default();
                    accumulate(entry, caller_addr, node.count, cmain);
                    cur = entry;
                }
                // Merge descendants into callees_tree.
                merge_descendants(callees, node, bins);
            }
        }

        let pushed = node_addr != 0;
        if pushed {
            ancestors.push(node_addr);
        }
        for (caddr, child) in &node.children {
            walk(
                child,
                *caddr,
                ancestors,
                target_key,
                bins,
                callers,
                callees,
                own_count,
            );
        }
        if pushed {
            ancestors.pop();
        }
    }

    fn to_flame_node(
        sn: SymbolNode,
        key: SymbolKey,
        threshold: u64,
    ) -> FlameNode {
        let SymbolNode {
            count,
            rep_address,
            is_main,
            children,
            ..
        } = sn;
        let mut child_nodes: Vec<FlameNode> = children
            .into_iter()
            .filter(|(_, c)| c.count >= threshold)
            .map(|(k, c)| to_flame_node(c, k, threshold))
            .collect();
        child_nodes.sort_by(|a, b| b.count.cmp(&a.count));
        FlameNode {
            address: rep_address,
            count,
            function_name: key.0,
            binary: key.1,
            is_main,
            children: child_nodes,
        }
    }

    let bins = binaries.read();
    let target_resolved = bins.lookup_symbol(target_address);
    let target_key: SymbolKey = match &target_resolved {
        Some(r) => (Some(r.function_name.clone()), Some(r.binary.clone())),
        None => (None, None),
    };

    let mut callers = SymbolNode::default();
    let mut callees = SymbolNode::default();
    let mut own_count: u64 = 0;

    let agg = aggregator.read();
    let root = agg.flame_root(tid);
    let mut ancestors: Vec<u64> = Vec::new();
    walk(
        &root,
        0,
        &mut ancestors,
        &target_key,
        &bins,
        &mut callers,
        &mut callees,
        &mut own_count,
    );

    // Stamp the target's own count + representative onto each tree's
    // root so the renderer has a useful "self" frame.
    callers.count = own_count;
    callers.rep_address = target_address;
    callers.is_main = target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false);
    callees.count = own_count;
    callees.rep_address = target_address;
    callees.is_main = target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false);

    let threshold = (own_count / 200).max(1);
    let callers_tree = to_flame_node(callers, target_key.clone(), threshold);
    let callees_tree = to_flame_node(callees, target_key.clone(), threshold);

    NeighborsUpdate {
        function_name: target_key.0,
        binary: target_key.1,
        is_main: target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false),
        own_count,
        callers_tree,
        callees_tree,
    }
}

fn build_flame_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    tid: Option<u32>,
) -> FlamegraphUpdate {
    let agg = aggregator.read();
    let bins = binaries.read();
    let total = agg.total_samples(tid);
    let threshold = (total / 200).max(1);

    let flame_root = agg.flame_root(tid);
    let mut children: Vec<FlameNode> = flame_root
        .children
        .iter()
        .filter(|(_, c)| c.count >= threshold)
        .map(|(a, c)| flame_node_to_proto(*a, c, threshold, &bins))
        .collect();
    for c in &mut children {
        fold_recursion(c);
    }
    children.sort_by(|a, b| b.count.cmp(&a.count));

    let root = FlameNode {
        address: 0,
        count: total,
        function_name: Some("(all)".into()),
        binary: None,
        is_main: false,
        children,
    };
    FlamegraphUpdate {
        total_samples: total,
        root,
    }
}

/// Collapse runs of same-symbol parent→child into a single node.
/// Recursive functions (and inlined call chains that share a name)
/// otherwise produce towers of identical boxes that eat vertical
/// space without adding information.
fn fold_recursion(node: &mut FlameNode) {
    while node.children.len() == 1 && symbol_eq(&node.children[0], node) {
        let child = node.children.remove(0);
        node.children = child.children;
    }
    for c in &mut node.children {
        fold_recursion(c);
    }
}

fn symbol_eq(a: &FlameNode, b: &FlameNode) -> bool {
    a.function_name.is_some() && a.function_name == b.function_name && a.binary == b.binary
}

fn flame_node_to_proto(
    address: u64,
    node: &aggregator::StackNode,
    threshold: u64,
    binaries: &BinaryRegistry,
) -> FlameNode {
    let resolved = binaries.lookup_symbol(address);
    let (function_name, binary, is_main) = match resolved {
        Some(r) => (Some(r.function_name), Some(r.binary), r.is_main),
        None => (None, None, false),
    };
    let mut children: Vec<FlameNode> = node
        .children
        .iter()
        .filter(|(_, c)| c.count >= threshold)
        .map(|(a, c)| flame_node_to_proto(*a, c, threshold, binaries))
        .collect();
    children.sort_by(|a, b| b.count.cmp(&a.count));
    FlameNode {
        address,
        count: node.count,
        function_name,
        binary,
        is_main,
        children,
    }
}

fn build_annotated_view(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    source: &Arc<parking_lot::Mutex<source::SourceResolver>>,
    address: u64,
    tid: Option<u32>,
) -> AnnotatedView {
    let resolved = binaries.write().resolve(address);

    let mut hl = highlight::AsmHighlighter::new();
    let mut lines: Vec<AnnotatedLine> = match &resolved {
        Some(r) => {
            let agg = aggregator.read();
            disassemble::disassemble(r, &mut hl, |addr| agg.self_count(addr, tid))
        }
        None => Vec::new(),
    };

    if let Some(r) = resolved.as_ref()
        && let Some(image) = r.image.as_ref()
    {
        let mut src = source.lock();
        let mut last: Option<(String, u32)> = None;
        for line in lines.iter_mut() {
            let svma = r.fn_start_svma + (line.address - r.base_address);
            let here = src.locate(&r.binary_path, image, svma);
            if here != last {
                if let Some((ref file, ln)) = here {
                    let html = src.snippet(file, ln);
                    line.source_header = Some(nperf_live_proto::SourceHeader {
                        file: file.clone(),
                        line: ln,
                        html,
                    });
                }
                last = here;
            }
        }
    }

    let function_name = match &resolved {
        Some(r) => r.function_name.clone(),
        None => format!("(no binary mapped at {:#x})", address),
    };
    let base_address = resolved.as_ref().map(|r| r.base_address).unwrap_or(address);
    AnnotatedView {
        function_name,
        base_address,
        queried_address: address,
        lines,
    }
}

/// Spawn the live-serving infrastructure on the current tokio runtime.
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
                    LiveEvent::Sample {
                        tid,
                        timestamp_ns,
                        user_addrs,
                    } => {
                        aggregator.write().record(tid, timestamp_ns, &user_addrs);
                    }
                    LiveEvent::ThreadName { tid, name } => {
                        aggregator.write().set_thread_name(tid, name);
                    }
                    LiveEvent::BinaryLoaded(loaded) => {
                        binaries.write().insert(loaded);
                    }
                    LiveEvent::BinaryUnloaded { base_avma } => {
                        binaries.write().remove(base_avma);
                    }
                    LiveEvent::TargetAttached { pid, task_port } => {
                        binaries.write().set_target(pid, task_port);
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
        source: Arc::new(parking_lot::Mutex::new(source::SourceResolver::new())),
    };
    let dispatcher = ProfilerDispatcher::new(server);
    let handle = tokio::spawn(async move {
        if let Err(e) = vox::serve_listener(listener, dispatcher).await {
            tracing::error!("vox serve_listener exited: {e}");
        }
    });

    Ok((LiveSinkImpl { tx }, handle))
}
