//! The interface between `stax-mac-capture` (capture backend) and the
//! caller (which writes packets to an stax archive). Keeping this trait
//! small lets stax-mac-capture remain ignorant of the on-disk format.

/// Events emitted by the recorder. The caller (stax-core) translates each
/// event into one or more `archive::Packet` writes.
pub trait SampleSink {
    /// One sampled stack trace.
    fn on_sample(&mut self, sample: SampleEvent<'_>);

    /// A new dyld image showed up in the target task.
    fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>);

    /// A previously-known dyld image was unloaded.
    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>);

    /// A thread was discovered for the first time, with a name.
    fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>);

    /// The preload dylib reported the target opened a `jit-<pid>.dump`
    /// file. The default impl does nothing -- only the child-launch path
    /// generates these events.
    #[allow(unused_variables)]
    fn on_jitdump(&mut self, ev: JitdumpEvent<'_>) {}

    /// A `/proc/kallsyms`-style text blob of kernel symbols. The
    /// kperf backend produces one at startup so the analysis side
    /// can resolve `kernel_backtrace` addresses; sinks should embed
    /// it as `Packet::FileBlob { path: "/proc/kallsyms", ... }` so
    /// `data_reader` picks it up via its existing pre-scan.
    #[allow(unused_variables)]
    fn on_kallsyms(&mut self, data: &[u8]) {}

    /// One thread woke another. The waker is whoever was on-CPU on
    /// the cpu that emitted the `MACH_MAKERUNNABLE` record, with the
    /// stack borrowed from its most recent PET tick. Only emitted by
    /// the kperf backend.
    #[allow(unused_variables)]
    fn on_wakeup(&mut self, event: WakeupEvent<'_>) {}

    /// One closed CPU interval. Drives the aggregator's time
    /// attribution: on-CPU intervals get their duration distributed
    /// across the PET samples that fell inside them; off-CPU
    /// intervals get attributed to the cached stack the thread was
    /// running before it parked. Sourced from `MACH_SCHED`
    /// transitions, so durations are ground truth -- no
    /// "samples × period" fabrication.
    #[allow(unused_variables)]
    fn on_cpu_interval(&mut self, event: CpuIntervalEvent<'_>) {}

    /// The recorder opened a shared resource that the live UI can
    /// query for raw bytes (today: the dyld shared cache mmap,
    /// wrapped in an `Arc<dyn MachOByteSource>`). Default no-op so
    /// archive-only sinks ignore it.
    #[allow(unused_variables)]
    fn on_macho_byte_source(&mut self, source: std::sync::Arc<dyn MachOByteSource>) {}

    /// One race-against-return probe result. The recorder produces
    /// these in the staxd-driven path: staxd suspends the sampled
    /// thread shortly after kperf's PMI lands, walks via framehop,
    /// and ships the result alongside the kperf records. Pair
    /// against a `SampleEvent` by matching `(tid, timestamp_ns ==
    /// timing.kperf_ts)`. Default no-op so archive-only sinks ignore.
    #[allow(unused_variables)]
    fn on_probe_result(&mut self, ev: ProbeResultEvent<'_>) {}
}

/// Race-against-return probe output for one kperf sample.
/// `ProbeTiming::kperf_ts` matches the corresponding `SampleEvent`'s
/// `timestamp_ns`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeTiming {
    pub kperf_ts: u64,
    /// mach_absolute_time when the parser enqueued this probe.
    pub enqueued: u64,
    /// mach_absolute_time when a probe worker started this request.
    pub worker_started: u64,
    /// mach_absolute_time after thread-port lookup/cache refresh.
    pub thread_lookup_done: u64,
    /// mach_absolute_time after thread_get_state completed.
    pub state_done: u64,
    /// mach_absolute_time after thread_resume returned.
    pub resume_done: u64,
    /// mach_absolute_time after the remote FP walk completed.
    pub walk_done: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeQueueStats {
    pub coalesced_requests: u64,
    pub worker_batch_len: u32,
}

pub struct ProbeResultEvent<'a> {
    pub tid: u32,
    pub timing: ProbeTiming,
    pub queue: ProbeQueueStats,
    pub mach_pc: u64,
    pub mach_lr: u64,
    pub mach_fp: u64,
    pub mach_sp: u64,
    pub mach_walked: &'a [u64],
    pub used_framehop: bool,
}

/// One PET stack-walk hit: a snapshot of where a thread was at one
/// moment of being on-CPU. Backtraces are callee-most first;
/// addresses are absolute (AVMAs / runtime instruction pointers).
///
/// `SampleEvent` is *only* the stack-identity input. Time accounting
/// happens via `CpuIntervalEvent`s sourced from MACH_SCHED records.
pub struct SampleEvent<'a> {
    pub timestamp_ns: u64,
    pub pid: u32,
    pub tid: u32,
    /// User-space stack. Empty for samples taken while the thread was
    /// in-kernel (and the kperf backend couldn't walk the user side).
    pub backtrace: &'a [u64],
    /// Kernel stack (callee-most first), or empty if the recorder
    /// can't or didn't capture kernel frames. stax-mac-capture (the
    /// suspend-and-walk path) always emits empty here; stax-mac-kperf
    /// fills it when kperf walked the kernel side.
    pub kernel_backtrace: &'a [u64],
    /// CPU cycles consumed since the previous PET sample on this
    /// thread (Apple Silicon fixed counter 0). 0 when not available
    /// (Linux backend, or kperf didn't emit a KPC record for this
    /// sample).
    pub cycles: u64,
    /// Instructions retired since the previous PET sample (Apple
    /// Silicon fixed counter 1). Same availability semantics as
    /// `cycles`.
    pub instructions: u64,
    /// L1 data cache misses on loads since the previous PET sample,
    /// from a configurable counter programmed at session start. 0 if
    /// PMU configuration didn't resolve this event for the host chip.
    pub l1d_misses: u64,
    /// Non-speculative branch mispredicts since the previous PET
    /// sample. Same availability semantics as `l1d_misses`.
    pub branch_mispreds: u64,
}

pub struct BinaryLoadedEvent<'a> {
    pub pid: u32,
    /// Base address (load address) of the image in the target's address space.
    pub base_avma: u64,
    /// Size of the image's `__TEXT` segment.
    pub vmsize: u64,
    /// SVMA of the image's `__TEXT` segment, i.e. the address the linker
    /// laid out symbols against. Subtracting this from a runtime PC and
    /// adding the value back to a `MachOSymbol::start_svma` lets the
    /// analysis side resolve a sample address without knowing the slide.
    pub text_svma: u64,
    pub path: &'a str,
    /// Mach-O LC_UUID, if present.
    pub uuid: Option<[u8; 16]>,
    /// CPU type / subtype string (e.g. `"arm64"`, `"x86_64"`).
    pub arch: Option<&'static str>,
    pub is_executable: bool,
    /// Symbols read from the image's `LC_SYMTAB`, addresses as SVMAs.
    pub symbols: &'a [crate::proc_maps::MachOSymbol],
    /// Raw `__TEXT` bytes for this image, when the recorder has them
    /// in hand. Used by the live UI as a disassembly source for
    /// images that aren't on disk (JIT'd code), so we don't need
    /// `task_for_pid` / `mach_vm_read` against the target. `None`
    /// for normal on-disk images -- they're loaded lazily by the
    /// live registry from their on-disk path.
    pub text_bytes: Option<&'a [u8]>,
}

