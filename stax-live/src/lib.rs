//! Aggregator + binary registry + Profiler service impl. Embedded
//! into stax-server, which feeds them via the wire-side ingest path.
//! There used to be an in-process `--serve` aggregator entry point
//! here too; that's been deleted.

use std::sync::Arc;

use parking_lot::RwLock;

use stax_live_proto::{
    AnnotatedLine, AnnotatedView, FlameNode, FlamegraphUpdate, IntervalEntry, IntervalListUpdate,
    LiveFilter, NeighborsUpdate, PetSampleEntry, PetSampleListUpdate, ProbeDiffBucket,
    ProbeDiffDepthCell, ProbeDiffEntry, ProbeDiffThread, ProbeDiffUpdate, Profiler, ResolvedFrame,
    STITCH_MIN_SUFFIX, ThreadInfo, ThreadsUpdate, TimelineBucket, TimelineUpdate, TopEntry,
    TopSort, TopUpdate, ViewParams,
};

pub use crate::aggregator::{IntervalKind, PmuSample};
use crate::aggregator::{Aggregation, EventCtx, OffCpuBreakdown, PmcAccum, StackNode};

mod aggregator;
mod binaries;
mod classify;
mod disassemble;
mod highlight;
pub mod source;

pub use aggregator::{Aggregator, ProbeResultRecord};
pub use binaries::{BinaryRegistry, LiveSymbolOwned, LoadedBinary};


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
        let aggregation = aggregate_with_filter(&agg, &bins, params.tid, &params.filter);
        group_top_entries(&aggregation, &bins, sort, limit as usize)
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
        output: vox::Tx<stax_live_proto::WakersUpdate>,
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
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
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
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
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

    async fn subscribe_probe_diff(
        &self,
        tid: Option<u32>,
        output: vox::Tx<ProbeDiffUpdate>,
    ) {
        tracing::info!(?tid, "subscribe_probe_diff: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = {
                    let agg = aggregator.read();
                    let bins = binaries.read();
                    build_probe_diff_update(&agg, &bins, tid)
                };
                if let Err(e) = output.send(update).await {
                    tracing::info!("subscribe_probe_diff: stream ended: {e:?}");
                    break;
                }
            }
        });
    }

    async fn subscribe_threads(&self, output: vox::Tx<ThreadsUpdate>) {
        tracing::info!("subscribe_threads: starting stream");
        let aggregator = self.aggregator.clone();
        let binaries = self.binaries.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let update = {
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
                    // Rank by on_cpu_ns + total off-cpu so active +
                    // heavily-blocked threads both float to the top.
                    threads.sort_by(|a, b| {
                        let a_total = a.on_cpu_ns.saturating_add(off_cpu_total_proto(&a.off_cpu));
                        let b_total = b.on_cpu_ns.saturating_add(off_cpu_total_proto(&b.off_cpu));
                        b_total.cmp(&a_total)
                    });
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
            let (waker_address, waker_function_name, waker_binary) = match (
                waker_tid,
                waker_user_stack.as_deref(),
            ) {
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

/// Bucket boundaries for the probe drift histogram, in real
/// nanoseconds (server converts mach ticks via mach_timebase).
const PROBE_DRIFT_BUCKETS_NS: &[u64] =
    &[1_000, 10_000, 100_000, 1_000_000, 10_000_000, 100_000_000];

const PROBE_RECENT_CAP: usize = 64;
const PROBE_DEPTH_CAP: usize = 32;
const PROBE_THREADS_CAP: usize = 32;

/// Look up the host's mach_timebase ratio once. Apple Silicon
/// reports 1:1 (so ticks == ns) but x86 macOS uses different
/// numer/denom. Cached per-process.
#[cfg(target_os = "macos")]
fn mach_timebase_numer_denom() -> (u32, u32) {
    use std::sync::OnceLock;
    static TB: OnceLock<(u32, u32)> = OnceLock::new();
    *TB.get_or_init(|| {
        // SAFETY: out-pointer to a stack-local of the right shape.
        let mut info: libc::mach_timebase_info = unsafe { std::mem::zeroed() };
        let _ = unsafe { libc::mach_timebase_info(&mut info) };
        if info.denom == 0 {
            (1, 1)
        } else {
            (info.numer, info.denom)
        }
    })
}

#[cfg(not(target_os = "macos"))]
fn mach_timebase_numer_denom() -> (u32, u32) {
    (1, 1)
}

fn ticks_to_ns(ticks: i128) -> i128 {
    let (numer, denom) = mach_timebase_numer_denom();
    if denom == 0 {
        ticks
    } else {
        ticks * (numer as i128) / (denom as i128)
    }
}

/// Pair each kperf PET sample with its matching race-against-return
/// probe result by `(tid, timestamp_ns == kperf_ts)`. Walks both
/// per-thread queues with a two-pointer merge: both queues are
/// append-ordered by timestamp, so a single linear scan finds every
/// pair *and* counts the unmatched ones on either side. Resolves
/// addresses through the live `BinaryRegistry` so the UI sees the
/// same module!symbol strings the flame graph does — one resolver
/// for everything.
fn build_probe_diff_update(
    agg: &Aggregator,
    bins: &BinaryRegistry,
    only_tid: Option<u32>,
) -> ProbeDiffUpdate {
    let session_start = agg.session_start_ns().unwrap_or(0);
    let mut hist = vec![0u64; 33];
    let mut bucket_count = vec![0u64; PROBE_DRIFT_BUCKETS_NS.len() + 1];
    let mut bucket_pc_match = vec![0u64; PROBE_DRIFT_BUCKETS_NS.len() + 1];
    let mut depth_match = vec![0u64; PROBE_DEPTH_CAP];
    let mut depth_total = vec![0u64; PROBE_DEPTH_CAP];

    let mut total_kperf: u64 = 0;
    let mut total_probes: u64 = 0;
    let mut paired: u64 = 0;
    let mut kperf_only: u64 = 0;
    let mut probe_only: u64 = 0;
    let mut probe_augmented: u64 = 0;
    let mut probe_deeper: u64 = 0;
    let mut pc_match_n: u64 = 0;
    let mut stitchable_n: u64 = 0;
    let mut framehop_n: u64 = 0;
    let mut fp_walk_n: u64 = 0;

    let mut threads_summary: Vec<ProbeDiffThread> = Vec::new();
    let mut recent: std::collections::VecDeque<ProbeDiffEntry> =
        std::collections::VecDeque::with_capacity(PROBE_RECENT_CAP);

    for thread_tid in agg.iter_threads() {
        if let Some(want) = only_tid {
            if thread_tid != want {
                continue;
            }
        }
        let Some(stats) = agg.thread_stats(thread_tid) else {
            continue;
        };
        total_kperf = total_kperf.saturating_add(stats.pet_samples.len() as u64);
        total_probes = total_probes.saturating_add(stats.probe_results.len() as u64);

        // Per-thread accumulators so we can return a per-thread
        // breakdown without a second pass.
        let mut t_paired: u64 = 0;
        let mut t_pc_match: u64 = 0;
        let mut t_stitchable: u64 = 0;
        let mut t_common_total: u64 = 0;

        // Two-pointer merge over the two append-ordered queues.
        let mut pet_iter = stats.pet_samples.iter().peekable();
        let mut probe_iter = stats.probe_results.iter().peekable();
        loop {
            match (pet_iter.peek(), probe_iter.peek()) {
                (None, None) => break,
                (Some(_), None) => {
                    pet_iter.next();
                    kperf_only += 1;
                }
                (None, Some(_)) => {
                    probe_iter.next();
                    probe_only += 1;
                }
                (Some(pet), Some(probe)) => {
                    if pet.timestamp_ns < probe.kperf_ts {
                        pet_iter.next();
                        kperf_only += 1;
                        continue;
                    }
                    if pet.timestamp_ns > probe.kperf_ts {
                        probe_iter.next();
                        probe_only += 1;
                        continue;
                    }
                    // Pair found.
                    let pet = pet_iter.next().unwrap();
                    let probe = probe_iter.next().unwrap();

                    paired += 1;
                    t_paired += 1;
                    if probe.used_framehop {
                        framehop_n += 1;
                    } else {
                        fp_walk_n += 1;
                    }

                    // kperf's user_backtrace shape after parser
                    // truncation: [leaf, walked_ret0, ..., walked_retN].
                    // The appended-LR is gone (NULL-truncated). So
                    // pet.stack[1..] is the FP-walked chain that
                    // matches probe.mach_walked in shape.
                    let kperf_walk: &[u64] = if !pet.stack.is_empty() {
                        &pet.stack[1..]
                    } else {
                        &[]
                    };
                    let probe_walk = &probe.mach_walked[..];

                    let common = longest_common_suffix(kperf_walk, probe_walk);
                    let bucket = common.min(hist.len() - 1);
                    hist[bucket] += 1;
                    t_common_total = t_common_total.saturating_add(common as u64);

                    // Per-depth match heatmap. Counted from the leaf:
                    // depth 0 = leaf PC (kperf[0] vs mach_pc),
                    // depth d = walked frame d-1 (kperf[d] vs mach_walked[d-1]).
                    let probe_full_depth = 1 + probe_walk.len();
                    let pet_full_depth = pet.stack.len();
                    let max_depth = pet_full_depth.min(probe_full_depth).min(PROBE_DEPTH_CAP);
                    for d in 0..max_depth {
                        depth_total[d] += 1;
                        let kperf_frame = pet.stack[d];
                        let probe_frame = if d == 0 { probe.mach_pc } else { probe_walk[d - 1] };
                        if kperf_frame == probe_frame {
                            depth_match[d] += 1;
                        }
                    }

                    // Probe value-add: kperf walked nothing usable
                    // (parked thread, FP=0 at PMI) but probe got
                    // ≥1 frame; or probe walked strictly deeper
                    // than kperf.
                    if pet_full_depth <= 1 && probe_full_depth >= 2 {
                        probe_augmented += 1;
                    }
                    if probe_full_depth > pet_full_depth {
                        probe_deeper += 1;
                    }

                    // Drift bucket (real ns).
                    let drift_ns_signed =
                        ticks_to_ns((probe.probe_done_ns as i128) - (probe.kperf_ts as i128));
                    let drift_abs_ns = drift_ns_signed.unsigned_abs() as u64;
                    let bucket_idx = PROBE_DRIFT_BUCKETS_NS
                        .iter()
                        .position(|&edge| drift_abs_ns < edge)
                        .unwrap_or(PROBE_DRIFT_BUCKETS_NS.len());
                    bucket_count[bucket_idx] += 1;

                    let kperf_leaf = pet.stack.first().copied().unwrap_or(0);
                    let pc_match = kperf_leaf == probe.mach_pc;
                    if pc_match {
                        pc_match_n += 1;
                        t_pc_match += 1;
                        bucket_pc_match[bucket_idx] += 1;
                    }
                    let stitchable = pc_match && (common as u32) >= STITCH_MIN_SUFFIX;
                    if stitchable {
                        stitchable_n += 1;
                        t_stitchable += 1;
                    }

                    // Drill-down: keep the most recent N entries
                    // with both stacks symbolicated. Stitched stack
                    // is the actual deliverable: probe's view since
                    // we've validated the leaf and the deep tail
                    // both agree with kperf — we ship the live-
                    // thread walk because it's atomic-from-target
                    // and we know it's coherent with the PMI sample.
                    let kperf_stack: Vec<ResolvedFrame> = pet
                        .stack
                        .iter()
                        .map(|&addr| resolve_frame(bins, addr))
                        .collect();
                    let kperf_kernel_stack: Vec<ResolvedFrame> = pet
                        .kernel_stack
                        .iter()
                        .map(|&addr| resolve_frame(bins, addr))
                        .collect();
                    let mut probe_stack: Vec<ResolvedFrame> =
                        Vec::with_capacity(probe_walk.len() + 1);
                    probe_stack.push(resolve_frame(bins, probe.mach_pc));
                    for &addr in probe_walk {
                        probe_stack.push(resolve_frame(bins, addr));
                    }
                    let stitched_stack: Vec<ResolvedFrame> = if stitchable {
                        probe_stack.clone()
                    } else {
                        Vec::new()
                    };

                    let entry = ProbeDiffEntry {
                        tid: thread_tid,
                        timestamp_ns: pet.timestamp_ns.saturating_sub(session_start),
                        drift_ns: drift_ns_signed
                            .clamp(i64::MIN as i128, i64::MAX as i128)
                            as i64,
                        kperf_stack,
                        kperf_kernel_stack,
                        probe_stack,
                        stitched_stack,
                        common_suffix: common as u32,
                        pc_match,
                        stitchable,
                        used_framehop: probe.used_framehop,
                    };
                    if recent.len() == PROBE_RECENT_CAP {
                        recent.pop_front();
                    }
                    recent.push_back(entry);
                }
            }
        }

        if t_paired > 0 {
            threads_summary.push(ProbeDiffThread {
                tid: thread_tid,
                paired: t_paired,
                pc_match: t_pc_match,
                stitchable: t_stitchable,
                avg_common_suffix: t_common_total as f32 / t_paired as f32,
                thread_name: agg.thread_name(thread_tid).map(|s| s.to_owned()),
            });
        }
    }

    threads_summary.sort_by(|a, b| b.paired.cmp(&a.paired));
    threads_summary.truncate(PROBE_THREADS_CAP);

    let mut drift_buckets: Vec<ProbeDiffBucket> =
        Vec::with_capacity(PROBE_DRIFT_BUCKETS_NS.len() + 1);
    for (i, &edge) in PROBE_DRIFT_BUCKETS_NS.iter().enumerate() {
        drift_buckets.push(ProbeDiffBucket {
            upper_ns: edge,
            samples: bucket_count[i],
            pc_match: bucket_pc_match[i],
        });
    }
    drift_buckets.push(ProbeDiffBucket {
        upper_ns: u64::MAX,
        samples: bucket_count[PROBE_DRIFT_BUCKETS_NS.len()],
        pc_match: bucket_pc_match[PROBE_DRIFT_BUCKETS_NS.len()],
    });

    let depth_cells: Vec<ProbeDiffDepthCell> = depth_total
        .iter()
        .zip(depth_match.iter())
        .enumerate()
        .filter(|(_, (total, _))| **total > 0)
        .map(|(d, (total, matched))| ProbeDiffDepthCell {
            depth: d as u32,
            matched: *matched,
            total: *total,
        })
        .collect();

    ProbeDiffUpdate {
        total_kperf_samples: total_kperf,
        total_probes,
        paired,
        kperf_only,
        probe_only,
        probe_augmented_kperf: probe_augmented,
        probe_walked_deeper: probe_deeper,
        common_suffix_hist: hist,
        depth_match: depth_cells,
        drift_buckets,
        pc_match: pc_match_n,
        stitchable: stitchable_n,
        framehop_used: framehop_n,
        fp_walk_used: fp_walk_n,
        threads: threads_summary,
        recent: recent.into_iter().collect(),
    }
}

/// Length of the longest matching suffix between two address
/// slices. `a[a.len()-1]` lines up with `b[b.len()-1]`; the count
/// is how many trailing frames match.
fn longest_common_suffix(a: &[u64], b: &[u64]) -> usize {
    let mut k = 0;
    let max_k = a.len().min(b.len());
    while k < max_k {
        if a[a.len() - 1 - k] != b[b.len() - 1 - k] {
            break;
        }
        k += 1;
    }
    k
}

/// Render one address as a `ResolvedFrame` using the live registry.
/// Falls back to `<unmapped:0xaddr>` when no module covers the
/// address (typical for jit code that didn't fire a BinaryLoaded
/// event yet).
fn resolve_frame(bins: &BinaryRegistry, address: u64) -> ResolvedFrame {
    if address == 0 {
        return ResolvedFrame {
            address,
            display: "<null>".to_owned(),
            binary: String::new(),
            function: String::new(),
        };
    }
    match bins.lookup_symbol(address) {
        Some(sym) => ResolvedFrame {
            address,
            display: format!("{}!{}", sym.binary, sym.function_name),
            binary: sym.binary,
            function: sym.function_name,
        },
        None => ResolvedFrame {
            address,
            display: format!("<unmapped:{address:#x}>"),
            binary: String::new(),
            function: String::new(),
        },
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
                g.total_on_cpu_ns =
                    g.total_on_cpu_ns.saturating_add(stats.total_on_cpu_ns);
                g.self_off_cpu.add_other(&stats.self_off_cpu);
                g.total_off_cpu.add_other(&stats.total_off_cpu);
                g.self_pet_samples =
                    g.self_pet_samples.saturating_add(stats.self_pet_samples);
                g.total_pet_samples =
                    g.total_pet_samples.saturating_add(stats.total_pet_samples);
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
fn build_timeline_update(
    aggregator: &Arc<RwLock<Aggregator>>,
    tid: Option<u32>,
) -> TimelineUpdate {
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
        let int_end = if interval.end_ns == 0 { last } else { interval.end_ns };
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

    fn classify(
        addr: u64,
        bins: &BinaryRegistry,
    ) -> (SymbolKey, bool, stax_demangle::Language) {
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
        node.off_cpu_intervals = node
            .off_cpu_intervals
            .saturating_add(src.off_cpu_intervals);
        let candidate = src.on_cpu_ns;
        if candidate > node.rep_self_ns {
            node.rep_address = addr;
            node.rep_self_ns = candidate;
            node.is_main = is_main;
            node.language = language;
        }
    }

    fn merge_descendants(
        dst: &mut SymbolNode,
        src: &StackNode,
        bins: &BinaryRegistry,
    ) {
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
            walk(child, *caddr, ancestors, target_key, bins, callers, callees, own);
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

fn compute_flame_update(
    aggregation: &Aggregation,
    binaries: &BinaryRegistry,
) -> FlamegraphUpdate {
    let total_on_cpu_ns = aggregation.total_on_cpu_ns;
    let total_off_cpu = aggregation.total_off_cpu;
    let total_combined = total_on_cpu_ns.saturating_add(total_off_cpu.total_ns());
    // 0.05% of total. Lower than the previous 0.5% so that when the
    // user focuses into a smaller subtree there's still meaningful
    // per-callsite detail instead of one big "(N small frames)" cell.
    // Bumps the wire payload roughly 5-10x but the live UI handles
    // it; the residue cell still catches the truly-tiny tail.
    let threshold = (total_combined / 2000).max(1);
    let mut interner = StringInterner::new();
    let (mut children, residue) =
        build_children_with_residue(&[&aggregation.flame_root], threshold, binaries, &mut interner);
    // build_children_with_residue already returns children sorted;
    // fold_recursion only rewrites a node's children Vec, never the
    // node's own data, so the top-level order stays correct.
    for c in &mut children {
        fold_recursion(c);
    }
    if let Some(extra) = residue {
        children.push(extra);
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
/// symbol, apply a duration threshold, and recurse. The siblings list
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
    sources: &[&StackNode],
    threshold: u64,
    binaries: &BinaryRegistry,
    interner: &mut StringInterner,
) -> (Vec<FlameNode>, Option<FlameNode>) {
    use std::collections::HashMap;

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
                None => (None, None, false, stax_demangle::Language::Unknown),
            };
            let key = (fname, bin);
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
    let mut residue_on_cpu_ns: u64 = 0;
    let mut residue_off_cpu = OffCpuBreakdown::default();
    let mut residue_pet_samples: u64 = 0;
    let mut residue_off_cpu_intervals: u64 = 0;
    let mut residue_dropped: u64 = 0;
    for ((fname, bin), acc) in entries {
        let acc_total = acc.on_cpu_ns.saturating_add(acc.off_cpu.total_ns());
        if acc_total >= threshold {
            let (mut grandchildren, gres) =
                build_children_with_residue(&acc.sub_sources, threshold, binaries, interner);
            if let Some(extra) = gres {
                grandchildren.push(extra);
            }
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
        } else {
            residue_on_cpu_ns = residue_on_cpu_ns.saturating_add(acc.on_cpu_ns);
            residue_off_cpu.add_other(&acc.off_cpu);
            residue_pet_samples = residue_pet_samples.saturating_add(acc.pet_samples);
            residue_off_cpu_intervals = residue_off_cpu_intervals
                .saturating_add(acc.off_cpu_intervals);
            residue_dropped += 1;
        }
    }
    let residue_total = residue_on_cpu_ns.saturating_add(residue_off_cpu.total_ns());
    let residue = if residue_total > 0 && residue_dropped > 0 {
        let label = interner.intern(format!("({} small frames)", residue_dropped));
        let unknown_lang = interner.intern_str(stax_demangle::Language::Unknown.as_str());
        Some(FlameNode {
            // Sentinel address: the frontend treats any node with
            // address == 0 as the root "(all)" otherwise; we set
            // function_name explicitly so the labelFor() takes that
            // path first, but we also pick u64::MAX here as a defence
            // in case future renderers fall back to address.
            address: u64::MAX,
            on_cpu_ns: residue_on_cpu_ns,
            off_cpu: residue_off_cpu.to_proto(),
            pet_samples: residue_pet_samples,
            off_cpu_intervals: residue_off_cpu_intervals,
            function_name: Some(label),
            binary: None,
            is_main: false,
            language: unknown_lang,
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

/// `self_lookup` returns `(self_on_cpu_ns, self_pet_samples)` for an
/// address, used to populate the annotated disassembly heatmap.
fn compute_annotated_view(
    binaries: &Arc<RwLock<BinaryRegistry>>,
    source: &Arc<parking_lot::Mutex<source::SourceResolver>>,
    address: u64,
    self_lookup: impl Fn(u64) -> (u64, u64),
) -> AnnotatedView {
    let resolved = binaries.write().resolve(address);

    let mut hl = highlight::AsmHighlighter::new();
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
                    let html = src.snippet(file, ln);
                    line.source_header = Some(stax_live_proto::SourceHeader {
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
