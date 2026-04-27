//! The interface between `nerf-mac-capture` (capture backend) and the
//! caller (which writes packets to an nperf archive). Keeping this trait
//! small lets nerf-mac-capture remain ignorant of the on-disk format.

/// Events emitted by the recorder. The caller (nperf-core) translates each
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

    /// The recorder opened a shared resource that the live UI can
    /// query for raw bytes (today: the dyld shared cache mmap,
    /// wrapped in an `Arc<dyn MachOByteSource>`). Default no-op so
    /// archive-only sinks ignore it.
    #[allow(unused_variables)]
    fn on_macho_byte_source(&mut self, source: std::sync::Arc<dyn MachOByteSource>) {}
}

/// One sample. Backtraces are callee-most first; addresses are absolute
/// (i.e. AVMAs in samply terminology, runtime instruction pointers).
pub struct SampleEvent<'a> {
    pub timestamp_ns: u64,
    pub pid: u32,
    pub tid: u32,
    /// User-space stack. Empty for samples taken while the thread was
    /// in-kernel (and the kperf backend couldn't walk the user side).
    pub backtrace: &'a [u64],
    /// Kernel stack (callee-most first), or empty if the recorder
    /// can't or didn't capture kernel frames. nerf-mac-capture (the
    /// suspend-and-walk path) always emits empty here; nerf-mac-kperf
    /// fills it when kperf walked the kernel side.
    pub kernel_backtrace: &'a [u64],
    /// `true` if this is a synthesised "off-CPU" sample standing in
    /// for time the thread spent blocked between two PET ticks. The
    /// stack is borrowed from the last on-CPU sample, the timestamp
    /// is somewhere in the off-CPU interval. samply has no equivalent.
    pub is_offcpu: bool,
    /// CPU cycles consumed since the previous PET sample on this
    /// thread (Apple Silicon fixed counter 0). 0 when not available
    /// (Linux backend, off-CPU samples, or kperf didn't emit a KPC
    /// record for this sample).
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

pub struct ThreadNameEvent<'a> {
    pub pid: u32,
    pub tid: u32,
    pub name: &'a str,
}

pub struct JitdumpEvent<'a> {
    pub pid: u32,
    pub path: &'a std::path::Path,
}
