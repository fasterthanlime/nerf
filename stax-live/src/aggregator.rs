//! Per-thread raw event log + on-demand attribution.
//!
//! The aggregator is event-sourced: nothing is pre-aggregated. We
//! keep two raw streams per thread:
//!
//! - `pet_samples`: stack-walks taken by kperf's PET timer at moments
//!   when the thread was *actually running* on a CPU (lightweight_pet
//!   only samples on-CPU threads). Source of stack identity.
//! - `intervals`: SCHED-derived (start, end, kind) tuples telling us
//!   exactly when the thread was on or off a CPU. Source of *time*.
//!
//! Aggregation walks both streams together. Each on-CPU interval's
//! duration is distributed evenly across the PET samples that fell
//! inside it; that's how we credit *time* to *stacks*. Each off-CPU
//! interval is attributed in full to the stack the thread was on at
//! the moment it went off-CPU, classified by leaf frame into one of
//! the `OffCpuReason` buckets.
//!
//! The flame snapshot is recomputed every refresh tick (~500ms).
//! That's a fresh O(samples + intervals × stack-depth) walk per query
//! -- acceptable for the live UI's cadence given a per-thread cap of
//! ~100k of each.

use std::borrow::Cow;
use std::collections::HashMap;

use stax_live_proto::{OffCpuReason, STITCH_MIN_SUFFIX};

use crate::binaries::BinaryRegistry;
use crate::classify::classify_offcpu;
use crate::probe_match::{
    PROBE_PAIR_WINDOW_NS, abs_tick_delta_ns, logical_probe_stack, longest_common_run,
};

#[derive(Clone, Copy, Default)]
pub struct PmcAccum {
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

impl PmcAccum {
    pub fn add(&mut self, s: &PmuSample) {
        self.cycles = self.cycles.saturating_add(s.cycles);
        self.instructions = self.instructions.saturating_add(s.instructions);
        self.l1d_misses = self.l1d_misses.saturating_add(s.l1d_misses);
        self.branch_mispreds = self.branch_mispreds.saturating_add(s.branch_mispreds);
    }

    pub fn add_other(&mut self, other: &PmcAccum) {
        self.cycles = self.cycles.saturating_add(other.cycles);
        self.instructions = self.instructions.saturating_add(other.instructions);
        self.l1d_misses = self.l1d_misses.saturating_add(other.l1d_misses);
        self.branch_mispreds = self.branch_mispreds.saturating_add(other.branch_mispreds);
    }
}

/// Per-sample PMU values handed in via `Aggregator::record_pet_sample`.
#[derive(Clone, Copy, Default)]
pub struct PmuSample {
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

/// Off-CPU duration broken down by why the thread was off-CPU. Mirrors
/// `stax_live_proto::OffCpuBreakdown` but lives here so the aggregator
/// can credit reasons without dragging in proto types throughout.
#[derive(Clone, Copy, Default, Debug)]
pub struct OffCpuBreakdown {
    pub idle_ns: u64,
    pub lock_ns: u64,
    pub semaphore_ns: u64,
    pub ipc_ns: u64,
    pub io_read_ns: u64,
    pub io_write_ns: u64,
    pub readiness_ns: u64,
    pub sleep_ns: u64,
    pub connect_ns: u64,
    pub other_ns: u64,
}

impl OffCpuBreakdown {
    pub fn add_reason(&mut self, reason: OffCpuReason, duration_ns: u64) {
        let slot: &mut u64 = match reason {
            OffCpuReason::Idle => &mut self.idle_ns,
            OffCpuReason::LockWait => &mut self.lock_ns,
            OffCpuReason::SemaphoreWait => &mut self.semaphore_ns,
            OffCpuReason::IpcWait => &mut self.ipc_ns,
            OffCpuReason::IoRead => &mut self.io_read_ns,
            OffCpuReason::IoWrite => &mut self.io_write_ns,
            OffCpuReason::Readiness => &mut self.readiness_ns,
            OffCpuReason::Sleep => &mut self.sleep_ns,
            OffCpuReason::ConnectionSetup => &mut self.connect_ns,
            OffCpuReason::Other => &mut self.other_ns,
        };
        *slot = slot.saturating_add(duration_ns);
    }

