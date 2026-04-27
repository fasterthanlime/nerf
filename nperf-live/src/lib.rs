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
    WakeupEvent as LiveWakeupEvent,
};
#[cfg(target_os = "macos")]
use nperf_core::live_sink::MachOByteSource;
use nperf_live_proto::{
    AnnotatedLine, AnnotatedView, FlameNode, FlamegraphUpdate, LiveFilter, NeighborsUpdate,
    Profiler, ProfilerDispatcher, ThreadInfo, ThreadsUpdate, TimelineBucket, TimelineUpdate,
    TopEntry, TopSort, TopUpdate, ViewParams,
};

use crate::aggregator::{PmcAccum, PmuSample, RawSample, RawTopEntry};

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
        is_offcpu: bool,
        cycles: u64,
        instructions: u64,
        l1d_misses: u64,
        branch_mispreds: u64,
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
    Wakeup {
        timestamp_ns: u64,
        waker_tid: u32,
        wakee_tid: u32,
        waker_user_stack: Vec<u64>,
        waker_kernel_stack: Vec<u64>,
    },
    #[cfg(target_os = "macos")]
    MachOByteSource(Arc<dyn MachOByteSource>),
}

#[derive(Clone)]
pub struct LiveSinkImpl {
    tx: mpsc::UnboundedSender<LiveEvent>,
    /// Set by the `set_paused` RPC. While `true` we drop incoming
    /// sample / wakeup events on the floor instead of enqueuing them
    /// for the aggregator; binary registry + thread-name updates
    /// keep flowing so already-frozen data still resolves cleanly.
    paused: Arc<std::sync::atomic::AtomicBool>,
}

