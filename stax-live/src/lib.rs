//! Aggregator + binary registry + Profiler service impl. Embedded
//! into stax-server, which feeds them via the wire-side ingest path.
//! There used to be an in-process `--serve` aggregator entry point
//! here too; that's been deleted.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::RwLock;

use stax_live_proto::{
    AnnotatedLine, AnnotatedView, CfgUpdate, FlameNode, FlamegraphUpdate, IntervalEntry,
    IntervalListUpdate, LiveFilter, NeighborsUpdate, PetSampleEntry, PetSampleListUpdate,
    Profiler, ThreadInfo, ThreadsUpdate, TimelineBucket, TimelineUpdate, TopEntry, TopSort,
    TopUpdate, ViewParams,
};

use crate::aggregator::{Aggregation, EventCtx, OffCpuBreakdown, PmcAccum, StackNode};
pub use crate::aggregator::{IntervalKind, PmuSample};

mod aggregator;
mod binaries;
mod cfg;
mod classify;
mod disassemble;
mod highlight;
#[cfg(target_os = "macos")]
mod kernel_symbols;
pub mod source;

pub use aggregator::Aggregator;
pub use binaries::{BinaryRegistry, LiveSymbolOwned, LoadedBinary, ResolvedSymbol};

#[derive(Clone)]
pub struct LiveServer {
    pub aggregator: Arc<RwLock<Aggregator>>,
    pub binaries: Arc<RwLock<BinaryRegistry>>,
    /// Monotonic data version bumped by stax-server when the
    /// aggregator or binary registry changes. Subscription tasks use
    /// this to avoid rebuilding expensive views while the browser is
    /// open but no new samples have landed.
    pub revision: Arc<AtomicU64>,
    /// One source resolver per server. addr2line `Context` isn't `Sync`
    /// (interior `LazyCell`s), so we use a `Mutex` rather than `RwLock`.
    /// Be careful not to hold this guard across `.await`.
    pub source: Arc<parking_lot::Mutex<source::SourceResolver>>,
    /// Shared with the LiveSinkImpl on the recorder side -- when set,
    /// new samples and wakeup edges get dropped before they reach
    /// the aggregator. Drives the "Pause" button in the live UI.
    pub paused: Arc<AtomicBool>,
}

fn should_publish_revision(revision: &AtomicU64, last_seen: &mut Option<u64>) -> bool {
    let current = revision.load(Ordering::Acquire);
    match *last_seen {
        Some(last) if last == current => false,
        _ => {
            *last_seen = Some(current);
            true
        }
    }
}

impl Profiler for LiveServer {
    async fn top(&self, limit: u32, sort: TopSort, params: ViewParams) -> Vec<TopEntry> {
        self.top_update(limit, sort, params).await.entries
    }