    pub fn add_other(&mut self, other: &OffCpuBreakdown) {
        self.idle_ns = self.idle_ns.saturating_add(other.idle_ns);
        self.lock_ns = self.lock_ns.saturating_add(other.lock_ns);
        self.semaphore_ns = self.semaphore_ns.saturating_add(other.semaphore_ns);
        self.ipc_ns = self.ipc_ns.saturating_add(other.ipc_ns);
        self.io_read_ns = self.io_read_ns.saturating_add(other.io_read_ns);
        self.io_write_ns = self.io_write_ns.saturating_add(other.io_write_ns);
        self.readiness_ns = self.readiness_ns.saturating_add(other.readiness_ns);
        self.sleep_ns = self.sleep_ns.saturating_add(other.sleep_ns);
        self.connect_ns = self.connect_ns.saturating_add(other.connect_ns);
        self.other_ns = self.other_ns.saturating_add(other.other_ns);
    }

    pub fn total_ns(&self) -> u64 {
        self.idle_ns
            .saturating_add(self.lock_ns)
            .saturating_add(self.semaphore_ns)
            .saturating_add(self.ipc_ns)
            .saturating_add(self.io_read_ns)
            .saturating_add(self.io_write_ns)
            .saturating_add(self.readiness_ns)
            .saturating_add(self.sleep_ns)
            .saturating_add(self.connect_ns)
            .saturating_add(self.other_ns)
    }

