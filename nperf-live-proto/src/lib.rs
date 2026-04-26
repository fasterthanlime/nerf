//! Schema for the nperf live RPC service.
//!
//! This crate is intentionally tiny: it holds only the `#[vox::service]`
//! trait + the wire types. Both `nperf-live` (the runtime that implements
//! and serves the trait) and `xtask` (which generates TypeScript bindings
//! from the trait) depend on this crate. Keeping the schema in its own
//! crate lets `xtask` skip the heavy runtime deps (tokio, transports, etc.)
//! that `nperf-live` pulls in.

use facet::Facet;

#[derive(Clone, Debug, Facet)]
pub struct TopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
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
}

#[derive(Clone, Debug, Facet)]
pub struct TopUpdate {
    pub total_samples: u64,
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
/// synthetic root that aggregates all stacks. Children sum to (or are
/// less than, after pruning) the parent's `count`.
#[derive(Clone, Debug, Facet)]
pub struct FlameNode {
    pub address: u64,
    pub count: u64,
    pub function_name: Option<String>,
    pub binary: Option<String>,
    pub is_main: bool,
    pub children: Vec<FlameNode>,
}

#[derive(Clone, Debug, Facet)]
pub struct FlamegraphUpdate {
    pub total_samples: u64,
    pub root: FlameNode,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadInfo {
    pub tid: u32,
    pub name: Option<String>,
    pub sample_count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct ThreadsUpdate {
    pub threads: Vec<ThreadInfo>,
}

/// One time bucket on the timeline.
#[derive(Clone, Debug, Facet)]
pub struct TimelineBucket {
    /// Bucket start, in nanoseconds since the recording started (i.e.
    /// since the first sample).
    pub start_ns: u64,
    /// Total samples whose timestamp fell into this bucket.
    pub count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TimelineUpdate {
    /// Width of each bucket in nanoseconds.
    pub bucket_size_ns: u64,
    /// Recording duration so the UI can show "Xs elapsed" without
    /// computing it client-side.
    pub duration_ns: u64,
    /// Total samples observed (sum of `count` across `buckets`).
    pub total_samples: u64,
    /// Buckets in chronological order, dense (zero-count buckets in
    /// the middle are emitted so the UI can map x-position → time
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
    /// Resolved name of the target symbol; `None` for unresolved
    /// addresses (JIT, kernel frames, etc.).
    pub function_name: Option<String>,
    pub binary: Option<String>,
    pub is_main: bool,
    /// Total samples that passed through this symbol (sum across
    /// every address resolving to it).
    pub own_count: u64,
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

/// One disassembled instruction with its current sample count.
#[derive(Clone, Debug, Facet)]
pub struct AnnotatedLine {
    pub address: u64,
    /// HTML-highlighted assembly text. Uses arborium's default
    /// `CustomElements` format (`<a-k>mov</a-k>` etc.); the frontend
    /// styles those tags via the generated theme.css. Render with
    /// `dangerouslySetInnerHTML`.
    pub html: String,
    pub self_count: u64,
    /// Set on the first instruction emitted for a new source location.
    /// `None` for instructions that share their source line with the
    /// previous instruction, and for binaries without DWARF.
    pub source_header: Option<SourceHeader>,
}

#[derive(Clone, Debug, Facet)]
pub struct AnnotatedView {
    /// Best-effort symbol name (or hex string fallback).
    pub function_name: String,
    /// Address the disassembly starts at. Used by the client to mark which
    /// line corresponds to the original query address.
    pub base_address: u64,
    pub queried_address: u64,
    pub lines: Vec<AnnotatedLine>,
}

#[vox::service]
pub trait Profiler {
    /// Snapshot of the top-N functions, ranked by `sort`. `tid` filters
    /// to one thread; `None` aggregates across all threads.
    async fn top(&self, limit: u32, sort: TopSort, tid: Option<u32>) -> Vec<TopEntry>;

    /// Stream periodic top-N updates to the client, ranked by `sort`.
    /// `tid` filters to one thread; `None` aggregates across all.
    async fn subscribe_top(
        &self,
        limit: u32,
        sort: TopSort,
        tid: Option<u32>,
        output: vox::Tx<TopUpdate>,
    );

    /// Total number of samples observed since the server started.
    async fn total_samples(&self) -> u64;

    /// Stream annotated disassembly for the function containing
    /// `address`. Sample counts update live; the disassembly itself
    /// only changes if the binary is unloaded/reloaded. `tid` filters
    /// the per-instruction count overlay (the disassembly bytes are
    /// the same regardless).
    async fn subscribe_annotated(
        &self,
        address: u64,
        tid: Option<u32>,
        output: vox::Tx<AnnotatedView>,
    );

    /// Stream periodic flamegraph snapshots. Nodes whose `count` is
    /// below ~0.5% of `total_samples` are pruned to bound the wire
    /// size; children are sorted hot-first.
    async fn subscribe_flamegraph(
        &self,
        tid: Option<u32>,
        output: vox::Tx<FlamegraphUpdate>,
    );

    /// Stream the live list of threads (tid, name, sample count).
    async fn subscribe_threads(&self, output: vox::Tx<ThreadsUpdate>);

    /// Stream a per-thread sample-density timeline, suitable for a
    /// horizontal histogram with brush selection. Buckets are sized
    /// adaptively to keep the count under ~200 regardless of recording
    /// duration. `tid` filters to one thread; `None` aggregates across.
    async fn subscribe_timeline(
        &self,
        tid: Option<u32>,
        output: vox::Tx<TimelineUpdate>,
    );

    /// Stream the callers and callees of the symbol containing
    /// `address`. The walker aggregates across every tree node whose
    /// resolved symbol matches the target, so multiple call sites all
    /// roll up. `tid` filters the call tree to one thread.
    async fn subscribe_neighbors(
        &self,
        address: u64,
        tid: Option<u32>,
        output: vox::Tx<NeighborsUpdate>,
    );
}

/// All service descriptors exposed by nperf-live; the codegen iterates over
/// this list.
pub fn all_services() -> Vec<&'static vox::session::ServiceDescriptor> {
    vec![profiler_service_descriptor()]
}
