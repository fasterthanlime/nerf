//! Schema for the stax live RPC service.
//!
//! This crate is intentionally tiny: it holds only the `#[vox::service]`
//! trait + the wire types. Both `stax-live` (the runtime that implements
//! and serves the trait) and `xtask` (which generates TypeScript bindings
//! from the trait) depend on this crate. Keeping the schema in its own
//! crate lets `xtask` skip the heavy runtime deps (tokio, transports, etc.)
//! that `stax-live` pulls in.

use facet::Facet;

/// Off-CPU time at a stack node, broken down by why the thread was
/// off-CPU. Sum across all fields = total off-CPU time.
///
/// The breakdown is the wire's main lever for "what is this thread
/// actually doing?": idle parking is uninteresting, lock contention
/// is usually the thing to chase, IO and IPC tell different stories.
/// The UI renders flame boxes color-segmented by these fields.
#[derive(Clone, Copy, Debug, Default, Facet)]
pub struct OffCpuBreakdown {
    /// Voluntarily parked waiting for new work
    /// (cond-vars, ulock, workq idle).
    pub idle_ns: u64,
    /// Blocked on a mutex / rwlock owned by another thread.
    pub lock_ns: u64,
    /// Blocked on a semaphore.
    pub semaphore_ns: u64,
    /// Blocked in mach_msg waiting for a reply.
    pub ipc_ns: u64,
    /// Blocking read syscall.
    pub io_read_ns: u64,
    /// Blocking write syscall.
    pub io_write_ns: u64,
    /// fd-readiness wait (poll/select/kevent).
    pub readiness_ns: u64,
    /// Explicit sleep.
    pub sleep_ns: u64,
    /// Connection-setup blocking (connect/accept/open).
    pub connect_ns: u64,
    /// Couldn't classify the leaf frame, or no PET stack was
    /// available to consult.
    pub other_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopEntry {
    pub address: u64,
    /// Demangled symbol name when the live binary registry has the
    /// containing image loaded. `None` for JIT'd code, kernel frames,
    /// or images that haven't been observed yet.
    pub function_name: Option<String>,
    /// Basename of the image (e.g. "libsystem_malloc.dylib"). Same
    /// availability semantics as `function_name`.
    pub binary: Option<String>,
    /// True when the containing binary is the main executable rather
    /// than a system / runtime dylib. The frontend uses this to colour
    /// target-code rows distinctly.
    pub is_main: bool,
    /// Source language inferred from demangling — `"rust"`, `"cpp"`,
    /// `"swift"`, etc.
    pub language: String,

    /// On-CPU time attributed to this symbol as a leaf frame, ns.
    pub self_on_cpu_ns: u64,
    /// On-CPU time attributed to this symbol as any frame on the
    /// stack, ns.
    pub total_on_cpu_ns: u64,
    /// Off-CPU breakdown attributed as a leaf.
    pub self_off_cpu: OffCpuBreakdown,
    /// Off-CPU breakdown attributed as any frame on the stack.
    pub total_off_cpu: OffCpuBreakdown,
    /// PET stack-walk hits where this symbol was the leaf.
    pub self_pet_samples: u64,
    /// PET stack-walk hits where this symbol appeared anywhere.
    pub total_pet_samples: u64,
    /// Off-CPU intervals attributed to this symbol as a leaf.
    pub self_off_cpu_intervals: u64,
    /// Off-CPU intervals attributed to this symbol anywhere.
    pub total_off_cpu_intervals: u64,

    /// CPU cycles attributed to this symbol's leaf samples, summed
    /// from Apple Silicon's fixed PMU counter 0. 0 on Linux / when
    /// PMC sampling is unavailable. Off-CPU contributes nothing here.
    pub self_cycles: u64,
    pub self_instructions: u64,
    pub self_l1d_misses: u64,
    pub self_branch_mispreds: u64,
    pub total_cycles: u64,
    pub total_instructions: u64,
    pub total_l1d_misses: u64,
    pub total_branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopUpdate {
    /// Total on-CPU time across every entry in this snapshot, ns.
    /// Bounded above by `cores × wall_time`.
    pub total_on_cpu_ns: u64,
    /// Total off-CPU time across every entry, ns. Per-reason
    /// breakdown across the whole snapshot.
    pub total_off_cpu: OffCpuBreakdown,
    pub entries: Vec<TopEntry>,
}

/// Sort key for the top-N list. Truncation happens after sorting, so
/// `ByTotal` will surface rows that are pure inner frames (high total,
/// zero self) which `BySelf` would push past the limit.
#[derive(Clone, Copy, Debug, Facet)]
#[repr(u8)]
pub enum TopSort {
    BySelf = 0,
    ByTotal = 1,
}

/// One node in the call-tree flamegraph. Address 0 is reserved for the
/// synthetic root that aggregates all stacks.
///
/// Each node carries on-CPU time and off-CPU time *separately*, with
/// the off-CPU portion broken down by reason. Children sum to (or are
/// less than, after pruning) the parent's totals, per-field. The UI
/// picks which field drives flame-box width and can color-segment a
/// box across the off-CPU breakdown.
///
/// `function_name`, `binary`, and `language` are indices into the
/// containing `FlamegraphUpdate.strings` / `NeighborsUpdate.strings`
/// table — interning saves ~50 bytes per node on the wire when most
/// nodes resolve to the same handful of (function, binary) pairs.
#[derive(Clone, Debug, Facet)]
pub struct FlameNode {
    pub address: u64,
    pub function_name: Option<u32>,
    pub binary: Option<u32>,
    pub is_main: bool,
    pub language: u32,

    /// Real CPU time at (or under) this stack, in nanoseconds.
    /// Computed from SCHED on-CPU intervals: each interval's duration
    /// is distributed evenly across the PET stack samples that fell
    /// inside it, then credited to every node on those stacks.
    pub on_cpu_ns: u64,
    /// Off-CPU time at this stack, by reason. Computed from SCHED
    /// off-CPU intervals using the leaf frame at the moment the
    /// thread blocked.
    pub off_cpu: OffCpuBreakdown,
    /// Number of PET stack-walk hits at (or under) this node. Lets
    /// the UI tell apart "10ms × 1 sample" (low confidence) from
    /// "10ms × 10 samples" (high confidence) for the same on-cpu
    /// number.
    pub pet_samples: u64,
    /// Number of off-CPU intervals attributed to this stack. Hot
    /// blocking-site indicator independent of total time.
    pub off_cpu_intervals: u64,

    /// PMU counter sums across PET samples that traversed this node.
    /// Off-CPU contributes nothing (no PMC during sleep). Lets the
    /// flamegraph colour-by-event mode fall straight out of the tree.
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,

    pub children: Vec<FlameNode>,
}

#[derive(Clone, Debug, Facet)]
pub struct FlamegraphUpdate {
    /// Total on-CPU time covered by this snapshot's intervals, ns.
    /// Equals `root.on_cpu_ns`.
    pub total_on_cpu_ns: u64,
    /// Total off-CPU time, by reason. Equals `root.off_cpu`.
    pub total_off_cpu: OffCpuBreakdown,
    /// Deduplicated string table: `FlameNode.function_name`,
    /// `binary`, and `language` are indices into this. A typical
    /// session has on the order of ~50 unique (function, binary)
    /// pairs that would otherwise repeat across thousands of nodes.
    pub strings: Vec<String>,
    pub root: FlameNode,
}

/// One row in a "who woke this thread?" panel. Aggregated server-side
/// across the wakee's wakeup ledger, grouped by (waker_tid,
/// waker_function). The leaf frame is what gets named so a user sees
/// e.g. "tid 5103 / dispatch_async_f · 24 wakeups" -- the function
/// where the wake-up call was issued.
#[derive(Clone, Debug, Facet)]
pub struct WakerEntry {
    pub waker_tid: u32,
    pub waker_address: u64,
    pub waker_function_name: Option<String>,
    pub waker_binary: Option<String>,
    pub language: String,
    pub count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct WakersUpdate {
    pub wakee_tid: u32,
    pub total_wakeups: u64,
    pub entries: Vec<WakerEntry>,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadInfo {
    pub tid: u32,
    pub name: Option<String>,
    /// On-CPU time for this thread, ns. Bounded by wall_time on a
    /// single core (≤ wall_time × cores in practice -- a thread can
    /// only be on one CPU at a time, so per-thread on_cpu_ns ≤
    /// wall_time).
    pub on_cpu_ns: u64,
    /// Off-CPU breakdown for this thread.
    pub off_cpu: OffCpuBreakdown,
    /// Total PET stack-walk hits we caught for this thread.
    pub pet_samples: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadsUpdate {
    pub threads: Vec<ThreadInfo>,
}

/// One time bucket on the timeline. On-CPU and off-CPU show up as
/// separately-stacked layers so the UI can distinguish "the system
/// was busy here" from "lots of threads were parked here."
#[derive(Clone, Debug, Facet)]
pub struct TimelineBucket {
    /// Bucket start, in nanoseconds since the recording started (i.e.
    /// since the first sample).
    pub start_ns: u64,
    /// On-CPU time attributed to this bucket from SCHED on-CPU
    /// intervals that overlapped it.
    pub on_cpu_ns: u64,
    /// Off-CPU time, summed across all reasons.
    pub off_cpu_ns: u64,
}

/// A pair of (start, end) timestamps in ns, both relative to the
/// recording start (the timestamp of the first sample). End-exclusive.
#[derive(Clone, Debug, Facet)]
pub struct TimeRange {
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct SymbolRef {
    pub function_name: Option<String>,
    pub binary: Option<String>,
}

/// Why a thread was off-CPU. Classified at the moment the thread
/// blocked from the leaf user-space frame on its stack at that
/// instant. The 10 categories cover the macOS / pthread / BSD
/// surface area; anything that doesn't match a known leaf goes to
/// `Other`.
///
/// Order matters: variants are repr(u8) and serialised by index.
/// Append new variants at the end -- inserting in the middle would
/// renumber everything past the insert and break older clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum OffCpuReason {
    /// Voluntarily idle: thread parked waiting for new work.
    /// `__psynch_cvwait`, `__ulock_wait`, `__workq_kernreturn`.
    /// The thread isn't blocked ON anything -- it's waiting to be
    /// told there's work. Cheap and usually uninteresting unless
    /// it's the *target* code's path through it.
    Idle = 0,
    /// Lock contention: thread wants to run but is blocked on a
    /// mutex / rwlock / spinlock owned by someone else. This is
    /// usually the off-CPU you actually want to fix.
    /// `__psynch_mutexwait`, `__psynch_rw_*`.
    LockWait = 1,
    /// Semaphore wait (explicit count-based sync).
    /// `__semwait_signal`, `semaphore_wait_trap`.
    SemaphoreWait = 2,
    /// Mach IPC blocked in mach_msg waiting for a reply.
    /// `mach_msg2_trap`, `mach_msg_overwrite_trap`.
    IpcWait = 3,
    /// Read-side IO syscall: `read`, `recv`, `recvfrom`, `recvmsg`,
    /// `pread`. (Includes blocking-mode socket reads.)
    IoRead = 4,
    /// Write-side IO syscall: `write`, `send`, `sendmsg`, `pwrite`.
    IoWrite = 5,
    /// fd-readiness wait: `select`, `pselect`, `poll`, `ppoll`,
    /// `kevent`, `kevent_id`, `kevent_qos`.
    Readiness = 6,
    /// Explicit sleep: `nanosleep`, `usleep`.
    Sleep = 7,
    /// Connection-setup blocking: `connect`, `accept`, `__open_nocancel`,
    /// dyld lazy-bind faults, etc.
    ConnectionSetup = 8,
    /// Couldn't classify the leaf frame, or no PET stack was
    /// available before the thread went off-CPU.
    Other = 9,
}

/// Filter applied at query time over the raw event log. When all
/// fields are at their defaults, the server hits the fast pre-aggregated
/// path; any non-default field forces re-aggregation.
///
/// Note: there's no on-CPU / off-CPU mode flag here. Every flame node
/// carries on/off-CPU and per-reason durations as separate fields, so
/// "what to render as box width" is purely a frontend concern -- the
/// server always serves the full breakdown.
#[derive(Clone, Debug, Facet)]
pub struct LiveFilter {
    pub time_range: Option<TimeRange>,
    /// Drop any sample / interval whose stack contains *any* of these
    /// symbols.
    pub exclude_symbols: Vec<SymbolRef>,
}

/// Bundle of "what to look at" knobs shared by every view
/// subscription. Bundled into one struct because vox/facet's tuple
/// bound caps method arities at 4.
#[derive(Clone, Debug, Facet)]
pub struct ViewParams {
    /// Filter to one thread's samples; `None` aggregates across all.
    pub tid: Option<u32>,
    pub filter: LiveFilter,
}

#[derive(Clone, Debug, Facet)]
pub struct TimelineUpdate {
    /// Width of each bucket in nanoseconds.
    pub bucket_size_ns: u64,
    /// Recording duration so the UI can show "Xs elapsed" without
    /// computing it client-side.
    pub recording_duration_ns: u64,
    /// Total on-CPU time across the timeline.
    pub total_on_cpu_ns: u64,
    /// Total off-CPU time across the timeline (all reasons summed).
    pub total_off_cpu_ns: u64,
    /// Buckets in chronological order, dense (zero buckets in the
    /// middle are emitted so the UI can map x-position → time
    /// directly).
    pub buckets: Vec<TimelineBucket>,
}

/// kcachegrind-style "family tree" of a symbol's neighbors.
///
/// `callers_tree` is rooted at the target. Its children are direct
/// callers (one level up the stack); their children are the callers'
/// callers; and so on. So the deeper you go, the further from the
/// target — i.e. the tree grows *outward toward main*.
///
/// `callees_tree` is also rooted at the target. Its children are
/// direct callees; its grandchildren are their callees. So the deeper
/// you go, the further into the call stack — i.e. the tree grows
/// *outward toward leaf frames*.
///
/// Both trees are keyed by symbol (multiple addresses inside the same
/// function merge), so recursion / multiple call sites all roll up.
/// Counts are pruned at ~0.5% of `own_count` to bound the wire size.
#[derive(Clone, Debug, Facet)]
pub struct NeighborsUpdate {
    /// Shared string table for all FlameNode references in this
    /// update plus the target's own symbol fields.
    pub strings: Vec<String>,
    /// Resolved name of the target symbol; index into `strings`.
    /// `None` for unresolved addresses (JIT, kernel frames, etc.).
    pub function_name: Option<u32>,
    pub binary: Option<u32>,
    pub is_main: bool,
    pub language: u32,
    /// On-CPU time attributed to this symbol (sum across every
    /// address resolving to it).
    pub own_on_cpu_ns: u64,
    /// Off-CPU breakdown for this symbol.
    pub own_off_cpu: OffCpuBreakdown,
    /// PET stack-walk hits at this symbol.
    pub own_pet_samples: u64,
    /// Off-CPU intervals attributed to this symbol.
    pub own_off_cpu_intervals: u64,
    pub callers_tree: FlameNode,
    pub callees_tree: FlameNode,
}

/// Source-line header attached to the first instruction generated from
/// a given (file, line) pair. The frontend renders one of these as a
/// banner row above the asm row whenever the source location changes
/// between consecutive instructions.
#[derive(Clone, Debug, Facet)]
pub struct SourceHeader {
    pub file: String,
    pub line: u32,
    /// Highlighted source-line snippet (arborium custom-tag HTML); empty
    /// when the file couldn't be loaded (build-machine-relative paths,
    /// missing source on this box, etc.).
    pub html: String,
}

/// One disassembled instruction with its sampled hit data.
#[derive(Clone, Debug, Facet)]
pub struct AnnotatedLine {
    pub address: u64,
    /// HTML-highlighted assembly text. Uses arborium's default
    /// `CustomElements` format (`<a-k>mov</a-k>` etc.); the frontend
    /// styles those tags via the generated theme.css. Render with
    /// `dangerouslySetInnerHTML`.
    pub html: String,
    /// On-CPU time attributed to this instruction as a leaf, ns.
    /// Heatmap source.
    pub self_on_cpu_ns: u64,
    /// PET stack-walk hits at this instruction. With on_cpu_ns this
    /// gives both "how much time" and "how confident."
    pub self_pet_samples: u64,
    /// Set on the first instruction emitted for a new source location.
    /// `None` for instructions that share their source line with the
    /// previous instruction, and for binaries without DWARF.
    pub source_header: Option<SourceHeader>,
}

/// One off-CPU interval surfaced by `subscribe_intervals`.
/// Recording-relative timestamps (ns since the first sample).
#[derive(Clone, Debug, Facet)]
pub struct IntervalEntry {
    pub tid: u32,
    pub start_ns: u64,
    pub duration_ns: u64,
    pub reason: OffCpuReason,
    /// Who woke this thread out of the off-CPU interval, if
    /// MACH_MAKERUNNABLE caught it. None for intervals that closed
    /// without a captured wakeup edge (open at end-of-recording, or
    /// the wakeup batch hadn't drained when the interval ended).
    pub waker_tid: Option<u32>,
    pub waker_address: Option<u64>,
    pub waker_function_name: Option<u32>,
    pub waker_binary: Option<u32>,
}

#[derive(Clone, Debug, Facet)]
pub struct IntervalListUpdate {
    /// Shared string table for waker function/binary references.
    pub strings: Vec<String>,
    /// Total intervals matching the query (entries may be capped by
    /// the server before sending; this is the pre-cap count).
    pub total_intervals: u64,
    /// Sum of `duration_ns` across all matching intervals.
    pub total_duration_ns: u64,
    /// Per-reason breakdown of the total.
    pub by_reason: OffCpuBreakdown,
    pub entries: Vec<IntervalEntry>,
}

/// One PET stack-walk hit surfaced by `subscribe_pet_samples`.
#[derive(Clone, Debug, Facet)]
pub struct PetSampleEntry {
    pub tid: u32,
    /// Recording-relative ns.
    pub timestamp_ns: u64,
    /// Cycles delta since the previous PET tick on this thread (0
    /// when PMU sampling isn't available).
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct PetSampleListUpdate {
    pub total_samples: u64,
    pub entries: Vec<PetSampleEntry>,
}

/// One symbolicated address — what the server's BinaryRegistry
/// resolved an address to. Used by the probe-vs-kperf diff view
/// to render frame chains side-by-side.
#[derive(Clone, Debug, Facet)]
pub struct ResolvedFrame {
    pub address: u64,
    /// Human-readable rendering: `module!symbol+offset`,
    /// `module+0xoffset` if the address is in a known module but
    /// no enclosing symbol was found, or `<unmapped:0xaddr>` if
    /// no module covers it.
    pub display: String,
    /// Module basename (or empty if unmapped).
    pub binary: String,
    /// Demangled function name (or empty if no enclosing symbol).
    pub function: String,
}

/// One row of the kperf-vs-probe diff: a kperf PET sample paired
/// with the matching race-against-return probe result by
/// `(tid, kperf_ts)`.
#[derive(Clone, Debug, Facet)]
pub struct ProbeDiffEntry {
    pub tid: u32,
    /// Recording-relative ns of the kperf sample.
    pub timestamp_ns: u64,
    /// Drift between kperf sample timestamp and probe completion,
    /// in real nanoseconds (converted via mach_timebase_info on
    /// the server side).
    pub drift_ns: i64,
    /// kperf's user backtrace, leaf-most first.
    pub kperf_stack: Vec<ResolvedFrame>,
    /// kperf's kernel backtrace at PMI, leaf-most first. Empty
    /// when kperf interrupted user code (no kstack to walk) or
    /// when the kernel walk failed.
    pub kperf_kernel_stack: Vec<ResolvedFrame>,
    /// Suspended-thread leaf PC + walked return addresses,
    /// leaf-most first (probe_stack[0] = mach_pc).
    pub probe_stack: Vec<ResolvedFrame>,
    /// Synthesised stack the runtime would ship if we adopted the
    /// race-against-return technique for this sample. Currently
    /// = `probe_stack` when `stitchable`, otherwise empty.
    /// Populated server-side from the same address resolver as
    /// the other stacks so the UI can render it directly.
    pub stitched_stack: Vec<ResolvedFrame>,
    /// How many trailing frames of kperf and probe matched after
    /// PAC-strip, comparing only the FP-walked portions.
    pub common_suffix: u32,
    /// `true` if the PAC-stripped leaf PC matched between the
    /// two views.
    pub pc_match: bool,
    /// `true` if `pc_match && common_suffix >= STITCH_MIN_SUFFIX`.
    /// When set, `stitched_stack` is populated.
    pub stitchable: bool,
    /// `true` if the probe walked via framehop, `false` for the
    /// FP-walk fallback.
    pub used_framehop: bool,
}

#[derive(Clone, Debug, Facet)]
pub struct ProbeDiffBucket {
    /// Inclusive upper bound of the bucket in nanoseconds.
    /// Last bucket has `u64::MAX` for "everything above".
    pub upper_ns: u64,
    pub samples: u64,
    pub pc_match: u64,
}

/// Match rate at one frame depth counted from the leaf. Index 0
/// is the leaf PC, 1 is the first walked return address, etc.
#[derive(Clone, Debug, Facet)]
pub struct ProbeDiffDepthCell {
    pub depth: u32,
    pub matched: u64,
    /// Number of paired samples that had a frame at this depth
    /// in *both* stacks (i.e., both kperf and probe walked at
    /// least `depth + 1` frames).
    pub total: u64,
}

/// Per-thread breakdown for the probe diff.
#[derive(Clone, Debug, Facet)]
pub struct ProbeDiffThread {
    pub tid: u32,
    pub paired: u64,
    pub pc_match: u64,
    pub stitchable: u64,
    pub avg_common_suffix: f32,
    pub thread_name: Option<String>,
}

#[derive(Clone, Debug, Facet)]
pub struct ProbeDiffUpdate {
    pub total_kperf_samples: u64,
    pub total_probes: u64,
    /// (tid, kperf_ts) pairs where both a kperf sample and a probe
    /// result exist.
    pub paired: u64,
    /// kperf samples observed without a matching probe result. A
    /// run of `kperf_only > 0` while `total_probes == 0` means the
    /// correlated shade probe is disabled/unimplemented; otherwise
    /// it's a pairing race or a probe-side drop.
    pub kperf_only: u64,
    /// Probe results observed without a matching kperf sample —
    /// indicates the probe fired but the matching kperf record
    /// was lost (rare; usually a parser truncation).
    pub probe_only: u64,
    /// Paired samples where kperf walked 0 user frames (parked
    /// thread, FP=0 at PMI) but probe successfully walked ≥1.
    /// Pure value-add over kperf alone.
    pub probe_augmented_kperf: u64,
    /// Paired samples where probe walked strictly deeper than
    /// kperf (probe.len > kperf.walked.len + 1, +1 for the leaf).
    pub probe_walked_deeper: u64,
    /// Distribution of common-suffix lengths. Index = exact
    /// suffix length (0..=32).
    pub common_suffix_hist: Vec<u64>,
    /// Match rate at each frame depth counted from the leaf.
    /// Index 0 = leaf PC. Bounded to 32 entries.
    pub depth_match: Vec<ProbeDiffDepthCell>,
    /// Drift histogram in real nanoseconds.
    pub drift_buckets: Vec<ProbeDiffBucket>,
    pub pc_match: u64,
    /// Paired samples where `pc_match && common_suffix >=
    /// STITCH_MIN_SUFFIX`. The "deliverable" count: how many
    /// samples a future race-against-return shipping mode would
    /// produce a high-confidence stitched stack for.
    pub stitchable: u64,
    pub framehop_used: u64,
    pub fp_walk_used: u64,
    pub threads: Vec<ProbeDiffThread>,
    /// The N most recent paired entries for drill-down. Ordered
    /// oldest → newest.
    pub recent: Vec<ProbeDiffEntry>,
}

/// Minimum common-suffix length for a paired sample to be
/// considered stitchable. Tuned to avoid over-counting trivial
/// matches (the bottom-most pthread/dispatch root is shared by
/// almost everything; 3 frames means we agree on at least the
/// dispatch worker + its caller + a real work frame).
pub const STITCH_MIN_SUFFIX: u32 = 3;

#[derive(Clone, Debug, Facet)]
pub struct AnnotatedView {
    /// Best-effort symbol name (or hex string fallback).
    pub function_name: String,
    pub language: String,
    /// Address the disassembly starts at. Used by the client to mark which
    /// line corresponds to the original query address.
    pub base_address: u64,
    pub queried_address: u64,
    pub lines: Vec<AnnotatedLine>,
}

#[vox::service]
pub trait Profiler {
    /// Snapshot of the top-N functions, ranked by `sort`. `params`
    /// bundles thread/time/exclude filters.
    async fn top(&self, limit: u32, sort: TopSort, params: ViewParams) -> Vec<TopEntry>;

    async fn subscribe_top(
        &self,
        limit: u32,
        sort: TopSort,
        params: ViewParams,
        output: vox::Tx<TopUpdate>,
    );

    /// Total on-CPU time across every thread, in nanoseconds.
    /// Bounded by `cores × wall_time` (you can't be on more than one
    /// CPU at a time, and there are only so many CPUs). Useful for
    /// "X CPU-seconds across the recording" displays.
    async fn total_on_cpu_ns(&self) -> u64;

    async fn subscribe_annotated(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<AnnotatedView>,
    );

    async fn subscribe_flamegraph(&self, params: ViewParams, output: vox::Tx<FlamegraphUpdate>);

    async fn subscribe_threads(&self, output: vox::Tx<ThreadsUpdate>);

    /// Always relative to the full recording (no `filter`); brush
    /// selection happens on top of the unfiltered timeline.
    async fn subscribe_timeline(&self, tid: Option<u32>, output: vox::Tx<TimelineUpdate>);

    async fn subscribe_neighbors(
        &self,
        address: u64,
        params: ViewParams,
        output: vox::Tx<NeighborsUpdate>,
    );

    /// Stream "who woke this thread?" updates: top wakers grouped by
    /// (waker_tid, waker_function), aggregated from the kperf
    /// MACH_MAKERUNNABLE wakeup edges. The wakee's tid is required;
    /// `None` produces an empty update (we don't aggregate across
    /// threads).
    async fn subscribe_wakers(&self, wakee_tid: u32, output: vox::Tx<WakersUpdate>);

    /// Stream the off-CPU intervals attributed to a single stack
    /// node, in chronological order. Lets the UI drill into a flame
    /// box and see "this stack was blocked here for 12ms, here for
    /// 30ms..." with each interval colored by reason and clickable
    /// to surface the waker. `flame_key` matches the `r/2/1/0`
    /// addressing the frontend already uses for focus.
    async fn subscribe_intervals(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<IntervalListUpdate>,
    );

    /// Stream the PET stack-walk hits attributed to a single stack
    /// node, in chronological order. Symmetric counterpart to
    /// `subscribe_intervals` for the on-CPU side.
    async fn subscribe_pet_samples(
        &self,
        flame_key: String,
        params: ViewParams,
        output: vox::Tx<PetSampleListUpdate>,
    );

    /// Pause / resume live ingestion. While paused, new samples and
    /// wakeup edges from the recorder get dropped before reaching
    /// the aggregator -- frozen views, no client churn -- but the
    /// recorder keeps running underneath, the binary registry keeps
    /// updating, and disassembly / source / annotation queries
    /// continue to work against the existing (frozen) data.
    async fn set_paused(&self, paused: bool);
    async fn is_paused(&self) -> bool;

    /// Stream periodic snapshots of the kperf-vs-probe diff:
    /// per-thread pairing of kperf PET samples with their
    /// race-against-return probe results, common-suffix histogram,
    /// drift histogram, and the most recent N entries with both
    /// stacks symbolicated through the live BinaryRegistry. Pass
    /// `tid = Some(_)` to scope to a single thread, or `None` for
    /// all threads.
    async fn subscribe_probe_diff(&self, tid: Option<u32>, output: vox::Tx<ProbeDiffUpdate>);
}

/// Stable handle for one run hosted by the server. Returned by
/// `RunControl::start_run` and accepted by every other run-scoped
/// query. New format / domain in the future; today it's just a u64
/// monotonically issued by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Facet)]
pub struct RunId(pub u64);

/// Lifecycle phase of a hosted run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum RunState {
    /// Recording is in progress; samples are streaming in.
    Recording,
    /// The recorder reported it stopped (target exited, time limit hit,
    /// `stop_active` was called). Aggregator state is frozen but still
    /// queryable.
    Stopped,
}

/// Why a run stopped. Surfaced once the run transitions to
/// `RunState::Stopped`.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum StopReason {
    /// The launched child exited (or the attached PID went away).
    TargetExited,
    /// `--time-limit` elapsed.
    TimeLimit,
    /// User Ctrl-C'd the recorder, or an agent called `stop_active`.
    UserStop,
    /// The recorder errored. `message` carries the human-readable
    /// detail.
    RecorderError { message: String },
}

#[derive(Clone, Debug, Facet)]
pub struct RunSummary {
    pub id: RunId,
    pub state: RunState,
    /// `None` while still recording.
    pub stop_reason: Option<StopReason>,
    /// Wall-clock start (unix nanos).
    pub started_at_unix_ns: u64,
    /// Wall-clock stop (unix nanos). `None` while still recording.
    pub stopped_at_unix_ns: Option<u64>,
    /// PID of the target process, if any. `None` for runs that
    /// haven't acquired a PID yet (very early in the lifecycle).
    pub target_pid: Option<u32>,
    /// Best-effort label derived from the launch command or attached
    /// PID's executable basename. Free-form; not guaranteed unique.
    pub label: String,
    /// PET stack-walk hits ingested so far. Sourced from kperf
    /// (kdebug PERF_CS_UHDR/UDATA), one per kernel-side sampling
    /// tick.
    pub pet_samples: u64,
    /// Off-CPU intervals ingested so far.
    pub off_cpu_intervals: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct ServerStatus {
    /// Wall-clock time the server itself started, unix nanos.
    pub server_started_at_unix_ns: u64,
    /// Empty when no run is active. The server hosts one run at a
    /// time; agents should `wait_active` or `stop_active` before
    /// starting another. (Modelled as `Vec<RunSummary>` rather than
    /// `Option<RunSummary>` because Option-of-struct trips
    /// vox-postcard at the moment.)
    pub active: Vec<RunSummary>,
}

/// Agent-side wait condition: which event makes `wait_active` return.
/// First-fired wins; `wait_active` always also returns once the run
/// transitions to `Stopped`, regardless of which condition was set.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum WaitCondition {
    /// Block until the active run transitions to `Stopped`. The
    /// natural choice for "let the recording finish, then I'll
    /// query."
    UntilStopped,
    /// Return as soon as the run has ingested at least `count` PET
    /// samples (returns immediately if already past). Useful for
    /// "give me enough data to be statistically meaningful, then
    /// look."
    ForSamples { count: u64 },
    /// Return after `seconds` of wall-clock time inside `wait_active`,
    /// even if the run is still recording.
    ForSeconds { seconds: u64 },
    /// Return as soon as a symbol whose demangled name contains
    /// `needle` (case-sensitive substring match) has been observed
    /// in the binary registry. Useful for "wait until the JIT has
    /// produced the function I want to look at."
    UntilSymbolSeen { needle: String },
}

/// Outcome of a `wait_active` call.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum WaitOutcome {
    /// The wait condition fired. `summary` is the run's snapshot
    /// at the moment the condition fired (still `Recording` if the
    /// condition was, e.g., `ForSamples`).
    ConditionMet { summary: RunSummary },
    /// The run reached `Stopped`. Always returned for `UntilStopped`,
    /// and pre-empts any other condition for the other variants.
    Stopped { summary: RunSummary },
    /// The caller-supplied `timeout_ms` elapsed first. `summary` is
    /// the run's snapshot at that moment (still `Recording`).
    TimedOut { summary: RunSummary },
    /// No run was active when `wait_active` was called.
    NoActiveRun,
}

/// One symbol entry from a Mach-O `LC_SYMTAB`. Same shape as the
/// recorder's internal `MachOSymbol`, lifted onto the wire so we can
/// ferry the symbol table from recorder to server. Addresses are
/// SVMAs.
#[derive(Clone, Debug, Facet)]
pub struct WireMachOSymbol {
    pub start_svma: u64,
    pub end_svma: u64,
    pub name: Vec<u8>,
}

#[derive(Clone, Debug, Facet)]
pub struct WireBinaryLoaded {
    pub path: String,
    pub base_avma: u64,
    pub vmsize: u64,
    pub text_svma: u64,
    pub arch: Option<String>,
    pub is_executable: bool,
    pub symbols: Vec<WireMachOSymbol>,
    /// `__TEXT` bytes embedded inline (today: JIT'd code via the
    /// jitdump tailer). `None` for on-disk images.
    pub text_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Facet)]
pub struct WireBinaryUnloaded {
    pub path: String,
    pub base_avma: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct WireSampleEvent {
    pub timestamp_ns: u64,
    pub pid: u32,
    pub tid: u32,
    pub kernel_backtrace: Vec<u64>,
    pub user_backtrace: Vec<u64>,
    pub cycles: u64,
    pub instructions: u64,
    pub l1d_misses: u64,
    pub branch_mispreds: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct WireOnCpuInterval {
    pub tid: u32,
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct WireOffCpuInterval {
    pub tid: u32,
    pub start_ns: u64,
    pub end_ns: u64,
    pub stack: Vec<u64>,
    pub waker_tid: Option<u32>,
    pub waker_user_stack: Option<Vec<u64>>,
}

#[derive(Clone, Debug, Facet)]
pub struct WireWakeup {
    pub timestamp_ns: u64,
    pub waker_tid: u32,
    pub wakee_tid: u32,
    pub waker_user_stack: Vec<u64>,
    pub waker_kernel_stack: Vec<u64>,
}

/// One race-against-return probe result, correlated with a kperf
/// sample by `(tid, kperf_ts_mach)`. This is produced by the
/// attachment-side target helper, not by staxd. Server resolves
/// addresses through the same BinaryRegistry path it uses for
/// kperf samples.
#[derive(Clone, Debug, Facet)]
pub struct WireProbeResult {
    pub tid: u32,
    /// Kdebug timestamp of the matching kperf sample (mach ticks),
    /// converted to ns by the recorder before shipping.
    pub kperf_ts_ns: u64,
    /// Wall-clock-ns timestamp of when the probe completed.
    pub probe_done_ns: u64,
    /// User PC at suspend (PAC-stripped).
    pub mach_pc: u64,
    /// Link register at suspend (PAC-stripped).
    pub mach_lr: u64,
    /// Frame pointer at suspend.
    pub mach_fp: u64,
    /// Stack pointer at suspend.
    pub mach_sp: u64,
    /// Walked return addresses from the suspended thread, leaf-most
    /// first; PAC-stripped; does not include the leaf PC.
    pub mach_walked: Vec<u64>,
    /// `true` if framehop produced the walk, `false` for FP-walk
    /// fallback. Lets the server label "DWARF-accurate" vs
    /// "best-effort" frames in the UI.
    pub used_framehop: bool,
}

/// One ingest event the recorder ships to the server. Mirrors the
/// in-process `LiveSink` trait minus `on_macho_byte_source` (which
/// holds an mmap-backed `Arc<dyn Trait>` that doesn't cross a
/// process boundary directly; the server will open the shared cache
/// itself by path in a follow-up).
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum IngestEvent {
    /// Recorder acquired its handle on the target. Fires once at
    /// the start of recording.
    TargetAttached {
        pid: u32,
        task_port: u64,
    },
    Sample(WireSampleEvent),
    OnCpuInterval(WireOnCpuInterval),
    OffCpuInterval(WireOffCpuInterval),
    BinaryLoaded(WireBinaryLoaded),
    BinaryUnloaded(WireBinaryUnloaded),
    ThreadName {
        pid: u32,
        tid: u32,
        name: String,
    },
    Wakeup(WireWakeup),
    /// Race-against-return probe result for one kperf sample.
    /// Correlate against a `Sample` by `(tid, kperf_ts_ns)`.
    ProbeResult(WireProbeResult),
}

#[derive(Clone, Debug, Facet)]
pub struct RunConfig {
    /// Free-form label (typically the launch command's basename).
    pub label: String,
    /// PET sampling frequency the recorder requested, Hz. Surfaced in
    /// `RunSummary` so the UI can label samples.
    pub frequency_hz: u32,
    /// Evaluation mode: shade probes each parsed kperf PET sample by
    /// suspending the sampled thread and emitting a paired
    /// `ProbeResult`. Off by default because it perturbs the target.
    pub race_kperf: bool,
}

#[derive(Clone, Debug, Facet)]
pub struct LaunchEnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Copy, Debug, Facet)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Debug, Facet)]
pub struct LaunchRequest {
    pub command: Vec<String>,
    pub cwd: String,
    pub env: Vec<LaunchEnvVar>,
    pub config: RunConfig,
    pub daemon_socket: String,
    pub time_limit_secs: Option<u64>,
    pub terminal_size: Option<TerminalSize>,
}

#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum TerminalInput {
    Bytes { data: Vec<u8> },
    Resize { size: TerminalSize },
    Close,
}

#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum TerminalOutput {
    Bytes {
        data: Vec<u8>,
    },
    ExitStatus {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Error {
        message: String,
    },
}

/// Recorder → server ingest plane. Open one channel per run; close
/// the channel to signal end-of-recording.
#[vox::service]
pub trait RunIngest {
    /// Open a new run. Returns the assigned `RunId` and consumes the
    /// `events` channel; the server treats channel-close as
    /// end-of-recording. Errors if another run is currently active
    /// — callers should `RunControl::wait_active` or `stop_active`
    /// before retrying.
    async fn start_run(
        &self,
        config: RunConfig,
        events: vox::Rx<IngestEvent>,
    ) -> Result<RunId, String>;