    pub fn to_proto(&self) -> stax_live_proto::OffCpuBreakdown {
        stax_live_proto::OffCpuBreakdown {
            idle_ns: self.idle_ns,
            lock_ns: self.lock_ns,
            semaphore_ns: self.semaphore_ns,
            ipc_ns: self.ipc_ns,
            io_read_ns: self.io_read_ns,
            io_write_ns: self.io_write_ns,
            readiness_ns: self.readiness_ns,
            sleep_ns: self.sleep_ns,
            connect_ns: self.connect_ns,
            other_ns: self.other_ns,
        }
    }
}

/// One PET stack-walk hit. Stacks are leaf-first.
pub struct PetSample {
    pub timestamp_ns: u64,
    pub stack: Box<[u64]>,
    /// Kernel stack at PMI, leaf-first. Empty if kperf interrupted
    /// user code (no kstack walked) or if the walk failed.
    pub kernel_stack: Box<[u64]>,
    pub pmc: PmuSample,
}

/// Correlation probe output. `kperf_ts` is the independent probe
/// request timestamp and pairs by nearest PET timestamp at query time.
pub struct ProbeResultRecord {
    pub tid: u32,
    pub timing: ProbeTiming,
    pub queue: ProbeQueueStats,
    pub mach_pc: u64,
    pub mach_lr: u64,
    pub mach_fp: u64,
    pub mach_sp: u64,
    /// Frame-pointer walked return addresses from the suspended
    /// thread, leaf-most first; PAC-stripped; does not include the
    /// leaf PC. This is compared against kperf for validation.
    pub mach_walked: Box<[u64]>,
    /// Compact-unwind-only return addresses from the same captured stack.
    pub compact_walked: Box<[u64]>,
    /// Compact-unwind return addresses, following compact DWARF FDE
    /// references.
    pub compact_dwarf_walked: Box<[u64]>,
    /// DWARF-unwound return addresses from the same captured stack.
    /// This is the candidate stitch stack when FP validation passes.
    pub dwarf_walked: Box<[u64]>,
    /// `true` if `dwarf_walked` is available for this capture.
    pub used_framehop: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeTiming {
    /// Mach-tick pairing key. For triggered probes this is the
    /// matching kperf sample timestamp; for correlation probes this
    /// is the independent probe request timestamp.
    pub kperf_ts: u64,
    pub staxd_read_started: u64,
    pub staxd_drained: u64,
    pub staxd_queued_for_send: u64,
    pub staxd_send_started: u64,
    pub client_received: u64,
    pub enqueued: u64,
    pub worker_started: u64,
    pub thread_lookup_done: u64,
    /// Mach-tick timestamp at which `thread_get_state` completed.
    /// Drift = kperf_ts → state_done.
    pub state_done: u64,
    pub resume_done: u64,
    pub walk_done: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeQueueStats {
    pub coalesced_requests: u64,
    pub worker_batch_len: u32,
}

/// One on-CPU or off-CPU interval, as reported by SCHED-record
/// transitions. For off-CPU, the stack the thread was on at the
/// moment it parked is included so the aggregator can attribute the
/// blocked time to it.
pub struct RawInterval {
    pub start_ns: u64,
    /// 0 means "still open" (thread is currently in this state and
    /// hasn't transitioned yet). Aggregation treats open intervals as
    /// ending at `last_sample_ns` -- the most recent timestamp the
    /// recorder has fed us -- so they show up in live snapshots.
    pub end_ns: u64,
    pub kind: IntervalKind,
}

pub enum IntervalKind {
    OnCpu,
    OffCpu {
        /// Stack at the moment the thread blocked, leaf-first. Empty
        /// when no PET stack had been captured for the thread before
        /// it parked.
        stack: Box<[u64]>,
        /// Wakeup attribution captured via MACH_MAKERUNNABLE. None
        /// when the wakeup batch hadn't drained yet, or for intervals
        /// that ended at end-of-recording without a wake event.
        waker_tid: Option<u32>,
        waker_user_stack: Option<Box<[u64]>>,
    },
}

/// One observed wakeup edge for the wakers panel. Independent of
/// interval tracking. `timestamp_ns` is currently unread (the panel
/// aggregates by waker symbol, not by time), but kept on-record so a
/// future timeline view can plot wakeups directly.
#[allow(dead_code)]
pub struct RawWakeup {
    pub timestamp_ns: u64,
    pub waker_tid: u32,
    pub waker_user_stack: Box<[u64]>,
    pub waker_kernel_stack: Box<[u64]>,
}

/// Per-thread cap on each event queue. Bounded memory: 100k samples ×
/// (avg 30 frames × 8B + 24B header) ≈ 26 MB worst case per thread,
/// before slack. Same cap on intervals + wakeups.
const MAX_EVENTS_PER_THREAD: usize = 100_000;

#[derive(Default)]
pub struct ThreadStats {
    pub(crate) pet_samples: std::collections::VecDeque<PetSample>,
    pub(crate) intervals: std::collections::VecDeque<RawInterval>,
    pub(crate) wakeups: std::collections::VecDeque<RawWakeup>,
    /// Probe results indexed by `kperf_ts` so a UI / query can
    /// look up "for sample at time T on tid X, what did the
    /// suspended-thread walk show?". Bounded with the same per-
    /// thread cap as the other queues.
    pub(crate) probe_results: std::collections::VecDeque<ProbeResultRecord>,
}

#[derive(Clone, Copy)]
pub struct RawWakerEntry {
    pub waker_tid: u32,
    pub waker_leaf_address: u64,
    pub count: u64,
}

/// In-memory aggregated stack tree. Built fresh for each query.
#[derive(Default, Clone)]
pub struct StackNode {
    pub on_cpu_ns: u64,
    pub off_cpu: OffCpuBreakdown,
    pub pet_samples: u64,
    pub off_cpu_intervals: u64,
    pub pmc: PmcAccum,
    pub children: HashMap<u64, StackNode>,
}

impl StackNode {
    fn merge(&mut self, other: &StackNode) {
        self.on_cpu_ns = self.on_cpu_ns.saturating_add(other.on_cpu_ns);
        self.off_cpu.add_other(&other.off_cpu);
        self.pet_samples = self.pet_samples.saturating_add(other.pet_samples);
        self.off_cpu_intervals = self
            .off_cpu_intervals
            .saturating_add(other.off_cpu_intervals);
        self.pmc.add_other(&other.pmc);
        for (&addr, child) in &other.children {
            self.children.entry(addr).or_default().merge(child);
        }
    }
}

/// Per-address self/total counters extracted from an aggregated tree
/// so the top-N table can rank addresses without re-walking.
#[derive(Clone, Copy, Default)]
pub struct AddressStats {
    pub self_on_cpu_ns: u64,
    pub total_on_cpu_ns: u64,
    pub self_off_cpu: OffCpuBreakdown,
    pub total_off_cpu: OffCpuBreakdown,
    pub self_pet_samples: u64,
    pub total_pet_samples: u64,
    pub self_off_cpu_intervals: u64,
    pub total_off_cpu_intervals: u64,
    pub self_pmc: PmcAccum,
    pub total_pmc: PmcAccum,
}

/// Result of a full aggregation pass. Carries the stack tree, the
/// per-address rollups for top-N, plus the headline totals.
pub struct Aggregation {
    pub flame_root: StackNode,
    pub by_address: HashMap<u64, AddressStats>,
    pub total_on_cpu_ns: u64,
    pub total_off_cpu: OffCpuBreakdown,
}

impl Aggregation {
    pub fn total_off_cpu_ns(&self) -> u64 {
        self.total_off_cpu.total_ns()
    }
}

/// Process-wide aggregator: per-thread event log plus thread name
/// lookup. "All-threads" queries iterate per-thread on demand.
#[derive(Default)]
pub struct Aggregator {
    threads: HashMap<u32, ThreadStats>,
    thread_names: HashMap<u32, String>,
    /// First sample/interval timestamp we ever saw, in ns. Anchors
    /// the timeline.
    session_start_ns: Option<u64>,
    /// Most recent timestamp (sample OR interval transition) the
    /// recorder fed us. Open intervals are treated as ending here at
    /// query time.
    last_event_ns: Option<u64>,
}

impl Aggregator {
    /// Record a race-against-return probe result. Pairs with the
    /// kperf `PetSample` that has the same `(tid, kperf_ts)` —
    /// queries can correlate at read time.
    pub fn record_probe_result(&mut self, result: ProbeResultRecord) {
        self.note_timestamp(result.timing.kperf_ts);
        let stats = self.threads.entry(result.tid).or_default();
        if stats.probe_results.len() >= MAX_EVENTS_PER_THREAD {
            stats.probe_results.pop_front();
        }
        stats.probe_results.push_back(result);
    }

    pub fn record_pet_sample(
        &mut self,
        tid: u32,
        timestamp_ns: u64,
        user_addrs: &[u64],
        kernel_addrs: &[u64],
        pmc: PmuSample,
    ) {
        self.note_timestamp(timestamp_ns);
        let stats = self.threads.entry(tid).or_default();
        if stats.pet_samples.len() >= MAX_EVENTS_PER_THREAD {
            stats.pet_samples.pop_front();
        }
        stats.pet_samples.push_back(PetSample {
            timestamp_ns,
            stack: user_addrs.to_vec().into_boxed_slice(),
            kernel_stack: kernel_addrs.to_vec().into_boxed_slice(),
            pmc,
        });
    }

    pub fn record_interval(&mut self, tid: u32, start_ns: u64, end_ns: u64, kind: IntervalKind) {
        self.note_timestamp(start_ns);
        if end_ns != 0 {
            self.note_timestamp(end_ns);
        }
        let stats = self.threads.entry(tid).or_default();
        if stats.intervals.len() >= MAX_EVENTS_PER_THREAD {
            stats.intervals.pop_front();
        }
        stats.intervals.push_back(RawInterval {
            start_ns,
            end_ns,
            kind,
        });
    }

    /// Append one wakeup edge into the wakee's per-thread ledger.
    /// Bounded same way as samples/intervals.
    pub fn record_wakeup(
        &mut self,
        timestamp_ns: u64,
        waker_tid: u32,
        wakee_tid: u32,
        waker_user_stack: Vec<u64>,
        waker_kernel_stack: Vec<u64>,
    ) {
        self.note_timestamp(timestamp_ns);
        let stats = self.threads.entry(wakee_tid).or_default();
        if stats.wakeups.len() >= MAX_EVENTS_PER_THREAD {
            stats.wakeups.pop_front();
        }
        stats.wakeups.push_back(RawWakeup {
            timestamp_ns,
            waker_tid,
            waker_user_stack: waker_user_stack.into_boxed_slice(),
            waker_kernel_stack: waker_kernel_stack.into_boxed_slice(),
        });
    }

    fn note_timestamp(&mut self, ts: u64) {
        if ts == 0 {
            return;
        }
        if self.session_start_ns.is_none() {
            self.session_start_ns = Some(ts);
        }
        self.last_event_ns = Some(match self.last_event_ns {
            Some(prev) => prev.max(ts),
            None => ts,
        });
    }

    /// Aggregate wakers for a given wakee tid.
    pub fn top_wakers(&self, wakee_tid: u32, limit: usize) -> Vec<RawWakerEntry> {
        let Some(stats) = self.threads.get(&wakee_tid) else {
            return Vec::new();
        };
        let mut groups: HashMap<(u32, u64), RawWakerEntry> = HashMap::new();
        for w in &stats.wakeups {
            let leaf = w
                .waker_user_stack
                .first()
                .copied()
                .or_else(|| w.waker_kernel_stack.first().copied())
                .unwrap_or(0);
            let key = (w.waker_tid, leaf);
            groups
                .entry(key)
                .and_modify(|e| e.count += 1)
                .or_insert(RawWakerEntry {
                    waker_tid: w.waker_tid,
                    waker_leaf_address: leaf,
                    count: 1,
                });
        }
        let mut out: Vec<RawWakerEntry> = groups.into_values().collect();
        out.sort_by(|a, b| b.count.cmp(&a.count));
        out.truncate(limit);
        out
    }

    pub fn session_start_ns(&self) -> Option<u64> {
        self.session_start_ns
    }

    pub fn last_event_ns(&self) -> Option<u64> {
        self.last_event_ns
    }

    pub fn set_thread_name(&mut self, tid: u32, name: String) {
        self.thread_names.insert(tid, name);
    }

    pub fn thread_name(&self, tid: u32) -> Option<&str> {
        self.thread_names.get(&tid).map(|s| s.as_str())
    }

    pub fn iter_threads(&self) -> impl Iterator<Item = u32> + '_ {
        self.threads.keys().copied()
    }

    pub fn thread_stats(&self, tid: u32) -> Option<&ThreadStats> {
        self.threads.get(&tid)
    }

    /// Iterate raw PET samples for a single thread, or every thread
    /// when `tid` is `None`.
    pub fn iter_pet_samples<'a>(
        &'a self,
        tid: Option<u32>,
    ) -> Box<dyn Iterator<Item = (u32, &'a PetSample)> + 'a> {
        match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => Box::new(t.pet_samples.iter().map(move |s| (tid, s))),
                None => Box::new(std::iter::empty()),
            },
            None => Box::new(
                self.threads
                    .iter()
                    .flat_map(|(&tid, t)| t.pet_samples.iter().map(move |s| (tid, s))),
            ),
        }
    }