pub struct BinaryUnloadedEvent<'a> {
    pub pid: u32,
    pub base_avma: u64,
    pub path: &'a str,
}

/// One thread X (the "waker") made another thread Y (the "wakee")
/// runnable -- typically by signalling a condvar, semaphore, or
/// dispatching work into a queue.  The stacks are the waker's most
/// recent on-CPU sample (PET tick), so they're an approximation of
/// where the wake-up call was issued from. Pairs naturally with
/// off-CPU sample emission: the waker's stack here is the same one
/// you'd see on the wakee's flame graph if you flipped to wall-clock
/// mode.
pub struct WakeupEvent<'a> {
    pub timestamp_ns: u64,
    pub pid: u32,
    pub waker_tid: u32,
    pub wakee_tid: u32,
    pub waker_user_stack: &'a [u64],
    pub waker_kernel_stack: &'a [u64],
}

/// Typed byte source for Mach-O addresses that aren't on disk and
/// that we can't `mach_vm_read` against (typically: dyld_shared_cache
/// dylibs on the kperf-launch path). Implementations live in
/// downstream crates that own the underlying mmap; the trait is here
/// so the sink can ferry an `Arc<dyn MachOByteSource>` from the
/// recorder to the live UI without either side having to know the
/// concrete impl.
///
/// `fetch` returns a slice with lifetime tied to `&self` so the
/// implementation can hand back a direct reference into its mmap'd
/// backing without an extra allocation. The caller copies out (Vec,
/// Cow, etc.) before dropping the borrow.
pub trait MachOByteSource: Send + Sync {
    fn fetch<'a>(&'a self, avma: u64, len: usize) -> Option<&'a [u8]>;
}

/// One closed CPU interval delivered by the recorder.
///
/// `kind` separates the two cases:
///
/// - `OnCpu`: the thread was running on a CPU for `[start_ns, end_ns)`.
///   No stack is included -- the aggregator finds the PET samples
///   that fell inside the interval and credits the duration across
///   them. If no PET sample landed in the interval (very short slice
///   between context switches), the interval contributes nothing
///   to flame attribution but still counts toward total CPU time.
///
/// - `OffCpu { stack, ... }`: the thread was blocked at `stack`
///   (the cached PET stack from the moment it parked) for
///   `[start_ns, end_ns)`. Whole interval is credited to that stack;
///   the aggregator classifies the leaf into an `OffCpuReason`
///   bucket. `waker_*` fields carry the MACH_MAKERUNNABLE wakeup
///   attribution when one was caught (often missing for intervals
///   that just happen to not have a wakeup observed yet).
pub struct CpuIntervalEvent<'a> {
    pub pid: u32,
    pub tid: u32,
    /// Start of the interval in the same monotonic timestamp space
    /// as `SampleEvent::timestamp_ns`.
    pub start_ns: u64,
    /// End of the interval (exclusive). Always `>= start_ns`.
    pub end_ns: u64,
    pub kind: CpuIntervalKind<'a>,
}

pub enum CpuIntervalKind<'a> {
    OnCpu,
    OffCpu {
        /// Cached user stack the thread was running just before it
        /// parked. Leaf-first. Empty when no PET sample had been
        /// captured for the thread before the off-CPU transition.
        stack: &'a [u64],
        /// Optional MACH_MAKERUNNABLE attribution: who woke the
        /// thread. None when the wakeup batch hadn't drained yet, or
        /// the interval ended at end-of-recording without a wakeup.
        waker_tid: Option<u32>,
        waker_user_stack: Option<&'a [u64]>,
    },
}

pub struct ThreadNameEvent<'a> {
    pub pid: u32,
    pub tid: u32,
    pub name: &'a str,
}

pub struct JitdumpEvent<'a> {
    pub pid: u32,
    pub path: &'a std::path::Path,
}
