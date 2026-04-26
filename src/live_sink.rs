use nwind::UserFrame;

pub struct SampleEvent< 'a > {
    pub timestamp: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u32,
    pub kernel_backtrace: &'a [u64],
    pub user_backtrace: &'a [UserFrame],
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
}