    /// Iterate raw intervals for a single thread, or every thread
    /// when `tid` is `None`.
    pub fn iter_intervals<'a>(
        &'a self,
        tid: Option<u32>,
    ) -> Box<dyn Iterator<Item = (u32, &'a RawInterval)> + 'a> {
        match tid {
            Some(tid) => match self.threads.get(&tid) {
                Some(t) => Box::new(t.intervals.iter().map(move |i| (tid, i))),
                None => Box::new(std::iter::empty()),
            },
            None => Box::new(
                self.threads
                    .iter()
                    .flat_map(|(&tid, t)| t.intervals.iter().map(move |i| (tid, i))),
            ),
        }
    }

    /// Run a full aggregation pass: walk every PET sample + interval
    /// for the requested thread (or all threads if `tid` is `None`),
    /// crediting on-CPU intervals to the PET samples that fell inside
    /// them and off-CPU intervals to the leaf-classified stack at
    /// time-of-blocking. Returns a fresh `Aggregation` -- nothing is
    /// cached between calls.
    ///
    /// `predicate` filters samples + intervals (by timestamp range,
    /// excluded symbols, etc.). Pass `|_, _| true` for no filtering.
    pub fn aggregate<P>(
        &self,
        tid: Option<u32>,
        binaries: &BinaryRegistry,
        mut predicate: P,
    ) -> Aggregation
    where
        P: FnMut(EventCtx<'_>) -> bool,
    {
        let now = self.last_event_ns.unwrap_or(0);
        let mut flame_root = StackNode::default();
        let mut by_address: HashMap<u64, AddressStats> = HashMap::new();
        let mut total_on_cpu_ns: u64 = 0;
        let mut total_off_cpu = OffCpuBreakdown::default();

        // Walk per-thread to keep the per-thread sample/interval
        // streams independent of each other (an on-CPU interval for
        // tid A only consumes PET samples for tid A).
        let tids: Vec<u32> = match tid {
            Some(tid) => {
                if self.threads.contains_key(&tid) {
                    vec![tid]
                } else {
                    Vec::new()
                }
            }
            None => self.threads.keys().copied().collect(),
        };

        for tid in tids {
            let stats = match self.threads.get(&tid) {
                Some(s) => s,
                None => continue,
            };
            let enriched_stacks = build_enriched_stack_map(stats);

            for interval in &stats.intervals {
                let start = interval.start_ns;
                let end = if interval.end_ns == 0 {
                    // Open interval: treat as ending at the most
                    // recent event we know about so live snapshots
                    // include the in-progress slice.
                    now.max(start)
                } else {
                    interval.end_ns
                };
                let duration = end.saturating_sub(start);
                if duration == 0 {
                    continue;
                }

                match &interval.kind {
                    IntervalKind::OnCpu => {
                        // Find PET samples for this thread whose
                        // timestamp falls inside [start, end). pet_samples
                        // are appended in order so this is monotone.
                        let samples: Vec<&PetSample> = stats
                            .pet_samples
                            .iter()
                            .filter(|s| s.timestamp_ns >= start && s.timestamp_ns < end)
                            .filter(|s| {
                                predicate(EventCtx::PetSample {
                                    tid,
                                    sample: s,
                                    binaries,
                                })
                            })
                            .collect();
                        if samples.is_empty() {
                            continue;
                        }
                        let credit_ns = duration / samples.len() as u64;
                        if credit_ns == 0 {
                            continue;
                        }
                        total_on_cpu_ns = total_on_cpu_ns.saturating_add(duration);
                        for s in samples {
                            let stack = enriched_stacks
                                .get(&s.timestamp_ns)
                                .map(Vec::as_slice)
                                .unwrap_or(&s.stack);
                            credit_on_cpu_to_tree(
                                &mut flame_root,
                                &mut by_address,
                                stack,
                                credit_ns,
                                &s.pmc,
                            );
                        }
                    }
                    IntervalKind::OffCpu {
                        stack,
                        waker_tid: _,
                        waker_user_stack: _,
                    } => {
                        if !predicate(EventCtx::Interval {
                            tid,
                            interval,
                            binaries,
                        }) {
                            continue;
                        }
                        let leaf_name = stack
                            .first()
                            .and_then(|&addr| binaries.lookup_symbol(addr))
                            .map(|r| r.function_name);
                        let reason = classify_offcpu(leaf_name.as_deref());
                        total_off_cpu.add_reason(reason, duration);
                        credit_off_cpu_to_tree(
                            &mut flame_root,
                            &mut by_address,
                            stack,
                            reason,
                            duration,
                        );
                    }
                }
            }
        }

        Aggregation {
            flame_root,
            by_address,
            total_on_cpu_ns,
            total_off_cpu,
        }
    }

    /// Convenience wrapper for "all data, all threads, no filter."
    pub fn aggregate_all(&self, binaries: &BinaryRegistry) -> Aggregation {
        self.aggregate(None, binaries, |_| true)
    }
}

/// Context passed to predicates so they can decide per-event whether
/// to include or drop an event during aggregation.
pub enum EventCtx<'a> {
    PetSample {
        tid: u32,
        sample: &'a PetSample,
        binaries: &'a BinaryRegistry,
    },
    Interval {
        tid: u32,
        interval: &'a RawInterval,
        binaries: &'a BinaryRegistry,
    },
}

fn build_enriched_stack_map(stats: &ThreadStats) -> HashMap<u64, Vec<u64>> {
    if stats.pet_samples.is_empty() || stats.probe_results.is_empty() {
        return HashMap::new();
    }

    let mut out = HashMap::new();
    let mut pets: Vec<_> = stats.pet_samples.iter().collect();
    pets.sort_by_key(|pet| pet.timestamp_ns);
    let mut probes: Vec<_> = stats.probe_results.iter().collect();
    probes.sort_by_key(|probe| probe.timing.kperf_ts);

    let mut pet_idx = 0usize;
    for probe in probes {
        while let Some(pet) = pets.get(pet_idx) {
            if pet.timestamp_ns < probe.timing.kperf_ts
                && abs_tick_delta_ns(pet.timestamp_ns, probe.timing.kperf_ts) > PROBE_PAIR_WINDOW_NS
            {
                pet_idx += 1;
            } else {
                break;
            }
        }

        let mut best_idx = None;
        let mut best_delta_ns = u64::MAX;
        let mut scan_idx = pet_idx;
        while let Some(pet) = pets.get(scan_idx) {
            let delta_ns = abs_tick_delta_ns(pet.timestamp_ns, probe.timing.kperf_ts);
            if pet.timestamp_ns > probe.timing.kperf_ts && delta_ns > PROBE_PAIR_WINDOW_NS {
                break;
            }
            if delta_ns <= PROBE_PAIR_WINDOW_NS && delta_ns < best_delta_ns {
                best_idx = Some(scan_idx);
                best_delta_ns = delta_ns;
            }
            scan_idx += 1;
        }

        let Some(best_idx) = best_idx else {
            continue;
        };
        let pet = pets[best_idx];
        pet_idx = best_idx + 1;

        if let Some(stack) = validated_enriched_user_stack(pet, probe) {
            out.insert(pet.timestamp_ns, stack);
        }
    }

    out
}

fn validated_enriched_user_stack(pet: &PetSample, probe: &ProbeResultRecord) -> Option<Vec<u64>> {
    if pet.stack.is_empty() || probe.dwarf_walked.is_empty() {
        return None;
    }

    let dwarf_stack = logical_probe_stack(probe.mach_pc, 0, &probe.dwarf_walked);
    if dwarf_stack.len() < pet.stack.len() {
        return None;
    }

    let kperf_walk = &pet.stack[1..];
    let dwarf_walk = &dwarf_stack[1..];
    if (longest_common_run(kperf_walk, dwarf_walk) as u32) < STITCH_MIN_SUFFIX {
        return None;
    }

    Some(dwarf_stack)
}

/// Walk a leaf-first stack and credit `credit_ns` of on-CPU time to
/// every node along it. The first frame is the leaf; that's the one
/// that gets `self_*` credit in `by_address`.
fn credit_on_cpu_to_tree(
    flame_root: &mut StackNode,
    by_address: &mut HashMap<u64, AddressStats>,
    stack: &[u64],
    credit_ns: u64,
    pmc: &PmuSample,
) {
    if stack.is_empty() {
        return;
    }
    let leaf = stack[0];
    {
        let s = by_address.entry(leaf).or_default();
        s.self_on_cpu_ns = s.self_on_cpu_ns.saturating_add(credit_ns);
        s.self_pet_samples = s.self_pet_samples.saturating_add(1);
        s.self_pmc.add(pmc);
    }
    let mut seen: smallset::SmallSet = Default::default();
    for &addr in stack {
        if seen.insert(addr) {
            let s = by_address.entry(addr).or_default();
            s.total_on_cpu_ns = s.total_on_cpu_ns.saturating_add(credit_ns);
            s.total_pet_samples = s.total_pet_samples.saturating_add(1);
            s.total_pmc.add(pmc);
        }
    }
    // Walk reversed so the synthetic root's children are caller-most
    // frames; descendants of that node are callees.
    let mut node = flame_root;
    for &addr in stack.iter().rev() {
        node = node.children.entry(addr).or_default();
        node.on_cpu_ns = node.on_cpu_ns.saturating_add(credit_ns);
        node.pet_samples = node.pet_samples.saturating_add(1);
        node.pmc.add(pmc);
    }
}

fn credit_off_cpu_to_tree(
    flame_root: &mut StackNode,
    by_address: &mut HashMap<u64, AddressStats>,
    stack: &[u64],
    reason: OffCpuReason,
    duration_ns: u64,
) {
    if stack.is_empty() {
        return;
    }
    let leaf = stack[0];
    {
        let s = by_address.entry(leaf).or_default();
        s.self_off_cpu.add_reason(reason, duration_ns);
        s.self_off_cpu_intervals = s.self_off_cpu_intervals.saturating_add(1);
    }
    let mut seen: smallset::SmallSet = Default::default();
    for &addr in stack {
        if seen.insert(addr) {
            let s = by_address.entry(addr).or_default();
            s.total_off_cpu.add_reason(reason, duration_ns);
            s.total_off_cpu_intervals = s.total_off_cpu_intervals.saturating_add(1);
        }
    }
    let mut node = flame_root;
    for &addr in stack.iter().rev() {
        node = node.children.entry(addr).or_default();
        node.off_cpu.add_reason(reason, duration_ns);
        node.off_cpu_intervals = node.off_cpu_intervals.saturating_add(1);
    }
}

#[allow(dead_code)]
pub(crate) fn merged_flame_root<'a>(
    aggs: impl IntoIterator<Item = &'a Aggregation>,
) -> Cow<'a, StackNode> {
    let mut iter = aggs.into_iter();
    let first = match iter.next() {
        Some(a) => a,
        None => return Cow::Owned(StackNode::default()),
    };
    let mut out = first.flame_root.clone();
    for a in iter {
        out.merge(&a.flame_root);
    }
    Cow::Owned(out)
}

mod smallset {
    /// Inline set for small N (typical stack depths are <= 30).
    /// Beats HashSet for tiny N -- linear scan + cache locality.
    #[derive(Default)]
    pub struct SmallSet {
        items: Vec<u64>,
    }

    impl SmallSet {
        pub fn insert(&mut self, value: u64) -> bool {
            if self.items.contains(&value) {
                false
            } else {
                self.items.push(value);
                true
            }
        }
    }
}