    async fn top_update(&self, limit: u32, sort: TopSort, params: ViewParams) -> TopUpdate {
        let agg = self.aggregator.read();
        let bins = self.binaries.read();
        let aggregation = aggregate_with_filter(&agg, &bins, params.tid, &params.filter);
        let entries = group_top_entries(&aggregation, &bins, sort, limit as usize);
        TopUpdate {
            total_on_cpu_ns: aggregation.total_on_cpu_ns,
            total_off_cpu: aggregation.total_off_cpu.to_proto(),
            entries,
        }
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
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let snapshot = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    let aggregation = aggregate_with_filter(&agg, &bins, tid, &filter);
                    let entries = group_top_entries(&aggregation, &bins, sort, limit as usize);
                    TopUpdate {
                        total_on_cpu_ns: aggregation.total_on_cpu_ns,
                        total_off_cpu: aggregation.total_off_cpu.to_proto(),
                        entries,
                    }
                };
                if let Err(e) = output.send(snapshot).await {
                    tracing::info!(?tid, "subscribe_top: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn total_on_cpu_ns(&self) -> u64 {
        let agg = self.aggregator.read();
        let bins = self.binaries.read();
        agg.aggregate_all(&bins).total_on_cpu_ns
    }

    async fn annotated(&self, address: u64, params: ViewParams) -> AnnotatedView {
        let ViewParams { tid, filter } = params;
        let by_address = {
            let agg = self.aggregator.read();
            let bins = self.binaries.read();
            aggregate_with_filter(&agg, &bins, tid, &filter).by_address
        };
        compute_annotated_view(&self.binaries, &self.source, address, |a| {
            by_address
                .get(&a)
                .map(|s| (s.self_on_cpu_ns, s.self_pet_samples))
                .unwrap_or((0, 0))
        })
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
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                // Snapshot per-address stats under the lock, drop, then build.
                let by_address = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    aggregate_with_filter(&agg, &bins, tid, &filter).by_address
                };
                let view = compute_annotated_view(&binaries, &source, address, |a| {
                    by_address
                        .get(&a)
                        .map(|s| (s.self_on_cpu_ns, s.self_pet_samples))
                        .unwrap_or((0, 0))
                });
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

    async fn cfg(&self, address: u64, params: ViewParams) -> CfgUpdate {
        let ViewParams { tid, filter } = params;
        let by_address = {
            let agg = self.aggregator.read();
            let bins = self.binaries.read();
            aggregate_with_filter(&agg, &bins, tid, &filter).by_address
        };
        compute_cfg_view(&self.binaries, address, |a| {
            by_address
                .get(&a)
                .map(|s| (s.self_on_cpu_ns, s.self_pet_samples))
                .unwrap_or((0, 0))
        })
    }

    async fn subscribe_cfg(&self, address: u64, params: ViewParams, output: vox::Tx<CfgUpdate>) {
        let ViewParams { tid, filter } = params;
        tracing::info!(
            address = format!("{:#x}", address),
            ?tid,
            "subscribe_cfg: starting stream"
        );
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let by_address = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    aggregate_with_filter(&agg, &bins, tid, &filter).by_address
                };
                let view = compute_cfg_view(&binaries, address, |a| {
                    by_address
                        .get(&a)
                        .map(|s| (s.self_on_cpu_ns, s.self_pet_samples))
                        .unwrap_or((0, 0))
                });
                if let Err(e) = output.send(view).await {
                    tracing::info!(
                        address = format!("{:#x}", address),
                        ?tid,
                        "subscribe_cfg: stream ended: {e:?}"
                    );
                    break;
                }
            }
        });
    }

    async fn flamegraph(&self, params: ViewParams) -> FlamegraphUpdate {
        let ViewParams { tid, filter } = params;
        let agg = self.aggregator.read();
        let bins = self.binaries.read();
        let aggregation = aggregate_with_filter(&agg, &bins, tid, &filter);
        compute_flame_update(&aggregation, &bins)
    }

    async fn subscribe_flamegraph(&self, params: ViewParams, output: vox::Tx<FlamegraphUpdate>) {
        let ViewParams { tid, filter } = params;
        tracing::info!(?tid, "subscribe_flamegraph: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    let aggregation = aggregate_with_filter(&agg, &bins, tid, &filter);
                    compute_flame_update(&aggregation, &bins)
                };
                if let Err(e) = output.send(update).await {
                    tracing::info!(?tid, "subscribe_flamegraph: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn neighbors(&self, address: u64, params: ViewParams) -> NeighborsUpdate {
        let ViewParams { tid, filter } = params;
        let agg = self.aggregator.read();
        let bins = self.binaries.read();
        let aggregation = aggregate_with_filter(&agg, &bins, tid, &filter);
        compute_neighbors_update(&aggregation.flame_root, &bins, address)
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
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    let aggregation = aggregate_with_filter(&agg, &bins, tid, &filter);
                    compute_neighbors_update(&aggregation.flame_root, &bins, address)
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

    async fn timeline(&self, tid: Option<u32>) -> TimelineUpdate {
        build_timeline_update(&self.aggregator, tid)
    }

    async fn subscribe_timeline(&self, tid: Option<u32>, output: vox::Tx<TimelineUpdate>) {
        tracing::info!(?tid, "subscribe_timeline: starting stream");
        let aggregator = self.aggregator.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = build_timeline_update(&aggregator, tid);
                if let Err(e) = output.send(update).await {
                    tracing::info!(?tid, "subscribe_timeline: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn wakers(&self, wakee_tid: u32) -> stax_live_proto::WakersUpdate {
        build_wakers_update(&self.aggregator, &self.binaries, wakee_tid)
    }

    async fn subscribe_wakers(
        &self,
        wakee_tid: u32,
        output: vox::Tx<stax_live_proto::WakersUpdate>,
    ) {
        tracing::info!(?wakee_tid, "subscribe_wakers: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = build_wakers_update(&aggregator, &binaries, wakee_tid);
                if let Err(e) = output.send(update).await {
                    tracing::info!(?wakee_tid, "subscribe_wakers: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn intervals(&self, flame_key: String, params: ViewParams) -> IntervalListUpdate {
        let _ = flame_key;
        let ViewParams { tid, filter } = params;
        let agg = self.aggregator.read();
        let bins = self.binaries.read();
        build_intervals_update(&agg, &bins, tid, &filter)
    }

    async fn subscribe_intervals(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<IntervalListUpdate>,
    ) {
        // `flame_key` is currently ignored (TODO: filter to the
        // intervals whose stack matches the prefix encoded by the
        // key). For now we return every off-CPU interval matching
        // the tid + time/exclude filter, capped at INTERVAL_CAP per
        // snapshot so the wire payload stays bounded.
        let ViewParams { tid, filter } = params;
        tracing::info!(?tid, %flame_key, "subscribe_intervals: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    build_intervals_update(&agg, &bins, tid, &filter)
                };
                if let Err(e) = output.send(update).await {
                    tracing::info!("subscribe_intervals: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn pet_samples(&self, flame_key: String, params: ViewParams) -> PetSampleListUpdate {
        let _ = flame_key;
        let ViewParams { tid, filter } = params;
        let agg = self.aggregator.read();
        build_pet_samples_update(&agg, tid, &filter)
    }

    async fn subscribe_pet_samples(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<PetSampleListUpdate>,
    ) {
        // Same flame_key-filter caveat as subscribe_intervals.
        let ViewParams { tid, filter } = params;
        tracing::info!(?tid, %flame_key, "subscribe_pet_samples: starting stream");
        let aggregator = self.aggregator.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = {
                    let agg = aggregator.read();
                    build_pet_samples_update(&agg, tid, &filter)
                };
                if let Err(e) = output.send(update).await {
                    tracing::info!("subscribe_pet_samples: stream ended: {e:?}");
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

    async fn threads(&self) -> ThreadsUpdate {
        build_threads_update(&self.aggregator, &self.binaries)
    }

    async fn subscribe_threads(&self, output: vox::Tx<ThreadsUpdate>) {
        tracing::info!("subscribe_threads: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        let revision = self.revision.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut last_seen = None;
            loop {
                interval.tick().await;
                if !should_publish_revision(&revision, &mut last_seen) {
                    continue;
                }
                let update = build_threads_update(&aggregator, &binaries);
                if let Err(e) = output.send(update).await {
                    tracing::info!("subscribe_threads: stream ended: {e:?}");
                    break;
                }
            }
        });
    }
}

fn off_cpu_total_proto(b: &stax_live_proto::OffCpuBreakdown) -> u64 {
    b.idle_ns
        .saturating_add(b.lock_ns)
        .saturating_add(b.semaphore_ns)
        .saturating_add(b.ipc_ns)
        .saturating_add(b.io_read_ns)
        .saturating_add(b.io_write_ns)
        .saturating_add(b.readiness_ns)
        .saturating_add(b.sleep_ns)
        .saturating_add(b.connect_ns)
        .saturating_add(b.other_ns)
}

fn build_threads_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
) -> ThreadsUpdate {
    let agg = aggregator.read();
    let bins = binaries.read();
    let mut threads: Vec<ThreadInfo> = agg
        .iter_threads()
        .map(|tid| {
            let aggregation = agg.aggregate(Some(tid), &bins, |_| true);
            let pet_samples: u64 = agg
                .thread_stats(tid)
                .map(|s| s.pet_samples.len() as u64)
                .unwrap_or(0);
            ThreadInfo {
                tid,
                name: agg.thread_name(tid).map(|s| s.to_owned()),
                on_cpu_ns: aggregation.total_on_cpu_ns,
                off_cpu: aggregation.total_off_cpu.to_proto(),
                pet_samples,
            }
        })
        .collect();
    threads.sort_by(|a, b| {
        let a_total = a.on_cpu_ns.saturating_add(off_cpu_total_proto(&a.off_cpu));
        let b_total = b.on_cpu_ns.saturating_add(off_cpu_total_proto(&b.off_cpu));
        b_total.cmp(&a_total)
    });
    ThreadsUpdate { threads }
}

/// One-stop helper: run `Aggregator::aggregate` with the
/// `LiveFilter`-derived predicate. Pulled out because every RPC
/// handler kicks off the same dance.
fn aggregate_with_filter(
    agg: &Aggregator,
    binaries: &BinaryRegistry,
    tid: Option<u32>,
    filter: &LiveFilter,
) -> Aggregation {
    if is_filter_empty(filter) {
        agg.aggregate(tid, binaries, |_| true)
    } else {
        let session_start = agg.session_start_ns().unwrap_or(0);
        let pred = make_predicate(filter, session_start);
        agg.aggregate(tid, binaries, pred)
    }
}

fn build_wakers_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    binaries: &Arc<RwLock<BinaryRegistry>>,
    wakee_tid: u32,
) -> stax_live_proto::WakersUpdate {
    let agg = aggregator.read();
    let bin = binaries.read();
    let raw = agg.top_wakers(wakee_tid, 50);
    let total: u64 = raw.iter().map(|w| w.count).sum();
    let entries: Vec<stax_live_proto::WakerEntry> = raw
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
            stax_live_proto::WakerEntry {
                waker_tid: w.waker_tid,
                waker_address: w.waker_leaf_address,
                waker_function_name: function_name,
                waker_binary: binary,
                language,
                count: w.count,
            }
        })
        .collect();
    stax_live_proto::WakersUpdate {
        wakee_tid,
        total_wakeups: total,
        entries,
    }
}

/// Cap on entries returned per drill-down RPC tick. Bounded wire size
/// matters more than completeness here -- the user is looking for
/// representative samples / intervals to inspect, not an exhaustive
/// dump. The total count fields tell the UI when there's more.
const DRILLDOWN_ENTRY_CAP: usize = 10_000;

fn build_intervals_update(
    agg: &Aggregator,
    binaries: &BinaryRegistry,
    tid: Option<u32>,
    filter: &LiveFilter,
) -> IntervalListUpdate {
    let session_start = agg.session_start_ns().unwrap_or(0);
    let mut interner = StringInterner::new();
    let mut total_intervals: u64 = 0;
    let mut total_duration_ns: u64 = 0;
    let mut by_reason = aggregator::OffCpuBreakdown::default();
    let mut entries: Vec<IntervalEntry> = Vec::new();

    for (event_tid, raw) in agg.iter_intervals(tid) {
        // We only surface off-CPU intervals here; on-CPU intervals
        // are visible via `subscribe_pet_samples` (where each
        // entry is one PET hit inside the interval). The two
        // streams together cover both axes.
        let (stack, waker_tid, waker_user_stack) = match &raw.kind {
            IntervalKind::OnCpu => continue,
            IntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => (stack, waker_tid, waker_user_stack),
        };
        // Apply the time-range / exclude-symbol filter.
        if let Some(ref tr) = filter.time_range {
            let rel = raw.start_ns.saturating_sub(session_start);
            if rel < tr.start_ns || rel >= tr.end_ns {
                continue;
            }
        }
        let interval_ns = raw.end_ns.saturating_sub(raw.start_ns);
        if interval_ns == 0 {
            continue;
        }
        let leaf_name = stack
            .first()
            .and_then(|&addr| binaries.lookup_symbol(addr))
            .map(|r| r.function_name);
        let reason = classify::classify_offcpu(leaf_name.as_deref());
        total_intervals = total_intervals.saturating_add(1);
        total_duration_ns = total_duration_ns.saturating_add(interval_ns);
        by_reason.add_reason(reason, interval_ns);

        if entries.len() < DRILLDOWN_ENTRY_CAP {
            // Resolve the waker symbol into the shared string table.
            let (waker_address, waker_function_name, waker_binary) =
                match (waker_tid, waker_user_stack.as_deref()) {
                    (Some(_), Some(stack)) => match stack.first().copied() {
                        Some(addr) => match binaries.lookup_symbol(addr) {
                            Some(r) => (
                                Some(addr),
                                Some(interner.intern(r.function_name)),
                                Some(interner.intern(r.binary)),
                            ),
                            None => (Some(addr), None, None),
                        },
                        None => (None, None, None),
                    },
                    _ => (None, None, None),
                };
            entries.push(IntervalEntry {
                tid: event_tid,
                start_ns: raw.start_ns.saturating_sub(session_start),
                duration_ns: interval_ns,
                reason,
                waker_tid: *waker_tid,
                waker_address,
                waker_function_name,
                waker_binary,
            });
        }
    }

    // Most-recent first so the drill-down panel surfaces what's
    // happening *now* without the user scrolling past hours of
    // history.
    entries.sort_by(|a, b| b.start_ns.cmp(&a.start_ns));

    IntervalListUpdate {
        strings: interner.into_strings(),
        total_intervals,
        total_duration_ns,
        by_reason: by_reason.to_proto(),
        entries,
    }
}

fn build_pet_samples_update(
    agg: &Aggregator,
    tid: Option<u32>,
    filter: &LiveFilter,
) -> PetSampleListUpdate {
    let session_start = agg.session_start_ns().unwrap_or(0);
    let mut total_samples: u64 = 0;
    let mut entries: Vec<PetSampleEntry> = Vec::new();
    for (event_tid, sample) in agg.iter_pet_samples(tid) {
        if let Some(ref tr) = filter.time_range {
            let rel = sample.timestamp_ns.saturating_sub(session_start);
            if rel < tr.start_ns || rel >= tr.end_ns {
                continue;
            }
        }
        total_samples = total_samples.saturating_add(1);
        if entries.len() < DRILLDOWN_ENTRY_CAP {
            entries.push(PetSampleEntry {
                tid: event_tid,
                timestamp_ns: sample.timestamp_ns.saturating_sub(session_start),
                cycles: sample.pmc.cycles,
                instructions: sample.pmc.instructions,
                l1d_misses: sample.pmc.l1d_misses,
                branch_mispreds: sample.pmc.branch_mispreds,
            });
        }
    }
    entries.sort_by(|a, b| b.timestamp_ns.cmp(&a.timestamp_ns));
    PetSampleListUpdate {
        total_samples,
        entries,
    }
}

fn is_filter_empty(filter: &LiveFilter) -> bool {
    filter.time_range.is_none() && filter.exclude_symbols.is_empty()
}

/// Build the predicate `Aggregator::aggregate` runs against every PET
/// sample / interval. Captures the filter + recording origin so
/// time-range and exclude-symbol filters can both be applied in one
/// pass. On/off-CPU split is *not* a filter -- every node carries
/// both kinds in separate fields and the UI picks which one to show.
fn make_predicate<'a>(
    filter: &'a LiveFilter,
    session_start_ns: u64,
) -> impl FnMut(EventCtx<'_>) -> bool + 'a {
    use std::collections::HashSet;
    let exclude: HashSet<(Option<String>, Option<String>)> = filter
        .exclude_symbols
        .iter()
        .map(|s| (s.function_name.clone(), s.binary.clone()))
        .collect();
    move |ctx: EventCtx<'_>| {
        let (timestamp, stack_iter, binaries): (u64, Box<dyn Iterator<Item = u64>>, _) = match ctx {
            EventCtx::PetSample {
                tid: _,
                sample,
                binaries,
            } => (
                sample.timestamp_ns,
                Box::new(sample.stack.iter().copied().collect::<Vec<_>>().into_iter()),
                binaries,
            ),
            EventCtx::Interval {
                tid: _,
                interval,
                binaries,
            } => {
                let stack: Vec<u64> = match &interval.kind {
                    IntervalKind::OnCpu => Vec::new(),
                    IntervalKind::OffCpu { stack, .. } => stack.iter().copied().collect(),
                };
                (interval.start_ns, Box::new(stack.into_iter()), binaries)
            }
        };
        if let Some(ref tr) = filter.time_range {
            let rel = timestamp.saturating_sub(session_start_ns);
            if rel < tr.start_ns || rel >= tr.end_ns {
                return false;
            }
        }
        if !exclude.is_empty() {
            for addr in stack_iter {
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

fn group_top_entries(
    aggregation: &Aggregation,
    binaries: &BinaryRegistry,
    sort: TopSort,
    limit: usize,
) -> Vec<TopEntry> {
    use std::collections::HashMap;

    // Group key: (function_name, binary_basename). When unresolved
    // (no containing image), each address is its own group (keyed by
    // its hex form so it stays unique).
    struct Agg {
        address: u64,
        representative_self_ns: u64,
        self_on_cpu_ns: u64,
        total_on_cpu_ns: u64,
        self_off_cpu: OffCpuBreakdown,
        total_off_cpu: OffCpuBreakdown,
        self_pet_samples: u64,
        total_pet_samples: u64,
        self_off_cpu_intervals: u64,
        total_off_cpu_intervals: u64,
        self_pmc: PmcAccum,
        total_pmc: PmcAccum,
        function_name: Option<String>,
        binary: Option<String>,
        is_main: bool,
        language: stax_demangle::Language,
    }
    let mut groups: HashMap<(String, String), Agg> = HashMap::new();
    for (&address, stats) in &aggregation.by_address {
        let resolved = binaries.lookup_symbol(address);
        let (fn_name, bin, is_main, language) = match resolved {
            Some(r) => (Some(r.function_name), Some(r.binary), r.is_main, r.language),
            None => (None, None, false, stax_demangle::Language::Unknown),
        };
        let key: (String, String) = match (&fn_name, &bin) {
            (Some(n), Some(b)) => (n.clone(), b.clone()),
            _ => (format!("{:#x}", address), String::new()),
        };
        groups
            .entry(key)
            .and_modify(|g| {
                g.self_on_cpu_ns = g.self_on_cpu_ns.saturating_add(stats.self_on_cpu_ns);
                g.total_on_cpu_ns = g.total_on_cpu_ns.saturating_add(stats.total_on_cpu_ns);
                g.self_off_cpu.add_other(&stats.self_off_cpu);
                g.total_off_cpu.add_other(&stats.total_off_cpu);
                g.self_pet_samples = g.self_pet_samples.saturating_add(stats.self_pet_samples);
                g.total_pet_samples = g.total_pet_samples.saturating_add(stats.total_pet_samples);
                g.self_off_cpu_intervals = g
                    .self_off_cpu_intervals
                    .saturating_add(stats.self_off_cpu_intervals);
                g.total_off_cpu_intervals = g
                    .total_off_cpu_intervals
                    .saturating_add(stats.total_off_cpu_intervals);
                g.self_pmc.add_other(&stats.self_pmc);
                g.total_pmc.add_other(&stats.total_pmc);
                let candidate_self = stats.self_on_cpu_ns;
                if candidate_self > g.representative_self_ns {
                    g.address = address;
                    g.representative_self_ns = candidate_self;
                }
            })
            .or_insert(Agg {
                address,
                representative_self_ns: stats.self_on_cpu_ns,
                self_on_cpu_ns: stats.self_on_cpu_ns,
                total_on_cpu_ns: stats.total_on_cpu_ns,
                self_off_cpu: stats.self_off_cpu,
                total_off_cpu: stats.total_off_cpu,
                self_pet_samples: stats.self_pet_samples,
                total_pet_samples: stats.total_pet_samples,
                self_off_cpu_intervals: stats.self_off_cpu_intervals,
                total_off_cpu_intervals: stats.total_off_cpu_intervals,
                self_pmc: stats.self_pmc,
                total_pmc: stats.total_pmc,
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
            function_name: g.function_name,
            binary: g.binary,
            is_main: g.is_main,
            language: g.language.as_str().to_owned(),
            self_on_cpu_ns: g.self_on_cpu_ns,
            total_on_cpu_ns: g.total_on_cpu_ns,
            self_off_cpu: g.self_off_cpu.to_proto(),
            total_off_cpu: g.total_off_cpu.to_proto(),
            self_pet_samples: g.self_pet_samples,
            total_pet_samples: g.total_pet_samples,
            self_off_cpu_intervals: g.self_off_cpu_intervals,
            total_off_cpu_intervals: g.total_off_cpu_intervals,
            self_cycles: g.self_pmc.cycles,
            self_instructions: g.self_pmc.instructions,
            self_l1d_misses: g.self_pmc.l1d_misses,
            self_branch_mispreds: g.self_pmc.branch_mispreds,
            total_cycles: g.total_pmc.cycles,
            total_instructions: g.total_pmc.instructions,
            total_l1d_misses: g.total_pmc.l1d_misses,
            total_branch_mispreds: g.total_pmc.branch_mispreds,
        })
        .collect();
    // Tie-break on function_name → binary → address so the row order
    // is stable across snapshots; otherwise rows with equal durations
    // shuffle every tick as the underlying HashMap iterates them in
    // a different order.
    out.sort_by(|a, b| {
        let a_self_off = off_cpu_total_proto(&a.self_off_cpu);
        let b_self_off = off_cpu_total_proto(&b.self_off_cpu);
        let a_total_off = off_cpu_total_proto(&a.total_off_cpu);
        let b_total_off = off_cpu_total_proto(&b.total_off_cpu);
        let a_self = a.self_on_cpu_ns.saturating_add(a_self_off);
        let b_self = b.self_on_cpu_ns.saturating_add(b_self_off);
        let a_total = a.total_on_cpu_ns.saturating_add(a_total_off);
        let b_total = b.total_on_cpu_ns.saturating_add(b_total_off);
        let primary = match sort {
            TopSort::BySelf => b_self.cmp(&a_self).then_with(|| b_total.cmp(&a_total)),
            TopSort::ByTotal => b_total.cmp(&a_total).then_with(|| b_self.cmp(&a_self)),
        };
        primary
            .then_with(|| a.function_name.cmp(&b.function_name))
            .then_with(|| a.binary.cmp(&b.binary))
            .then_with(|| a.address.cmp(&b.address))
    });
    out.truncate(limit);
    out
}

/// Build the timeline by walking SCHED-derived intervals (the
/// authoritative source of "when was a thread doing what"). Each
/// interval's duration gets distributed across the buckets it
/// overlaps, split into on-CPU vs off-CPU stacks. PET samples
/// don't directly drive the timeline -- they're stack-only.
///
/// Bucket size is chosen so we stay around `TARGET_BUCKETS`
/// regardless of recording duration, with a sensible minimum so we
/// don't over-quantize a 1-second recording.
fn build_timeline_update(aggregator: &Arc<RwLock<Aggregator>>, tid: Option<u32>) -> TimelineUpdate {
    const TARGET_BUCKETS: u64 = 200;
    const MIN_BUCKET_NS: u64 = 50_000_000; // 50 ms

    let agg = aggregator.read();
    let start = agg.session_start_ns().unwrap_or(0);
    let last = agg.last_event_ns().unwrap_or(start);
    let recording_duration_ns = last.saturating_sub(start);

    let bucket_size_ns = if recording_duration_ns == 0 {
        MIN_BUCKET_NS
    } else {
        (recording_duration_ns / TARGET_BUCKETS).max(MIN_BUCKET_NS)
    };
    let n_buckets = ((recording_duration_ns / bucket_size_ns) + 1) as usize;
    let mut on_cpu_per_bucket: Vec<u64> = vec![0; n_buckets.max(1)];
    let mut off_cpu_per_bucket: Vec<u64> = vec![0; n_buckets.max(1)];

    let mut total_on_cpu_ns: u64 = 0;
    let mut total_off_cpu_ns: u64 = 0;

    for (_tid, interval) in agg.iter_intervals(tid) {
        let int_start = interval.start_ns;
        let int_end = if interval.end_ns == 0 {
            last
        } else {
            interval.end_ns
        };
        if int_end <= int_start {
            continue;
        }
        let on_cpu = matches!(interval.kind, IntervalKind::OnCpu);
        // Distribute the interval's duration across the buckets it
        // overlaps. For each overlapping bucket [b_start, b_end), the
        // share is min(int_end, b_end) - max(int_start, b_start).
        let rel_start = int_start.saturating_sub(start);
        let rel_end = int_end.saturating_sub(start);
        let first_bucket = (rel_start / bucket_size_ns) as usize;
        let last_bucket = ((rel_end.saturating_sub(1)) / bucket_size_ns) as usize;
        for b in first_bucket..=last_bucket.min(n_buckets.saturating_sub(1)) {
            let b_start = (b as u64) * bucket_size_ns;
            let b_end = b_start.saturating_add(bucket_size_ns);
            let share = b_end.min(rel_end).saturating_sub(b_start.max(rel_start));
            if share == 0 {
                continue;
            }
            if on_cpu {
                on_cpu_per_bucket[b] = on_cpu_per_bucket[b].saturating_add(share);
                total_on_cpu_ns = total_on_cpu_ns.saturating_add(share);
            } else {
                off_cpu_per_bucket[b] = off_cpu_per_bucket[b].saturating_add(share);
                total_off_cpu_ns = total_off_cpu_ns.saturating_add(share);
            }
        }
    }

    let buckets: Vec<TimelineBucket> = on_cpu_per_bucket
        .into_iter()
        .zip(off_cpu_per_bucket.into_iter())
        .enumerate()
        .map(|(i, (on_cpu_ns, off_cpu_ns))| TimelineBucket {
            start_ns: i as u64 * bucket_size_ns,
            on_cpu_ns,
            off_cpu_ns,
        })
        .collect();

    TimelineUpdate {
        bucket_size_ns,
        recording_duration_ns,
        total_on_cpu_ns,
        total_off_cpu_ns,
        buckets,
    }
}

/// Build the kcachegrind-style "family tree" view of a symbol.
///
/// We walk the call tree once and, for every node whose resolved
/// symbol matches the target, do two things:
///   1. Merge the entire ancestor chain into `callers_tree`,
///      growing outward toward `main`.
///   2. Merge the entire descendant subtree into `callees_tree`,
///      keyed by symbol (so recursion + multiple call sites collapse).
///
/// SymbolNode tracks both on-CPU time and the off-CPU breakdown so
/// the family-tree view shows the same dimensions as the main flame.
fn compute_neighbors_update(
    flame_root: &StackNode,
    binaries: &BinaryRegistry,
    target_address: u64,
) -> NeighborsUpdate {
    use std::collections::HashMap;

    type SymbolKey = (Option<String>, Option<String>);

    #[derive(Default)]
    struct SymbolNode {
        on_cpu_ns: u64,
        off_cpu: OffCpuBreakdown,
        pet_samples: u64,
        off_cpu_intervals: u64,
        rep_address: u64,
        rep_self_ns: u64,
        is_main: bool,
        language: stax_demangle::Language,
        children: HashMap<SymbolKey, SymbolNode>,
    }

    fn classify(addr: u64, bins: &BinaryRegistry) -> (SymbolKey, bool, stax_demangle::Language) {
        match bins.lookup_symbol(addr) {
            Some(r) => (
                (Some(r.function_name), Some(r.binary)),
                r.is_main,
                r.language,
            ),
            None => ((None, None), false, stax_demangle::Language::Unknown),
        }
    }

    /// Add the data from one source `StackNode` into a `SymbolNode`,
    /// updating its rep-address heuristic.
    fn accumulate(
        node: &mut SymbolNode,
        addr: u64,
        src: &StackNode,
        is_main: bool,
        language: stax_demangle::Language,
    ) {
        node.on_cpu_ns = node.on_cpu_ns.saturating_add(src.on_cpu_ns);
        node.off_cpu.add_other(&src.off_cpu);
        node.pet_samples = node.pet_samples.saturating_add(src.pet_samples);
        node.off_cpu_intervals = node.off_cpu_intervals.saturating_add(src.off_cpu_intervals);
        let candidate = src.on_cpu_ns;
        if candidate > node.rep_self_ns {
            node.rep_address = addr;
            node.rep_self_ns = candidate;
            node.is_main = is_main;
            node.language = language;
        }
    }

    fn merge_descendants(dst: &mut SymbolNode, src: &StackNode, bins: &BinaryRegistry) {
        for (caddr, child) in &src.children {
            let (key, is_main, language) = classify(*caddr, bins);
            let entry = dst.children.entry(key).or_default();
            accumulate(entry, *caddr, child, is_main, language);
            merge_descendants(entry, child, bins);
        }
    }

    fn walk(
        node: &StackNode,
        node_addr: u64,
        ancestors: &mut Vec<u64>,
        target_key: &SymbolKey,
        bins: &BinaryRegistry,
        callers: &mut SymbolNode,
        callees: &mut SymbolNode,
        own: &mut SymbolNode,
    ) {
        if node_addr != 0 {
            let (key, _is_main, _language) = classify(node_addr, bins);
            if &key == target_key {
                accumulate(own, node_addr, node, _is_main, _language);
                // Insert ancestor chain into callers_tree, innermost-first.
                let mut cur = &mut *callers;
                for &caller_addr in ancestors.iter().rev() {
                    let (ckey, cmain, clang) = classify(caller_addr, bins);
                    let entry = cur.children.entry(ckey).or_default();
                    accumulate(entry, caller_addr, node, cmain, clang);
                    cur = entry;
                }
                merge_descendants(callees, node, bins);
            }
        }

        let pushed = node_addr != 0;
        if pushed {
            ancestors.push(node_addr);
        }
        for (caddr, child) in &node.children {
            walk(
                child, *caddr, ancestors, target_key, bins, callers, callees, own,
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
        interner: &mut StringInterner,
    ) -> FlameNode {
        let SymbolNode {
            on_cpu_ns,
            off_cpu,
            pet_samples,
            off_cpu_intervals,
            rep_address,
            is_main,
            language,
            children,
            ..
        } = sn;
        // Sort by (on_cpu_ns desc, fname asc, bin asc) so order is
        // stable across snapshots.
        let mut entries: Vec<(SymbolKey, SymbolNode)> = children
            .into_iter()
            .filter(|(_, c)| c.on_cpu_ns.saturating_add(c.off_cpu.total_ns()) >= threshold)
            .collect();
        entries.sort_by(|a, b| {
            let a_total = a.1.on_cpu_ns.saturating_add(a.1.off_cpu.total_ns());
            let b_total = b.1.on_cpu_ns.saturating_add(b.1.off_cpu.total_ns());
            b_total
                .cmp(&a_total)
                .then_with(|| a.0.0.cmp(&b.0.0))
                .then_with(|| a.0.1.cmp(&b.0.1))
        });
        let child_nodes: Vec<FlameNode> = entries
            .into_iter()
            .map(|(k, c)| to_flame_node(c, k, threshold, interner))
            .collect();
        FlameNode {
            address: rep_address,
            on_cpu_ns,
            off_cpu: off_cpu.to_proto(),
            pet_samples,
            off_cpu_intervals,
            function_name: interner.intern_opt(key.0),
            binary: interner.intern_opt(key.1),
            is_main,
            language: interner.intern_str(language.as_str()),
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
        .unwrap_or(stax_demangle::Language::Unknown);

    let mut callers = SymbolNode::default();
    let mut callees = SymbolNode::default();
    let mut own = SymbolNode::default();

    let mut ancestors: Vec<u64> = Vec::new();
    walk(
        flame_root,
        0,
        &mut ancestors,
        &target_key,
        binaries,
        &mut callers,
        &mut callees,
        &mut own,
    );

    // Stamp the target's own data + representative onto each tree's
    // root so the renderer has a useful "self" frame.
    callers.on_cpu_ns = own.on_cpu_ns;
    callers.off_cpu = own.off_cpu;
    callers.pet_samples = own.pet_samples;
    callers.off_cpu_intervals = own.off_cpu_intervals;
    callers.rep_address = target_address;
    callers.is_main = target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false);
    callers.language = target_language;
    callees.on_cpu_ns = own.on_cpu_ns;
    callees.off_cpu = own.off_cpu;
    callees.pet_samples = own.pet_samples;
    callees.off_cpu_intervals = own.off_cpu_intervals;
    callees.rep_address = target_address;
    callees.is_main = target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false);
    callees.language = target_language;

    let own_total_ns = own.on_cpu_ns.saturating_add(own.off_cpu.total_ns());
    // Same lenient 0.05% threshold as the main flamegraph so the
    // family tree shows small but non-trivial neighbours.
    let threshold = (own_total_ns / 2000).max(1);
    let mut interner = StringInterner::new();
    let target_fname = interner.intern_opt(target_key.0.clone());
    let target_bin = interner.intern_opt(target_key.1.clone());
    let target_lang = interner.intern_str(target_language.as_str());
    let callers_tree = to_flame_node(callers, target_key.clone(), threshold, &mut interner);
    let callees_tree = to_flame_node(callees, target_key, threshold, &mut interner);

    NeighborsUpdate {
        strings: interner.into_strings(),
        function_name: target_fname,
        binary: target_bin,
        is_main: target_resolved.as_ref().map(|r| r.is_main).unwrap_or(false),
        language: target_lang,
        own_on_cpu_ns: own.on_cpu_ns,
        own_off_cpu: own.off_cpu.to_proto(),
        own_pet_samples: own.pet_samples,
        own_off_cpu_intervals: own.off_cpu_intervals,
        callers_tree,
        callees_tree,
    }
}

/// Tiny string-table builder shared between `compute_flame_update` and
/// `compute_neighbors_update`. Frees us from sending the same
/// `function_name` / `binary` / `language` strings for every node in
/// the tree -- a typical session has on the order of ~50 unique pairs
/// repeated across thousands of nodes.
struct StringInterner {
    strings: Vec<String>,
    index: std::collections::HashMap<String, u32>,
}

impl StringInterner {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            index: std::collections::HashMap::new(),
        }
    }

    fn intern_str(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.strings.len() as u32;
        let owned = s.to_owned();
        self.index.insert(owned.clone(), i);
        self.strings.push(owned);
        i
    }

    fn intern(&mut self, s: String) -> u32 {
        if let Some(&i) = self.index.get(&s) {
            return i;
        }
        let i = self.strings.len() as u32;
        self.index.insert(s.clone(), i);
        self.strings.push(s);
        i
    }

    fn intern_opt(&mut self, s: Option<String>) -> Option<u32> {
        s.map(|s| self.intern(s))
    }

    fn into_strings(self) -> Vec<String> {
        self.strings
    }
}

fn compute_flame_update(aggregation: &Aggregation, binaries: &BinaryRegistry) -> FlamegraphUpdate {
    let total_on_cpu_ns = aggregation.total_on_cpu_ns;
    let total_off_cpu = aggregation.total_off_cpu;
    let mut interner = StringInterner::new();
    let mut symbol_cache: HashMap<u64, ResolvedSymbol> = HashMap::new();
    let mut children = build_children(
        &[&aggregation.flame_root],
        binaries,
        &mut interner,
        &mut symbol_cache,
    );
    // build_children already returns children sorted; fold_recursion
    // only rewrites a node's children Vec, never the node's own data,
    // so the top-level order stays correct.
    for c in &mut children {
        fold_recursion(c);
    }

    let unknown_lang = interner.intern_str(stax_demangle::Language::Unknown.as_str());

    // Root sums counters across all children so the "(all)" row
    // shows the recording's grand totals.
    let total_cycles: u64 = children.iter().map(|c| c.cycles).sum();
    let total_instructions: u64 = children.iter().map(|c| c.instructions).sum();
    let total_l1d_misses: u64 = children.iter().map(|c| c.l1d_misses).sum();
    let total_branch_mispreds: u64 = children.iter().map(|c| c.branch_mispreds).sum();
    let total_pet_samples: u64 = children.iter().map(|c| c.pet_samples).sum();
    let total_off_cpu_intervals: u64 = children.iter().map(|c| c.off_cpu_intervals).sum();

    let all_label = interner.intern_str("(all)");
    let root = FlameNode {
        address: 0,
        on_cpu_ns: total_on_cpu_ns,
        off_cpu: total_off_cpu.to_proto(),
        pet_samples: total_pet_samples,
        off_cpu_intervals: total_off_cpu_intervals,
        function_name: Some(all_label),
        binary: None,
        is_main: false,
        language: unknown_lang,
        cycles: total_cycles,
        instructions: total_instructions,
        l1d_misses: total_l1d_misses,
        branch_mispreds: total_branch_mispreds,
        children,
    };
    FlamegraphUpdate {
        total_on_cpu_ns,
        total_off_cpu: total_off_cpu.to_proto(),
        strings: interner.into_strings(),
        root,
    }
}

/// Pull up any child whose symbol matches its parent: legitimately
/// recursive Rust functions, *and* the JIT-on-ARM64 case where kperf's
/// user-stack walker emits the live LR register as a phantom frame of
/// the same symbol any time a sample lands between two BL/BLR call
/// sites without a frame push. Either way, the same activation showing
/// up multiple times in the call tree is noise — flame graphs are
/// about flat per-symbol time, not per-PC. Splice the child's
/// children up so they sit directly under the parent, preserving the
/// surviving siblings' order. The cumulative `on_cpu_ns` on each
/// surviving node is unchanged; the folded child's contribution
/// becomes part of the parent's self-time.
fn fold_recursion(node: &mut FlameNode) {
    let mut i = 0;
    while i < node.children.len() {
        if symbol_eq(&node.children[i], node) {
            let child = node.children.remove(i);
            node.children.splice(i..i, child.children);
            // Re-examine at i — a spliced grandchild may itself match.
        } else {
            i += 1;
        }
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
/// symbol, and recurse. The siblings list lets us fold multiple
/// call-site addresses that map to the same symbol into one cell
/// without copying subtrees: callers below pass the borrowed
/// `StackNode`s of the merged group on to the recursive step.
///
/// Without this grouping, the flame is keyed by raw PC address —
/// recursive functions and any function called from multiple sites
/// fragment into a row of skinny same-name cells, and the same-name
/// children in the subtree never merge either. The neighbours view
/// already groups by symbol; the main flame now matches.
fn build_children(
    sources: &[&StackNode],
    binaries: &BinaryRegistry,
    interner: &mut StringInterner,
    symbol_cache: &mut HashMap<u64, ResolvedSymbol>,
) -> Vec<FlameNode> {
    type SymbolKey = (Option<String>, Option<String>);

    struct Acc<'a> {
        on_cpu_ns: u64,
        off_cpu: OffCpuBreakdown,
        pet_samples: u64,
        off_cpu_intervals: u64,
        pmc: PmcAccum,
        rep_addr: u64,
        rep_self_ns: u64,
        is_main: bool,
        language: stax_demangle::Language,
        sub_sources: Vec<&'a StackNode>,
    }

    /// Resolve an address to its (function_name, binary, is_main, language)
    /// tuple, caching results so repeated occurrences of the same address
    /// (common in recursive / widely-called functions) avoid redundant
    /// binary-registry lookups and string clones.
    fn resolve<'c>(
        cache: &'c mut HashMap<u64, ResolvedSymbol>,
        binaries: &BinaryRegistry,
        addr: u64,
    ) -> Option<&'c ResolvedSymbol> {
        use std::collections::hash_map::Entry;
        match cache.entry(addr) {
            Entry::Occupied(e) => Some(e.into_mut()),
            Entry::Vacant(e) => {
                let resolved = binaries.lookup_symbol(addr)?;
                Some(e.insert(resolved))
            }
        }
    }

    let mut groups: HashMap<SymbolKey, Acc> = HashMap::new();
    for src in sources {
        for (&addr, child) in &src.children {
            let resolved = resolve(symbol_cache, binaries, addr);
            let (fname, bin, is_main, lang) = match resolved {
                Some(r) => (
                    Some(&r.function_name),
                    Some(&r.binary),
                    r.is_main,
                    r.language,
                ),
                None => (None, None, false, stax_demangle::Language::Unknown),
            };
            let key = (fname.cloned(), bin.cloned());
            let acc = groups.entry(key).or_insert_with(|| Acc {
                on_cpu_ns: 0,
                off_cpu: OffCpuBreakdown::default(),
                pet_samples: 0,
                off_cpu_intervals: 0,
                pmc: PmcAccum::default(),
                rep_addr: addr,
                rep_self_ns: 0,
                is_main,
                language: lang,
                sub_sources: Vec::new(),
            });
            acc.on_cpu_ns = acc.on_cpu_ns.saturating_add(child.on_cpu_ns);
            acc.off_cpu.add_other(&child.off_cpu);
            acc.pet_samples = acc.pet_samples.saturating_add(child.pet_samples);
            acc.off_cpu_intervals = acc
                .off_cpu_intervals
                .saturating_add(child.off_cpu_intervals);
            acc.pmc.add_other(&child.pmc);
            // Largest single contributor's address is the click-through
            // representative; we rank by on_cpu_ns since that's the
            // most common attribution path.
            let candidate = child.on_cpu_ns;
            if candidate > acc.rep_self_ns {
                acc.rep_addr = addr;
                acc.rep_self_ns = candidate;
                acc.is_main = is_main;
                acc.language = lang;
            }
            acc.sub_sources.push(child);
        }
    }

    // Sort by (on+off duration desc, function_name asc, binary asc)
    // before interning so the visible order is stable across snapshots.
    let mut entries: Vec<((Option<String>, Option<String>), Acc)> = groups.into_iter().collect();
    entries.sort_by(|a, b| {
        let a_total = a.1.on_cpu_ns.saturating_add(a.1.off_cpu.total_ns());
        let b_total = b.1.on_cpu_ns.saturating_add(b.1.off_cpu.total_ns());
        b_total
            .cmp(&a_total)
            .then_with(|| a.0.0.cmp(&b.0.0))
            .then_with(|| a.0.1.cmp(&b.0.1))
    });

    let mut visible: Vec<FlameNode> = Vec::new();
    for ((fname, bin), acc) in entries {
        let grandchildren = build_children(&acc.sub_sources, binaries, interner, symbol_cache);
        visible.push(FlameNode {
            address: acc.rep_addr,
            on_cpu_ns: acc.on_cpu_ns,
            off_cpu: acc.off_cpu.to_proto(),
            pet_samples: acc.pet_samples,
            off_cpu_intervals: acc.off_cpu_intervals,
            function_name: interner.intern_opt(fname),
            binary: interner.intern_opt(bin),
            is_main: acc.is_main,
            language: interner.intern_str(acc.language.as_str()),
            cycles: acc.pmc.cycles,
            instructions: acc.pmc.instructions,
            l1d_misses: acc.pmc.l1d_misses,
            branch_mispreds: acc.pmc.branch_mispreds,
            children: grandchildren,
        });
    }
    visible
}

/// `self_lookup` returns `(self_on_cpu_ns, self_pet_samples)` for an
/// address, used to populate the CFG block heatmap.
fn compute_cfg_view(
    binaries: &Arc<RwLock<binaries::BinaryRegistry>>,
    address: u64,
    self_lookup: impl Fn(u64) -> (u64, u64),
) -> CfgUpdate {
    let resolved = binaries.write().resolve(address);
    match resolved {
        Some(r) => {
            let function_name = r.function_name.clone();
            let language = r.language.as_str().to_owned();
            cfg::compute_cfg_update(&r, address, function_name, language, self_lookup)
        }
        None => CfgUpdate {
            function_name: format!("(no binary mapped at {:#x})", address),
            language: stax_demangle::Language::Unknown.as_str().to_owned(),
            base_address: address,
            queried_address: address,
            blocks: Vec::new(),
            edges: Vec::new(),
        },
    }
}

/// `self_lookup` returns `(self_on_cpu_ns, self_pet_samples)` for an
/// address, used to populate the annotated disassembly heatmap.
fn compute_annotated_view(
    binaries: &Arc<RwLock<BinaryRegistry>>,
    source: &Arc<parking_lot::Mutex<source::SourceResolver>>,
    address: u64,
    self_lookup: impl Fn(u64) -> (u64, u64),
) -> AnnotatedView {
    let resolved = binaries.write().resolve(address);

    let mut hl = highlight::TokenHighlighter::new();
    let mut lines: Vec<AnnotatedLine> = match &resolved {
        Some(r) => disassemble::disassemble(r, &mut hl, |addr| self_lookup(addr)),
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
                    let tokens = src.snippet(file, ln);
                    line.source_header = Some(stax_live_proto::SourceHeader {
                        file: file.clone(),
                        line: ln,
                        tokens,
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
        .unwrap_or(stax_demangle::Language::Unknown);
    let base_address = resolved.as_ref().map(|r| r.base_address).unwrap_or(address);
    AnnotatedView {
        function_name,
        language: language.as_str().to_owned(),
        base_address,
        queried_address: address,
        lines,
    }
}