    /// Attach an ingest channel to a run that was already created by
    /// `RunControl::start_attach` / `start_launch`. This is the
    /// server-orchestrated path: the server owns lifecycle and shade
    /// owns recording + ingest.
    async fn attach_run(&self, run_id: RunId, events: vox::Rx<IngestEvent>) -> Result<(), String>;

    /// Reliable, request/response target attachment notification.
    /// Channel sends are not a durability boundary; this method
    /// returns only after stax-server has applied the target state.
    async fn publish_target_attached(
        &self,
        run_id: RunId,
        pid: u32,
        task_port: u64,
    ) -> Result<(), String>;

    /// Reliable, request/response image-load ingest. Binaries define
    /// the address space used by all later symbolication, so they
    /// must not ride on the lossy/high-volume event channel.
    async fn publish_binaries_loaded(
        &self,
        run_id: RunId,
        binaries: Vec<WireBinaryLoaded>,
    ) -> Result<(), String>;

    /// Reliable, request/response image-unload ingest. The current
    /// server retains mappings for historical samples, but keep the
    /// lifecycle event on the reliable plane so future timestamped
    /// image lifetimes don't inherit channel-loss semantics.
    async fn publish_binaries_unloaded(
        &self,
        run_id: RunId,
        binaries: Vec<WireBinaryUnloaded>,
    ) -> Result<(), String>;
}

/// Shade-facing terminal broker. The CLI/native UI provides its
/// terminal channels to `RunControl::start_launch`; the server holds
/// them until the spawned shade connects here with the PTY-side
/// channels. The server only relays bytes/events.
#[vox::service]
pub trait TerminalBroker {
    async fn attach_terminal(
        &self,
        run_id: RunId,
        input_to_shade: vox::Tx<TerminalInput>,
        output_from_shade: vox::Rx<TerminalOutput>,
    ) -> Result<(), String>;
}

/// Agent-facing control plane. One service instance per server; runs
/// are addressed by `RunId`. The web UI uses the existing `Profiler`
/// trait for view subscriptions; agents use `RunControl` for
/// lifecycle + the same `Profiler` for queries (with `subscribe_*`
/// returning a single update being equivalent to a unary call).
#[vox::service]
pub trait RunControl {
    /// Snapshot the server. Returns the active run (if any) plus
    /// server-wide info. Used by `stax status`.
    async fn status(&self) -> ServerStatus;

