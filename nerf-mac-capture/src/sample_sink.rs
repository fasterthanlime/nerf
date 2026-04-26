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
}

pub struct BinaryUnloadedEvent<'a> {
    pub pid: u32,
    pub base_avma: u64,
    pub path: &'a str,
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