impl LiveSink for LiveSinkImpl {
    fn on_sample(&self, event: &SampleEvent) {
        if self.paused.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let user_addrs: Vec<u64> = event.user_backtrace.iter().map(|f| f.address).collect();
        let _ = self.tx.send(LiveEvent::Sample {
            tid: event.tid,
            timestamp_ns: event.timestamp,
            user_addrs,
            is_offcpu: event.is_offcpu,
            cycles: event.cycles,
            instructions: event.instructions,
            l1d_misses: event.l1d_misses,
            branch_mispreds: event.branch_mispreds,
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
            text_bytes: event.text_bytes.map(|b| b.to_vec()),
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

    fn on_wakeup(&self, event: &LiveWakeupEvent) {
        if self.paused.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let _ = self.tx.send(LiveEvent::Wakeup {
            timestamp_ns: event.timestamp,
            waker_tid: event.waker_tid,
            wakee_tid: event.wakee_tid,
            waker_user_stack: event.waker_user_stack.to_vec(),
            waker_kernel_stack: event.waker_kernel_stack.to_vec(),
        });
    }

    #[cfg(target_os = "macos")]
    fn on_macho_byte_source(&self, source: Arc<dyn MachOByteSource>) {
        let _ = self.tx.send(LiveEvent::MachOByteSource(source));
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
    /// Shared with the LiveSinkImpl on the recorder side -- when set,
    /// new samples and wakeup edges get dropped before they reach
    /// the aggregator. Drives the "Pause" button in the live UI.
    pub paused: Arc<std::sync::atomic::AtomicBool>,
}

impl Profiler for LiveServer {
    async fn top(&self, limit: u32, sort: TopSort, params: ViewParams) -> Vec<TopEntry> {
        let agg = self.aggregator.read();
        let bins = self.binaries.read();
        let raw = collect_top_raw(&agg, &bins, params.tid, &params.filter);
        group_top_entries(raw, &bins, sort, limit as usize)
    }

    async fn subscribe_top(
        &self,
        limit: u32,
        sort: TopSort,
        params: ViewParams,
        output: vox::Tx<TopUpdate>,
    ) {
        let ViewParams { tid, filter } = params;
        tracing::info!(?sort, ?tid, limit, "subscribe_top: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                let snapshot = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    if is_filter_empty(&filter) {
                        let raw = agg.top_raw(usize::MAX, tid);
                        let entries = group_top_entries(raw, &bins, sort, limit as usize);
                        TopUpdate {
                            total_samples: agg.total_samples(tid),
                            entries,
                        }
                    } else {
                        let pred = make_predicate(
                            &filter,
                            agg.session_start_ns().unwrap_or(0),
                            &bins,
                        );
                        let f = agg.aggregate_filtered(tid, pred);
                        let entries =
                            group_top_entries(f.top_raw(usize::MAX), &bins, sort, limit as usize);
                        TopUpdate {
                            total_samples: f.total_samples,
                            entries,
                        }
                    }
                };
                if let Err(e) = output.send(snapshot).await {
                    tracing::info!(?tid, "subscribe_top: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn total_samples(&self) -> u64 {
        self.aggregator.read().total_samples(None)
    }

    async fn subscribe_annotated(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<AnnotatedView>,
    ) {
        let ViewParams { tid, filter } = params;
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
                let view = {
                    if is_filter_empty(&filter) {
                        let agg = aggregator.read();
                        compute_annotated_view(&binaries, &source, address, |a| {
                            agg.self_count(a, tid)
                        })
                    } else {
                        // Snapshot self_counts under the lock, drop, then build.
                        let self_counts = {
                            let agg = aggregator.read();
                            let bins = binaries.read();
                            let pred = make_predicate(
                                &filter,
                                agg.session_start_ns().unwrap_or(0),
                                &bins,
                            );
                            agg.aggregate_filtered(tid, pred).self_counts
                        };
                        compute_annotated_view(&binaries, &source, address, |a| {
                            self_counts.get(&a).copied().unwrap_or(0)
                        })
                    }
                };
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

    async fn subscribe_flamegraph(&self, params: ViewParams, output: vox::Tx<FlamegraphUpdate>) {
        let ViewParams { tid, filter } = params;
        tracing::info!(?tid, "subscribe_flamegraph: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    if is_filter_empty(&filter) {
                        compute_flame_update(agg.total_samples(tid), &agg.flame_root(tid), &bins)
                    } else {
                        let pred = make_predicate(
                            &filter,
                            agg.session_start_ns().unwrap_or(0),
                            &bins,
                        );
                        let f = agg.aggregate_filtered(tid, pred);
                        compute_flame_update(f.total_samples, &f.flame_root, &bins)
                    }
                };
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
        params: ViewParams,
        output: vox::Tx<NeighborsUpdate>,
    ) {
        let ViewParams { tid, filter } = params;
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
                let update = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    if is_filter_empty(&filter) {
                        compute_neighbors_update(&agg.flame_root(tid), &bins, address)
                    } else {
                        let pred = make_predicate(
                            &filter,
                            agg.session_start_ns().unwrap_or(0),
                            &bins,
                        );
                        let f = agg.aggregate_filtered(tid, pred);
                        compute_neighbors_update(&f.flame_root, &bins, address)
                    }
                };
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

    async fn subscribe_wakers(
        &self,
        wakee_tid: u32,
        output: vox::Tx<nperf_live_proto::WakersUpdate>,
    ) {
        tracing::info!(?wakee_tid, "subscribe_wakers: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = build_wakers_update(&aggregator, &binaries, wakee_tid);
                if let Err(e) = output.send(update).await {
                    tracing::info!(?wakee_tid, "subscribe_wakers: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn set_paused(&self, paused: bool) {
        self.paused
            .store(paused, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(paused, "set_paused");
    }

    async fn is_paused(&self) -> bool {
        self.paused.load(std::sync::atomic::Ordering::Relaxed)
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

fn build_wakers_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    wakee_tid: u32,
) -> nperf_live_proto::WakersUpdate {
    let agg = aggregator.read();
    let bin = binaries.read();
    let raw = agg.top_wakers(wakee_tid, 50);
    let total: u64 = raw.iter().map(|w| w.count).sum();
    let entries: Vec<nperf_live_proto::WakerEntry> = raw
        .into_iter()
        .map(|w| {
            let resolved = bin.lookup_symbol(w.waker_leaf_address);
            let (function_name, binary, language) = match resolved {
                Some(r) => (
                    Some(r.function_name),
                    Some(r.binary),
                    r.language.as_str().to_owned(),
                ),
                None => (None, None, "unknown".to_owned()),
            };
            nperf_live_proto::WakerEntry {
                waker_tid: w.waker_tid,
                waker_address: w.waker_leaf_address,
                waker_function_name: function_name,
                waker_binary: binary,
                language,
                count: w.count,
            }
        })
        .collect();
    nperf_live_proto::WakersUpdate {
        wakee_tid,
        total_wakeups: total,
        entries,
    }
}

fn is_filter_empty(filter: &LiveFilter) -> bool {
    filter.time_range.is_none()
        && filter.exclude_symbols.is_empty()
        && matches!(filter.sample_mode, nperf_live_proto::SampleMode::Both)
}

/// Build the predicate that `aggregate_filtered` calls for each raw
/// sample. Captures `filter` + `binaries` + the recording origin so
/// time-range, sample-mode, and exclude-symbol filters can all be
/// applied in one pass.
fn make_predicate<'a>(
    filter: &'a LiveFilter,
    session_start_ns: u64,
    binaries: &'a BinaryRegistry,
) -> impl FnMut(&RawSample) -> bool + 'a {
    use std::collections::HashSet;
    let exclude: HashSet<(Option<String>, Option<String>)> = filter
        .exclude_symbols
        .iter()
        .map(|s| (s.function_name.clone(), s.binary.clone()))
        .collect();
    move |sample: &RawSample| {
        match filter.sample_mode {
            nperf_live_proto::SampleMode::Both => {}
            nperf_live_proto::SampleMode::OnCpu => {
                if sample.is_offcpu {
                    return false;
                }
            }
            nperf_live_proto::SampleMode::OffCpu => {
                if !sample.is_offcpu {
                    return false;
                }
            }
        }
        if let Some(ref tr) = filter.time_range {
            let rel = sample.timestamp_ns.saturating_sub(session_start_ns);
            if rel < tr.start_ns || rel >= tr.end_ns {
                return false;
            }
        }
        if !exclude.is_empty() {
            for &addr in sample.stack.iter() {
                let key = match binaries.lookup_symbol(addr) {
                    Some(r) => (Some(r.function_name), Some(r.binary)),
                    None => (None, None),
                };
                if exclude.contains(&key) {
                    return false;
                }
            }
        }
        true
    }
}

/// Helper for the one-shot `top` RPC: chooses fast/slow path based on
/// the filter and returns raw counts (pre-grouping).
fn collect_top_raw(
    agg: &Aggregator,
    binaries: &BinaryRegistry,
    tid: Option<u32>,
    filter: &LiveFilter,
) -> Vec<RawTopEntry> {
    if is_filter_empty(filter) {
        agg.top_raw(usize::MAX, tid)
    } else {
        let pred = make_predicate(filter, agg.session_start_ns().unwrap_or(0), binaries);
        agg.aggregate_filtered(tid, pred).top_raw(usize::MAX)
    }
}

fn group_top_entries(
    raw: Vec<RawTopEntry>,
    binaries: &BinaryRegistry,
    sort: TopSort,
    limit: usize,
) -> Vec<TopEntry> {
    use std::collections::HashMap;

    // Group key: (function_name, binary_basename). When unresolved (no
    // containing image), each address is its own group (keyed by its
    // hex form so it stays unique).
    struct Agg {
        address: u64,
        representative_self: u64,
        self_total: u64,
        total_total: u64,
        self_cycles: u64,
        self_instructions: u64,
        self_l1d_misses: u64,
        self_branch_mispreds: u64,
        total_cycles: u64,
        total_instructions: u64,
        total_l1d_misses: u64,
        total_branch_mispreds: u64,
        function_name: Option<String>,
        binary: Option<String>,
        is_main: bool,
        language: nperf_demangle::Language,
    }
    let mut groups: HashMap<(String, String), Agg> = HashMap::new();
    for e in raw {
        let resolved = binaries.lookup_symbol(e.address);
        let (fn_name, bin, is_main, language) = match resolved {
            Some(r) => (Some(r.function_name), Some(r.binary), r.is_main, r.language),
            None => (None, None, false, nperf_demangle::Language::Unknown),
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
                g.self_cycles = g.self_cycles.saturating_add(e.self_pmc.cycles);
                g.self_instructions = g
                    .self_instructions
                    .saturating_add(e.self_pmc.instructions);
                g.self_l1d_misses = g.self_l1d_misses.saturating_add(e.self_pmc.l1d_misses);
                g.self_branch_mispreds = g
                    .self_branch_mispreds
                    .saturating_add(e.self_pmc.branch_mispreds);
                g.total_cycles = g.total_cycles.saturating_add(e.total_pmc.cycles);
                g.total_instructions = g
                    .total_instructions
                    .saturating_add(e.total_pmc.instructions);
                g.total_l1d_misses = g
                    .total_l1d_misses
                    .saturating_add(e.total_pmc.l1d_misses);
                g.total_branch_mispreds = g
                    .total_branch_mispreds
                    .saturating_add(e.total_pmc.branch_mispreds);
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
                self_cycles: e.self_pmc.cycles,
                self_instructions: e.self_pmc.instructions,
                self_l1d_misses: e.self_pmc.l1d_misses,
                self_branch_mispreds: e.self_pmc.branch_mispreds,
                total_cycles: e.total_pmc.cycles,
                total_instructions: e.total_pmc.instructions,
                total_l1d_misses: e.total_pmc.l1d_misses,
                total_branch_mispreds: e.total_pmc.branch_mispreds,
                function_name: fn_name,
                binary: bin,
                is_main,
                language,
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
            language: g.language.as_str().to_owned(),
            self_cycles: g.self_cycles,
            self_instructions: g.self_instructions,
            self_l1d_misses: g.self_l1d_misses,
            self_branch_mispreds: g.self_branch_mispreds,
            total_cycles: g.total_cycles,
            total_instructions: g.total_instructions,
            total_l1d_misses: g.total_l1d_misses,
            total_branch_mispreds: g.total_branch_mispreds,
        })
        .collect();
    // Tie-break on function_name → binary → address so the row order
    // is stable across snapshots; otherwise rows with equal counts
    // shuffle every tick as the underlying HashMap iterates them in
    // a different order.
    out.sort_by(|a, b| {
        let primary = match sort {
            TopSort::BySelf => b
                .self_count
                .cmp(&a.self_count)
                .then_with(|| b.total_count.cmp(&a.total_count)),
            TopSort::ByTotal => b
                .total_count
                .cmp(&a.total_count)
                .then_with(|| b.self_count.cmp(&a.self_count)),
        };
        primary
            .then_with(|| a.function_name.cmp(&b.function_name))
            .then_with(|| a.binary.cmp(&b.binary))
            .then_with(|| a.address.cmp(&b.address))
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
fn compute_neighbors_update(
    flame_root: &aggregator::StackNode,
    binaries: &BinaryRegistry,
    target_address: u64,
) -> NeighborsUpdate {
    use std::collections::HashMap;

    type SymbolKey = (Option<String>, Option<String>);

    #[derive(Default)]
    struct SymbolNode {
        count: u64,
        rep_address: u64,
        rep_self: u64,
        is_main: bool,
        language: nperf_demangle::Language,
        children: HashMap<SymbolKey, SymbolNode>,
    }

    fn classify(
        addr: u64,
        bins: &BinaryRegistry,
    ) -> (SymbolKey, bool, nperf_demangle::Language) {
        match bins.lookup_symbol(addr) {
            Some(r) => (
                (Some(r.function_name), Some(r.binary)),
                r.is_main,
                r.language,
            ),
            None => ((None, None), false, nperf_demangle::Language::Unknown),
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
        language: nperf_demangle::Language,
    ) {
        node.count += delta;
        if delta > node.rep_self {
            node.rep_address = addr;
            node.rep_self = delta;
            node.is_main = is_main;
            node.language = language;
        }
    }

    fn merge_descendants(
        dst: &mut SymbolNode,
        src: &aggregator::StackNode,
        bins: &BinaryRegistry,
    ) {
        for (caddr, child) in &src.children {
            let (key, is_main, language) = classify(*caddr, bins);
            let entry = dst.children.entry(key).or_default();
            accumulate(entry, *caddr, child.count, is_main, language);
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
            let (key, _is_main, _language) = classify(node_addr, bins);
            if &key == target_key {
                *own_count += node.count;
                // Insert ancestor chain into callers_tree, innermost-first.
                let mut cur = &mut *callers;
                for &caller_addr in ancestors.iter().rev() {
                    let (ckey, cmain, clang) = classify(caller_addr, bins);
                    let entry = cur.children.entry(ckey).or_default();
                    accumulate(entry, caller_addr, node.count, cmain, clang);
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
            language,
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
            language: language.as_str().to_owned(),
            // PMC values aren't currently propagated through the
            // SymbolNode-based callers/callees trees -- the neighbors
            // view shows counts only.
            cycles: 0,
            instructions: 0,
            l1d_misses: 0,
            branch_mispreds: 0,
            children: child_nodes,
        }
    }

    let target_resolved = binaries.lookup_symbol(target_address);
    let target_key: SymbolKey = match &target_resolved {
        Some(r) => (Some(r.function_name.clone()), Some(r.binary.clone())),
        None => (None, None),
    };
    let target_language = target_resolved
        .as_ref()
        .map(|r| r.language)
        .unwrap_or(nperf_demangle::Language::Unknown);

    let mut callers = SymbolNode::default();
    let mut callees = SymbolNode::default();
    let mut own_count: u64 = 0;

    let mut ancestors: Vec<u64> = Vec::new();
    walk(
        flame_root,
        0,
        &mut ancestors,
        &target_key,
        binaries,
        &mut callers,
        &mut callees,
        &mut own_count,
    );

    // Stamp the target's own count + representative onto each tree's
    // root so the renderer has a useful "self" frame.
    callers.count = own_count;
    callers.rep_address = target_address;
    callers.is_main = target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false);
    callers.language = target_language;
    callees.count = own_count;
    callees.rep_address = target_address;
    callees.is_main = target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false);
    callees.language = target_language;

    // Same lenient 0.05% threshold as the main flamegraph so the
    // family tree shows small but non-trivial neighbours.
    let threshold = (own_count / 2000).max(1);
    let callers_tree = to_flame_node(callers, target_key.clone(), threshold);
    let callees_tree = to_flame_node(callees, target_key.clone(), threshold);

    NeighborsUpdate {
        function_name: target_key.0,
        binary: target_key.1,
        is_main: target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false),
        language: target_language.as_str().to_owned(),
        own_count,
        callers_tree,
        callees_tree,
    }
}

fn compute_flame_update(
    total: u64,
    flame_root: &aggregator::StackNode,
    binaries: &BinaryRegistry,
) -> FlamegraphUpdate {
    // 0.05% of total. Lower than the previous 0.5% so that when the
    // user focuses into a smaller subtree (say 30k of 750k samples)
    // there's still meaningful per-callsite detail instead of one
    // big "(N small frames)" cell. Bumps the wire payload roughly
    // 5-10x but the live UI handles it; the residue cell still
    // catches the truly-tiny tail.
    let threshold = (total / 2000).max(1);
    let (mut children, residue) =
        build_children_with_residue(&[flame_root], threshold, binaries);
    for c in &mut children {
        fold_recursion(c);
    }
    children.sort_by(|a, b| b.count.cmp(&a.count));
    if let Some(extra) = residue {
        children.push(extra);
    }

    // Samples whose user-stack walk returned zero frames (kernel-only
    // samples, walk failures, etc.) increment `total_samples` but
    // never touch `flame_root.children`. Without this synthetic leaf
    // the (all) row says "100%" but the visible cells fill <100% of
    // the width, leaving black space that looks like a bug. Spell it
    // out instead.
    let visible_sum: u64 = children.iter().map(|c| c.count).sum();
    if total > visible_sum {
        let missing = total - visible_sum;
        children.push(FlameNode {
            address: u64::MAX - 1,
            count: missing,
            function_name: Some(format!("(in-kernel / no user stack: {missing} samples)")),
            binary: None,
            is_main: false,
            language: nperf_demangle::Language::Unknown.as_str().to_owned(),
            cycles: 0,
            instructions: 0,
            l1d_misses: 0,
            branch_mispreds: 0,
            children: Vec::new(),
        });
    }

    // Root sums counters across all children so the "(all)" row
    // shows the recording's grand totals.
    let total_cycles: u64 = children.iter().map(|c| c.cycles).sum();
    let total_instructions: u64 = children.iter().map(|c| c.instructions).sum();
    let total_l1d_misses: u64 = children.iter().map(|c| c.l1d_misses).sum();
    let total_branch_mispreds: u64 = children.iter().map(|c| c.branch_mispreds).sum();

    let root = FlameNode {
        address: 0,
        count: total,
        function_name: Some("(all)".into()),
        binary: None,
        is_main: false,
        language: nperf_demangle::Language::Unknown.as_str().to_owned(),
        cycles: total_cycles,
        instructions: total_instructions,
        l1d_misses: total_l1d_misses,
        branch_mispreds: total_branch_mispreds,
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

/// Walk a list of "sibling" StackNodes that should be considered
/// together, group their children by resolved (function, binary)
/// symbol, apply a count threshold, and recurse. The siblings list
/// lets us fold multiple call-site addresses that map to the same
/// symbol into one cell without copying subtrees: callers below pass
/// the borrowed `StackNode`s of the merged group on to the recursive
/// step.
///
/// Without this grouping, the flame is keyed by raw PC address —
/// recursive functions and any function called from multiple sites
/// fragment into a row of skinny same-name cells, and the same-name
/// children in the subtree never merge either. The neighbours view
/// already groups by symbol; the main flame now matches.
///
/// Sub-threshold groups are folded into a single greyed-out residue
/// sibling so the renderer doesn't leave black space where the long
/// tail used to live.
fn build_children_with_residue(
    sources: &[&aggregator::StackNode],
    threshold: u64,
    binaries: &BinaryRegistry,
) -> (Vec<FlameNode>, Option<FlameNode>) {
    use std::collections::HashMap;

    type SymbolKey = (Option<String>, Option<String>);

    struct Acc<'a> {
        count: u64,
        pmc: PmcAccum,
        rep_addr: u64,
        rep_count: u64,
        is_main: bool,
        language: nperf_demangle::Language,
        sub_sources: Vec<&'a aggregator::StackNode>,
    }

    let mut groups: HashMap<SymbolKey, Acc> = HashMap::new();
    for src in sources {
        for (&addr, child) in &src.children {
            let resolved = binaries.lookup_symbol(addr);
            let (fname, bin, is_main, lang) = match resolved {
                Some(r) => (
                    Some(r.function_name),
                    Some(r.binary),
                    r.is_main,
                    r.language,
                ),
                None => (None, None, false, nperf_demangle::Language::Unknown),
            };
            let key = (fname, bin);
            let acc = groups.entry(key).or_insert_with(|| Acc {
                count: 0,
                pmc: PmcAccum::default(),
                rep_addr: addr,
                rep_count: 0,
                is_main,
                language: lang,
                sub_sources: Vec::new(),
            });
            acc.count += child.count;
            acc.pmc.cycles = acc.pmc.cycles.saturating_add(child.pmc.cycles);
            acc.pmc.instructions = acc.pmc.instructions.saturating_add(child.pmc.instructions);
            acc.pmc.l1d_misses = acc.pmc.l1d_misses.saturating_add(child.pmc.l1d_misses);
            acc.pmc.branch_mispreds = acc
                .pmc
                .branch_mispreds
                .saturating_add(child.pmc.branch_mispreds);
            // Largest single contributor's address is the click-through
            // representative (matches what compute_neighbors_update does).
            if child.count > acc.rep_count {
                acc.rep_addr = addr;
                acc.rep_count = child.count;
                acc.is_main = is_main;
                acc.language = lang;
            }
            acc.sub_sources.push(child);
        }
    }

    let mut visible: Vec<FlameNode> = Vec::new();
    let mut residue_count: u64 = 0;
    let mut residue_dropped: u64 = 0;
    for ((fname, bin), acc) in groups {
        if acc.count >= threshold {
            let (mut grandchildren, gres) =
                build_children_with_residue(&acc.sub_sources, threshold, binaries);
            grandchildren.sort_by(|a, b| b.count.cmp(&a.count));
            if let Some(extra) = gres {
                grandchildren.push(extra);
            }
            visible.push(FlameNode {
                address: acc.rep_addr,
                count: acc.count,
                function_name: fname,
                binary: bin,
                is_main: acc.is_main,
                language: acc.language.as_str().to_owned(),
                cycles: acc.pmc.cycles,
                instructions: acc.pmc.instructions,
                l1d_misses: acc.pmc.l1d_misses,
                branch_mispreds: acc.pmc.branch_mispreds,
                children: grandchildren,
            });
        } else {
            residue_count = residue_count.saturating_add(acc.count);
            residue_dropped += 1;
        }
    }
    let residue = if residue_count > 0 && residue_dropped > 0 {
        Some(FlameNode {
            // Sentinel address: the frontend treats any node with
            // address == 0 as the root "(all)" otherwise; we set
            // function_name explicitly so the labelFor() takes that
            // path first, but we also pick u64::MAX here as a defence
            // in case future renderers fall back to address.
            address: u64::MAX,
            count: residue_count,
            function_name: Some(format!("({} small frames)", residue_dropped)),
            binary: None,
            is_main: false,
            language: nperf_demangle::Language::Unknown.as_str().to_owned(),
            cycles: 0,
            instructions: 0,
            l1d_misses: 0,
            branch_mispreds: 0,
            children: Vec::new(),
        })
    } else {
        None
    };
    (visible, residue)
}

fn compute_annotated_view(
    binaries: &Arc<RwLock<BinaryRegistry>>,
    source: &Arc<parking_lot::Mutex<source::SourceResolver>>,
    address: u64,
    self_count: impl Fn(u64) -> u64,
) -> AnnotatedView {
    let resolved = binaries.write().resolve(address);

    let mut hl = highlight::AsmHighlighter::new();
    let mut lines: Vec<AnnotatedLine> = match &resolved {
        Some(r) => disassemble::disassemble(r, &mut hl, |addr| self_count(addr)),
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
    let language = resolved
        .as_ref()
        .map(|r| r.language)
        .unwrap_or(nperf_demangle::Language::Unknown);
    let base_address = resolved.as_ref().map(|r| r.base_address).unwrap_or(address);
    AnnotatedView {
        function_name,
        language: language.as_str().to_owned(),
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
                        is_offcpu,
                        cycles,
                        instructions,
                        l1d_misses,
                        branch_mispreds,
                    } => {
                        aggregator.write().record(
                            tid,
                            timestamp_ns,
                            &user_addrs,
                            is_offcpu,
                            PmuSample {
                                cycles,
                                instructions,
                                l1d_misses,
                                branch_mispreds,
                            },
                        );
                    }
                    LiveEvent::Wakeup {
                        timestamp_ns,
                        waker_tid,
                        wakee_tid,
                        waker_user_stack,
                        waker_kernel_stack,
                    } => {
                        aggregator.write().record_wakeup(
                            timestamp_ns,
                            waker_tid,
                            wakee_tid,
                            waker_user_stack,
                            waker_kernel_stack,
                        );
                    }
                    #[cfg(target_os = "macos")]
                    LiveEvent::MachOByteSource(source) => {
                        binaries.write().set_macho_byte_source(source);
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

    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let server = LiveServer {
        aggregator,
        binaries,
        source: Arc::new(parking_lot::Mutex::new(source::SourceResolver::new())),
        paused: paused.clone(),
    };
    let dispatcher = ProfilerDispatcher::new(server);
    let handle = tokio::spawn(async move {
        if let Err(e) = vox::serve_listener(listener, dispatcher).await {
            tracing::error!("vox serve_listener exited: {e}");
        }
    });

    Ok((LiveSinkImpl { tx, paused }, handle))
}
