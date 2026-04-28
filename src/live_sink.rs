use std::sync::Arc;

use nwind::UserFrame;

#[cfg(target_os = "macos")]
pub use stax_mac_capture::MachOByteSource;

/// One PET stack-walk hit. PET samples are stack-identity *only* in
/// the new model: time accounting comes from `CpuIntervalEvent`s
/// sourced from SCHED records.
pub struct SampleEvent<'a> {
    pub timestamp: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u32,
    pub kernel_backtrace: &'a [u64],
    pub user_backtrace: &'a [UserFrame],
    /// CPU cycles consumed by the thread since the previous on-CPU
    /// sample (Apple Silicon fixed PMU counter 0). 0 on Linux.
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

/// One closed CPU interval. See `stax_mac_capture::CpuIntervalEvent`
/// for the semantics; this is the live-sink mirror with a
/// `UserFrame`-typed stack instead of raw `u64`s so it composes with
/// the rest of the `live_sink` event types.
pub struct CpuIntervalEvent<'a> {
    pub pid: u32,
    pub tid: u32,
    pub start_ns: u64,
    pub end_ns: u64,
    pub kind: CpuIntervalKind<'a>,
}

pub enum CpuIntervalKind<'a> {
    OnCpu,
    OffCpu {
        /// Cached user-space stack at moment of blocking, leaf-first.
        stack: &'a [UserFrame],
        waker_tid: Option<u32>,
        waker_user_stack: Option<&'a [u64]>,
    },
}

/// One symbol from a binary's symbol table (Mach-O `nlist_64` or ELF
/// symtab/dynsym). Addresses are SVMAs (binary-relative; same space as
/// `BinaryLoadedEvent::text_svma`).
pub struct LiveSymbol<'a> {
    pub start_svma: u64,
    pub end_svma: u64,
    /// Raw, possibly mangled, possibly non-UTF-8 symbol bytes.
    pub name: &'a [u8],
}

pub struct BinaryLoadedEvent<'a> {
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
    pub arch: Option<&'a str>,
    /// Whether this image is the main executable (Mach-O `MH_EXECUTE`
    /// / ELF `ET_EXEC`/`ET_DYN` with PIE). The live UI uses this to
    /// visually distinguish target code from system dylibs.
    pub is_executable: bool,
    pub symbols: &'a [LiveSymbol<'a>],
    /// Raw `__TEXT` bytes for this image, when the recorder captured
    /// them inline (currently: JIT'd code via the jitdump tailer).
    /// Used by the binary registry as a disassembly source for
    /// images that aren't on disk and that we can't `mach_vm_read`
    /// against.
    pub text_bytes: Option<&'a [u8]>,
}

pub struct BinaryUnloadedEvent<'a> {
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
pub struct ThreadName<'a> {
    pub pid: u32,
    pub tid: u32,
    pub name: &'a str,
}

/// One observed thread-thread wakeup. `waker_*_stack` is the waker
/// thread's most recent PET tick, so the live aggregator can build
/// a "who woke me?" view per wakee tid -- naming the symbols where
/// the wake-up call was issued from.
pub struct WakeupEvent<'a> {
    pub timestamp: u64,
    pub pid: u32,
    pub waker_tid: u32,
    pub wakee_tid: u32,
    pub waker_user_stack: &'a [u64],
    pub waker_kernel_stack: &'a [u64],
}

/// Race-against-return probe output. Correlates with the matching
/// `SampleEvent` by `(tid, kperf_ts == SampleEvent::timestamp)`.
/// `mach_walked` is the suspended thread's stack from framehop
/// (or FP-walk fallback), leaf-most first, PAC-stripped, no leaf
/// PC. Server resolves through the same BinaryRegistry as kperf
/// samples.
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

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeTiming {
    pub kperf_ts: u64,
    pub enqueued: u64,
    pub worker_started: u64,
    pub thread_lookup_done: u64,
    pub state_done: u64,
    pub resume_done: u64,
    pub walk_done: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeQueueStats {
    pub coalesced_requests: u64,
    pub worker_batch_len: u32,
}

/// Recording-side observer. Methods are async so consumers can do
/// real I/O — buffer drains, vox sends, parallel image walks —
/// without blocking the recorder's runtime.
///
/// Note on hot-path overhead: `on_sample` fires every PET tick
/// (~hundreds of thousands per second on a busy target).
/// async_trait wraps each call in a `Box<dyn Future>`, so the
/// concrete `on_sample` impl should keep its body tiny — push
/// onto a sync channel and return. The actual processing belongs
/// behind that channel.
#[async_trait::async_trait]
pub trait LiveSink: Send + Sync {
    /// Hand the recorder a clonable handle on this sink's "stop
    /// now" signal. `IngestSink` returns `Some(_)` and flips it
    /// when stax-server closes its end of the ingest channel
    /// (typically because `RunControl::stop_active` fired from
    /// another shell); the legacy in-process aggregator returns
    /// `None` and recording runs until SIGINT.
    fn stop_flag(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }

    async fn on_sample(&self, event: &SampleEvent);

    /// Recorder acquired its handle on the target. Fires once at the
    /// start of recording, before any samples.
    #[allow(unused_variables)]
    async fn on_target_attached(&self, event: &TargetAttached) {}

    /// A new image was mapped into the target process.
    #[allow(unused_variables)]
    async fn on_binary_loaded(&self, event: &BinaryLoadedEvent) {}

    /// A previously-loaded image was unmapped.
    #[allow(unused_variables)]
    async fn on_binary_unloaded(&self, event: &BinaryUnloadedEvent) {}

    /// A thread was discovered (or renamed). Fires whenever the
    /// recorder learns a tid → name mapping.
    #[allow(unused_variables)]
    async fn on_thread_name(&self, event: &ThreadName) {}

    /// One thread woke another. Backend-specific: only the kperf
    /// path on macOS emits these (via MACH_MAKERUNNABLE kdebug
    /// records). Default no-op so other backends compile.
    #[allow(unused_variables)]
    async fn on_wakeup(&self, event: &WakeupEvent) {}

    /// One closed CPU interval (on-CPU or off-CPU) sourced from
    /// MACH_SCHED records. Drives the live aggregator's time
    /// attribution. Default no-op for backends that don't track
    /// scheduling events.
    #[allow(unused_variables)]
    async fn on_cpu_interval(&self, event: &CpuIntervalEvent) {}

    /// One race-against-return probe result, paired with a
    /// `SampleEvent` by `(tid, timestamp == kperf_ts)`. Default
    /// no-op so backends without the probe (Linux, anything not
    /// driven by staxd) compile fine.
    #[allow(unused_variables)]
    async fn on_probe_result<'a>(&self, event: &ProbeResultEvent<'a>) {}

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
    async fn on_macho_byte_source(&self, source: Arc<dyn MachOByteSource>) {}
}