    /// All runs the server has ever hosted (active + historical
    /// in-memory archive). Bounded by the server's eviction policy
    /// (in-memory only for now; on-disk persistence is a follow-up).
    async fn list_runs(&self) -> Vec<RunSummary>;

    /// Start a recording by attaching stax-shade to an existing pid.
    async fn start_attach(
        &self,
        pid: u32,
        config: RunConfig,
        daemon_socket: String,
        time_limit_secs: Option<u64>,
    ) -> Result<RunId, String>;

    /// Start a recording by launching a new process under stax-shade.
    /// `terminal_input` and `terminal_output` are the frontend side
    /// of the target PTY stream. The CLI merely bridges these to its
    /// local terminal; native/web UIs can render the same stream.
    async fn start_launch(
        &self,
        request: LaunchRequest,
        terminal_input: vox::Rx<TerminalInput>,
        terminal_output: vox::Tx<TerminalOutput>,
    ) -> Result<RunId, String>;

    /// Block until `condition` fires, the active run stops, or
    /// `timeout_ms` elapses (whichever comes first). Returns
    /// `NoActiveRun` immediately when nothing is recording.
    async fn wait_active(&self, condition: WaitCondition, timeout_ms: Option<u64>) -> WaitOutcome;

    /// Ask the recorder to stop the active run cleanly. Returns the
    /// final `RunSummary` once the run has transitioned to `Stopped`.
    /// Errors if no run is active.
    async fn stop_active(&self) -> Result<RunSummary, String>;
}

/// All service descriptors exposed by stax-live; the codegen iterates over
/// this list.
pub fn all_services() -> Vec<&'static vox::session::ServiceDescriptor> {
    vec![
        profiler_service_descriptor(),
        run_control_service_descriptor(),
        run_ingest_service_descriptor(),
        terminal_broker_service_descriptor(),
    ]
}
