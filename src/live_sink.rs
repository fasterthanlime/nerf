use std::sync::Arc;

use nwind::UserFrame;

#[cfg(target_os = "macos")]
pub use nerf_mac_capture::MachOByteSource;

pub struct SampleEvent< 'a > {
    pub timestamp: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u32,
    pub kernel_backtrace: &'a [u64],
    pub user_backtrace: &'a [UserFrame],
    /// Synthesised "off-CPU" sample standing in for time the thread
    /// spent blocked between two PET ticks. Stack is borrowed from
    /// the last on-CPU sample, the timestamp is somewhere in the
    /// off-CPU interval. Always `false` on the Linux backend (we
    /// don't synthesise off-CPU samples there yet).
    pub is_offcpu: bool,
    /// CPU cycles consumed by the thread since the previous on-CPU
    /// sample (Apple Silicon fixed PMU counter 0). 0 on Linux and on
    /// off-CPU samples.
    pub cycles: u64,
    /// Instructions retired since the previous on-CPU sample (Apple
    /// Silicon fixed PMU counter 1).
    pub instructions: u64,
    /// L1D cache misses on loads since the previous on-CPU sample,
    /// from a configurable PMU counter. 0 when unavailable.
    pub l1d_misses: u64,
    /// Branch mispredicts since the previous on-CPU sample. Same
    /// availability semantics as `l1d_misses`.
    pub branch_mispreds: u64,
}

/// One symbol from a binary's symbol table (Mach-O `nlist_64` or ELF
/// symtab/dynsym). Addresses are SVMAs (binary-relative; same space as
/// `BinaryLoadedEvent::text_svma`).
pub struct LiveSymbol< 'a > {
    pub start_svma: u64,
    pub end_svma: u64,
    /// Raw, possibly mangled, possibly non-UTF-8 symbol bytes.
    pub name: &'a [u8],
}

pub struct BinaryLoadedEvent< 'a > {
    /// Filesystem path the dynamic loader resolved this image to (or
    /// the dyld cache install-name on macOS for system dylibs).
    pub path: &'a str,
    /// Runtime base address (AVMA) where the image's text segment was
    /// mapped.
    pub base_avma: u64,
    /// Size of the text segment.
    pub vmsize: u64,
    /// SVMA of the text segment in the on-disk binary, i.e. the address
    /// the linker laid out symbols against. `slide = base_avma - text_svma`.
    pub text_svma: u64,
    /// Architecture identifier matching `archive::Packet::MachineInfo`
    /// (e.g. "aarch64", "amd64"). Used to pick the disassembler.
    pub arch: Option< &'a str >,
    /// Whether this image is the main executable (Mach-O `MH_EXECUTE`
    /// / ELF `ET_EXEC`/`ET_DYN` with PIE). The live UI uses this to
    /// visually distinguish target code from system dylibs.
    pub is_executable: bool,
    pub symbols: &'a [LiveSymbol< 'a >],
    /// Raw `__TEXT` bytes for this image, when the recorder captured
    /// them inline (currently: JIT'd code via the jitdump tailer).
    /// Used by the binary registry as a disassembly source for
    /// images that aren't on disk and that we can't `mach_vm_read`
    /// against.
    pub text_bytes: Option< &'a [u8] >,
}

pub struct BinaryUnloadedEvent< 'a > {
    pub path: &'a str,
    pub base_avma: u64,
}

/// One-shot init event sent when the recorder has acquired its handle on
/// the target. `task_port` is the macOS Mach task port (a `mach_port_t`
/// widened to u64); on Linux it's 0 and the registry falls back to
/// `/proc/<pid>/mem`. The registry uses these to read instruction bytes
/// directly from the target when an address falls outside any mapped
/// image (typically JIT'd code).
pub struct TargetAttached {
    pub pid: u32,
    pub task_port: u64,
}

/// One thread in the target acquired (or had its name updated to)
/// `name`. The live aggregator stashes these by tid so the UI can
/// label thread-filter selections.
pub struct ThreadName< 'a > {
    pub pid: u32,
    pub tid: u32,
    pub name: &'a str,
}

/// One observed thread-thread wakeup. `waker_*_stack` is the waker
/// thread's most recent PET tick, so the live aggregator can build
/// a "who woke me?" view per wakee tid -- naming the symbols where
/// the wake-up call was issued from.
pub struct WakeupEvent< 'a > {
    pub timestamp: u64,
    pub pid: u32,
    pub waker_tid: u32,
    pub wakee_tid: u32,
    pub waker_user_stack: &'a [u64],
    pub waker_kernel_stack: &'a [u64],
}

pub trait LiveSink: Send + Sync {
    fn on_sample( &self, event: &SampleEvent );

    /// Recorder acquired its handle on the target. Fires once at the
    /// start of recording, before any samples.
    #[allow(unused_variables)]
    fn on_target_attached( &self, event: &TargetAttached ) {}

    /// A new image was mapped into the target process.
    #[allow(unused_variables)]
    fn on_binary_loaded( &self, event: &BinaryLoadedEvent ) {}

    /// A previously-loaded image was unmapped.
    #[allow(unused_variables)]
    fn on_binary_unloaded( &self, event: &BinaryUnloadedEvent ) {}

    /// A thread was discovered (or renamed). Fires whenever the
    /// recorder learns a tid → name mapping.
    #[allow(unused_variables)]
    fn on_thread_name( &self, event: &ThreadName ) {}

    /// One thread woke another. Backend-specific: only the kperf
    /// path on macOS emits these (via MACH_MAKERUNNABLE kdebug
    /// records). Default no-op so other backends compile.
    #[allow(unused_variables)]
    fn on_wakeup( &self, event: &WakeupEvent ) {}

    /// Recorder hands the live side a typed byte source it can use
    /// to satisfy disassembly requests for addresses inside the
    /// dyld shared cache. Fires once at startup (after the cache
    /// is opened), so the `Arc` is shared between the recorder
    /// (for image enumeration) and the live binary registry (for
    /// the disassembly fallback).
    ///
    /// macOS-only because the cache itself is a macOS construct.
    /// Cross-platform `LiveSink` impls compile fine; nothing on
    /// Linux ever calls this method.
    #[cfg(target_os = "macos")]
    #[allow(unused_variables)]
    fn on_macho_byte_source( &self, source: Arc<dyn MachOByteSource> ) {}
}
