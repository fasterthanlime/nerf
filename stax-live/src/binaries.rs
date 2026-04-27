//! Live binary registry: tracks which images the target has loaded and
//! lazily fetches their on-disk bytes when the user asks for an
//! annotation. Symbol tables come from the sampler (Mach-O `nlist_64`s
//! pulled by `stax-mac-capture`); the bytes come from disk or the
//! macOS dyld shared cache.
//!
//! All state here is plain owned `Vec`/`String`; no nwind / no archive
//! plumbing. Fed by `LiveSinkImpl::on_binary_loaded`, which serialises
//! events from the (sync) sampler thread onto the tokio side.

use std::sync::Arc;

/// One symbol from a binary's symtab, owned form (the borrowed
/// `LiveSymbol` only lives for the duration of `on_binary_loaded`).
pub struct LiveSymbolOwned {
    pub start_svma: u64,
    pub end_svma: u64,
    pub name: Vec<u8>,
}

/// One image mapped into the target.
pub struct LoadedBinary {
    pub path: String,
    pub base_avma: u64,
    pub avma_end: u64,
    pub text_svma: u64,
    pub arch: Option<String>,
    /// Mach-O `MH_EXECUTE` (the target's main binary) vs every other
    /// loaded image. Used by the UI to set the main executable apart.
    pub is_executable: bool,
    pub symbols: Vec<LiveSymbolOwned>,
    /// Inline `__TEXT` bytes when the recorder shipped them with the
    /// load event (currently: JIT'd code via the jitdump tailer).
    /// Used by `resolve()` as a third disassembly fallback after
    /// disk-loaded image and `mach_vm_read` against the target task.
    pub text_bytes: Option<Vec<u8>>,
}

/// Cached on-disk image: bytes + segment table, mirroring
/// `cmd_annotate::CodeImage`. Built lazily, possibly from the dyld cache.
pub struct CodeImage {
    pub bytes: Arc<Vec<u8>>,
    pub segments: Vec<CodeSegment>,
}

#[derive(Clone, Copy)]
pub struct CodeSegment {
    pub address: u64,
    pub size: u64,
    pub file_offset: u64,
    pub file_size: u64,
}

impl CodeImage {
    pub fn fetch(&self, start_svma: u64, len: usize) -> Option<&[u8]> {
        let end = start_svma.checked_add(len as u64)?;
        for seg in &self.segments {
            let seg_end = seg.address.checked_add(seg.size)?;
            if seg.address <= start_svma && end <= seg_end {
                let in_segment = start_svma - seg.address;
                if in_segment.checked_add(len as u64)? > seg.file_size {
                    return None;
                }
                let file_off = (seg.file_offset + in_segment) as usize;
                if file_off.checked_add(len)? > self.bytes.len() {
                    return None;
                }
                return Some(&self.bytes[file_off..file_off + len]);
            }
        }
        None
    }
}

pub struct ResolvedSymbol {
    pub function_name: String,
    pub binary: String,
    pub is_main: bool,
    /// Detected source language from demangling. `Unknown` for
    /// addresses without a symbol or where the demangler couldn't
    /// classify the mangling.
    pub language: stax_demangle::Language,
}

pub struct ResolvedAddress {
    pub binary_path: String,
    pub arch: Option<String>,
    pub function_name: String,
    pub language: stax_demangle::Language,
    /// Function start address, in the same AVMA space the user clicked.
    pub base_address: u64,
    /// AVMA space, end (exclusive).
    pub end_address: u64,
    /// Bytes of the function. Owned because we copy out of the cached
    /// `CodeImage` (whose `Arc<Vec<u8>>` we don't want to expose past the
    /// registry lock).
    pub bytes: Vec<u8>,
    /// SVMA the function starts at — i.e. `base_address` translated back
    /// into the binary's symbol-VMA space. We need this to ask DWARF for
    /// the (file, line) of each instruction.
    pub fn_start_svma: u64,
    /// The full image bytes for DWARF lookups. `None` for the
    /// "unmapped target memory" path where we don't have a binary at
    /// all.
    pub image: Option<Arc<CodeImage>>,
}

pub struct BinaryRegistry {
    /// Loaded images. Order is insertion order — used as the
    /// stable identity for `by_base_index` below. Has thousands
    /// of entries once the recorder ships the dyld shared cache,
    /// so don't ever linear-scan this for address lookups.
    by_base: Vec<LoadedBinary>,
    /// Sorted (base_avma, avma_end, by_base_idx) index for
    /// O(log N) address-to-binary lookup. Lazily rebuilt: any
    /// `insert`/`remove` clears it, the next `lookup_symbol`
    /// rebuilds. Wrapped in `Mutex` because lookups happen
    /// behind the outer `RwLock`'s read guard, so we need
    /// interior mutability of just this cache.
    by_base_index: parking_lot::Mutex<Option<Vec<(u64, u64, usize)>>>,
    /// CodeImage cache keyed by binary path. `Option` so a failed load
    /// is remembered (don't keep re-trying the dyld cache for an image
    /// we already proved isn't there).
    images: std::collections::HashMap<String, Option<Arc<CodeImage>>>,
    /// Lazily-opened macOS dyld shared cache (one per arch).
    dyld_bundle: Option<Option<Arc<DyldCacheBundle>>>,
    dyld_arch: Option<String>,
    /// PID + Mach task port handed to us by `on_target_attached`.
    /// Used to read instruction bytes directly from the target when an
    /// address falls outside any mapped image (typically JIT'd code).
    target_pid: Option<u32>,
    target_task_port: Option<u64>,
    /// Typed byte source the recorder shares with us once at attach
    /// for SC dylib disassembly. Last-resort fallback in `resolve`
    /// when no on-disk file, no `mach_vm_read`, and no inline
    /// `text_bytes` are available.
    #[cfg(target_os = "macos")]
    macho_byte_source: Option<Arc<dyn stax_mac_capture::MachOByteSource>>,
    /// Concrete handle on the same dyld shared cache, used for
    /// symbol-name resolution when an address falls outside any
    /// registered binary. The recorder used to emit a
    /// `BinaryLoaded` for every cache image (~3500 of them, ~14M
    /// symbols on the wire); now the server resolves them locally
    /// against this cache instead.
    #[cfg(target_os = "macos")]
    shared_cache: Option<Arc<stax_mac_shared_cache::SharedCache>>,
}

struct DyldCacheBundle {
    main: memmap2::Mmap,
    subcaches: Vec<memmap2::Mmap>,
}

impl BinaryRegistry {
    pub fn new() -> Self {
        Self {
            by_base: Vec::new(),
            by_base_index: parking_lot::Mutex::new(None),
            images: std::collections::HashMap::new(),
            dyld_bundle: None,
            dyld_arch: None,
            target_pid: None,
            target_task_port: None,
            #[cfg(target_os = "macos")]
            macho_byte_source: None,
            #[cfg(target_os = "macos")]
            shared_cache: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn set_shared_cache(&mut self, cache: Arc<stax_mac_shared_cache::SharedCache>) {
        self.shared_cache = Some(cache);
    }

    pub fn set_target(&mut self, pid: u32, task_port: u64) {
        self.target_pid = Some(pid);
        if task_port != 0 {
            self.target_task_port = Some(task_port);
        }
    }

    #[cfg(target_os = "macos")]
    pub fn set_macho_byte_source(
        &mut self,
        source: Arc<dyn stax_mac_capture::MachOByteSource>,
    ) {
        self.macho_byte_source = Some(source);
    }

    pub fn insert(&mut self, mut binary: LoadedBinary) {
        self.by_base.retain(|b| b.base_avma != binary.base_avma);
        if self.dyld_arch.is_none() {
            self.dyld_arch = binary.arch.clone();
        }
        // Sort the symbol table once so lookup_symbol can binary-search.
        // System dylibs have thousands of symbols and we resolve every
        // sampled address on every top-N tick.
        binary.symbols.sort_by_key(|s| s.start_svma);
        self.by_base.push(binary);
        self.invalidate_index();
    }

    pub fn remove(&mut self, base_avma: u64) {
        self.by_base.retain(|b| b.base_avma != base_avma);
        self.invalidate_index();
    }

    fn invalidate_index(&mut self) {
        *self.by_base_index.lock() = None;
    }

    /// Find the binary that covers `address` via binary search on
    /// the lazily-built `by_base_index`. Rebuilds the index on
    /// first call after any insert/remove.
    fn binary_for_address(&self, address: u64) -> Option<usize> {
        let mut guard = self.by_base_index.lock();
        if guard.is_none() {
            let mut idx: Vec<(u64, u64, usize)> = self
                .by_base
                .iter()
                .enumerate()
                .map(|(i, b)| (b.base_avma, b.avma_end, i))
                .collect();
            // Stable sort by base_avma. Overlapping ranges (rare —
            // happens when a binary is replaced and the same range
            // gets reused) are tolerated: partition_point picks the
            // last entry with base_avma <= address, and the
            // end-bound check below filters out a stale prior
            // mapping.
            idx.sort_by_key(|e| e.0);
            *guard = Some(idx);
        }
        let idx = guard.as_ref().expect("just populated");
        // partition_point gives us the first entry with base_avma > address;
        // the candidate is the one before that.
        let pos = idx.partition_point(|e| e.0 <= address);
        if pos == 0 {
            return None;
        }
        let (_, end, by_base_idx) = idx[pos - 1];
        if address < end { Some(by_base_idx) } else { None }
    }

    /// True when any loaded binary has a symbol whose raw name
    /// (LC_SYMTAB / jitdump bytes) contains `needle` as a
    /// substring. Drives `WaitCondition::UntilSymbolSeen`. Cheap
    /// rather than thorough: case-sensitive, no demangling, no
    /// regex. JIT'd names typically arrive un-mangled so a
    /// substring of the source-level function name lands; for
    /// Rust / C++ the user can pass a chunk of the mangled
    /// symbol they're after.
    pub fn any_symbol_contains(&self, needle: &str) -> bool {
        let needle_bytes = needle.as_bytes();
        if needle_bytes.is_empty() {
            return true;
        }
        for binary in &self.by_base {
            for sym in &binary.symbols {
                if memchr::memmem::find(&sym.name, needle_bytes).is_some() {
                    return true;
                }
            }
        }
        false
    }

    /// Resolve `address` to a (function name, binary basename, is-main)
    /// triple without loading any image bytes. Used by top-N rendering
    /// where we want labels but don't need disassembly.
    ///
    /// Falls through to the locally-mapped dyld shared cache when no
    /// registered binary covers the address — this is how dyld
    /// cache-resident symbols (libsystem, libdispatch, …) resolve
    /// without the recorder ever shipping their tables over the wire.
    pub fn lookup_symbol(&self, address: u64) -> Option<ResolvedSymbol> {
        let Some(idx) = self.binary_for_address(address) else {
            #[cfg(target_os = "macos")]
            return self.lookup_symbol_in_shared_cache(address);
            #[cfg(not(target_os = "macos"))]
            return None;
        };
        let binary = &self.by_base[idx];
        let svma = svma_for(binary, address);
        let basename = short_path(&binary.path).to_owned();
        let is_main = binary.is_executable;
        // Symbols are sorted by start_svma at insert time. partition_point
        // gives us the first symbol whose start_svma > svma; the candidate
        // containing svma is the one before that (if its end_svma > svma).
        let idx = binary.symbols.partition_point(|s| s.start_svma <= svma);
        let demangled = if idx > 0 {
            let candidate = &binary.symbols[idx - 1];
            if svma < candidate.end_svma {
                Some(stax_demangle::demangle_bytes(&candidate.name))
            } else {
                None
            }
        } else {
            None
        };
        let (function_name, language) = match demangled {
            Some(d) => (d.name, d.language),
            None => (
                // Binary is mapped but no symbol for this address —
                // still useful to show the basename so the user knows
                // where the sample landed.
                format!("{}+{:#x}", basename, svma),
                stax_demangle::Language::Unknown,
            ),
        };
        Some(ResolvedSymbol {
            function_name,
            binary: basename,
            is_main,
            language,
        })
    }

    /// Look up a sampled address in the locally-mapped dyld
    /// shared cache. Returns the dyld install-name's basename as
    /// `binary` and the demangled symbol name (or the cache-svma
    /// hex if there's no enclosing symbol).
    #[cfg(target_os = "macos")]
    fn lookup_symbol_in_shared_cache(&self, address: u64) -> Option<ResolvedSymbol> {
        let cache = self.shared_cache.as_ref()?;
        let img_ref = cache.lookup_address(address)?;
        let img = img_ref.image();
        let basename = short_path(&img.install_name).to_owned();
        // Cache images are linker-laid-out at `text_svma`; the
        // runtime mapping shifts everything by `runtime_avma -
        // text_svma`. Translate the sampled AVMA back into that
        // SVMA space to match the symbols in the LC_SYMTAB.
        let svma = address.wrapping_sub(img.runtime_avma).wrapping_add(img.text_svma);
        // Symbols are NOT sorted on the cache side — we use
        // partition_point regardless because enumerate_runtime_images
        // emits them in nlist order which is close-to-sorted, but for
        // safety walk linearly. (Tens of thousands of symbols per
        // image; binary search would need a sorted invariant we
        // don't currently enforce on this side.)
        let demangled = img
            .symbols
            .iter()
            .find(|s| svma >= s.start_svma && svma < s.end_svma)
            .map(|s| stax_demangle::demangle_bytes(&s.name));
        let (function_name, language) = match demangled {
            Some(d) => (d.name, d.language),
            None => (
                format!("{}+{:#x}", basename, svma),
                stax_demangle::Language::Unknown,
            ),
        };
        Some(ResolvedSymbol {
            function_name,
            binary: basename,
            is_main: false,
            language,
        })
    }

    /// Resolve `address` (AVMA) into a function: which binary, which
    /// symbol, and the bytes of the function. Lazily loads the binary's
    /// `CodeImage` on first hit. Falls through several layers:
    ///   1. binary mapped + image loadable → bytes from disk (enables DWARF)
    ///   2. binary mapped + image NOT loadable → bytes via mach_vm_read
    ///      (still gives disassembly, just no DWARF/source)
    ///   3. address not in any mapped binary → read window of target memory
    pub fn resolve(&mut self, address: u64) -> Option<ResolvedAddress> {
        let binary_idx = match self.binary_for_address(address) {
            Some(i) => i,
            None => return self.resolve_unmapped(address),
        };

        // Snapshot the bits we need from the binary so we can drop the
        // borrow before touching `self.images` (which `&mut`s self).
        let (path, arch, base_avma, text_svma, sym_idx, inline_bytes) = {
            let binary = &self.by_base[binary_idx];
            let svma = svma_for(binary, address);
            let sym_idx = binary
                .symbols
                .iter()
                .position(|s| svma >= s.start_svma && svma < s.end_svma);
            (
                binary.path.clone(),
                binary.arch.clone(),
                binary.base_avma,
                binary.text_svma,
                sym_idx,
                binary.text_bytes.clone(),
            )
        };

        let image = self.image_for(&path, arch.as_deref());

        // Re-borrow the binary now that the registry mutation is done.
        let binary = &self.by_base[binary_idx];
        let basename = short_path(&binary.path).to_owned();
        let (function_name, language, fn_start_svma, fn_end_svma) = match sym_idx {
            Some(i) => {
                let s = &binary.symbols[i];
                let d = stax_demangle::demangle_bytes(&s.name);
                (d.name, d.language, s.start_svma, s.end_svma)
            }
            None => {
                // No symbol — fall back to a small window around the
                // queried address so the user still sees something useful.
                let svma = svma_for(binary, address);
                let window = 64u64;
                (
                    format!("{}+{:#x}", basename, svma),
                    stax_demangle::Language::Unknown,
                    svma.saturating_sub(window / 2),
                    svma.saturating_add(window / 2),
                )
            }
        };

        let len = fn_end_svma.saturating_sub(fn_start_svma) as usize;
        if len == 0 {
            return None;
        }
        let base_address = avma_for_svma(base_avma, text_svma, fn_start_svma);
        let end_address = avma_for_svma(base_avma, text_svma, fn_end_svma);

        // Disassembly bytes have four fallbacks, in order of
        // preference:
        //   1. on-disk `CodeImage` -- gives us DWARF + source;
        //   2. `mach_vm_read` against the target -- only when we
        //      have a task port (samply backend / `--pid` path);
        //   3. inline `text_bytes` from the load event -- JIT'd
        //      code where the recorder shipped the bytes alongside
        //      the symbol;
        //   4. shared `MachOByteSource` -- typically the dyld
        //      shared cache mmap'd by the recorder, queried by
        //      avma. Lets system-library disassembly work on the
        //      kperf-launch path where there's no on-disk file
        //      and AMFI denies us a task port.
        let bytes = match image
            .as_ref()
            .and_then(|img| img.fetch(fn_start_svma, len))
        {
            Some(b) => b.to_vec(),
            None => match self.read_target_memory(base_address, len) {
                Some(b) => b,
                None => match inline_bytes.as_ref().and_then(|inline| {
                    let off = fn_start_svma.checked_sub(text_svma)? as usize;
                    let end = off.checked_add(len)?;
                    if end > inline.len() {
                        None
                    } else {
                        Some(inline[off..end].to_vec())
                    }
                }) {
                    Some(b) => b,
                    None => self.macho_byte_source_fetch(base_address, len)?,
                },
            },
        };

        Some(ResolvedAddress {
            binary_path: path,
            arch,
            function_name,
            language,
            base_address,
            end_address,
            bytes,
            fn_start_svma,
            image,
        })
    }

    /// Fallback when `address` falls outside every mapped image: read
    /// instruction bytes straight out of the target's address space.
    /// Returns `None` if we never got a target task port (Linux, or
    /// pre-attach), or if the read fails (page unmapped, etc.).
    fn resolve_unmapped(&self, address: u64) -> Option<ResolvedAddress> {
        // ±128 bytes of context around the queried address. Aligned down
        // to a 4-byte boundary so the aarch64 disassembler stays in sync.
        const WINDOW: u64 = 256;
        let half = WINDOW / 2;
        let base_address = (address.saturating_sub(half)) & !0x3;
        let bytes = self.read_target_memory(base_address, WINDOW as usize)?;
        // Detect the host arch from the dyld cache hint we picked up
        // during `insert`; for unmapped addresses we have no per-binary
        // arch tag of our own.
        let arch = self.dyld_arch.clone();
        Some(ResolvedAddress {
            binary_path: String::from("(target memory)"),
            arch,
            function_name: format!("(unmapped) {:#x}", address),
            language: stax_demangle::Language::Unknown,
            base_address,
            end_address: base_address + bytes.len() as u64,
            bytes,
            fn_start_svma: base_address,
            image: None,
        })
    }

    #[cfg(target_os = "macos")]
    fn read_target_memory(&self, address: u64, len: usize) -> Option<Vec<u8>> {
        use mach2::kern_return::KERN_SUCCESS;
        use mach2::vm::mach_vm_read_overwrite;
        use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

        let task = self.target_task_port? as mach2::port::mach_port_t;
        let mut buf = vec![0u8; len];
        let mut got: mach_vm_size_t = 0;
        let kr = unsafe {
            mach_vm_read_overwrite(
                task,
                address as mach_vm_address_t,
                len as mach_vm_size_t,
                buf.as_mut_ptr() as mach_vm_address_t,
                &mut got,
            )
        };
        if kr != KERN_SUCCESS {
            tracing::debug!(
                "mach_vm_read_overwrite({:#x}, {}) failed: kr={}",
                address,
                len,
                kr
            );
            return None;
        }
        buf.truncate(got as usize);
        Some(buf)
    }

    #[cfg(not(target_os = "macos"))]
    fn read_target_memory(&self, _address: u64, _len: usize) -> Option<Vec<u8>> {
        // TODO: pread /proc/<pid>/mem on Linux.
        None
    }

    /// Fourth-tier disassembly fallback (after disk image,
    /// `mach_vm_read`, and inline `text_bytes`). Asks the shared
    /// `MachOByteSource` for `len` bytes at `address`; on success
    /// the trait hands us a slice straight into the leaked cache
    /// mmap and we copy out into an owned `Vec` to fit the existing
    /// `ResolvedAddress::bytes` shape.
    #[cfg(target_os = "macos")]
    fn macho_byte_source_fetch(&self, address: u64, len: usize) -> Option<Vec<u8>> {
        let src = self.macho_byte_source.as_ref()?;
        src.fetch(address, len).map(|b| b.to_vec())
    }

    #[cfg(not(target_os = "macos"))]
    fn macho_byte_source_fetch(&self, _address: u64, _len: usize) -> Option<Vec<u8>> {
        None
    }

    fn image_for(&mut self, path: &str, arch: Option<&str>) -> Option<Arc<CodeImage>> {
        if let Some(entry) = self.images.get(path) {
            return entry.clone();
        }
        let loaded = load_image(path).or_else(|| {
            // Try the macOS dyld shared cache for system-only install paths.
            self.dyld_image(path, arch)
        });
        self.images.insert(path.to_owned(), loaded.clone());
        loaded
    }

    fn dyld_image(&mut self, path: &str, arch: Option<&str>) -> Option<Arc<CodeImage>> {
        let arch = arch.or(self.dyld_arch.as_deref())?;
        if self.dyld_bundle.is_none() {
            self.dyld_bundle = Some(open_local_dyld_cache(arch).map(Arc::new));
        }
        let bundle = self.dyld_bundle.as_ref()?.clone()?;
        let main: &[u8] = &bundle.main;
        let sub: Vec<&[u8]> = bundle.subcaches.iter().map(|m| &m[..]).collect();
        let cache =
            object::read::macho::DyldCache::<object::Endianness>::parse(main, &sub).ok()?;
        for image in cache.images() {
            let img_path = match image.path() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if img_path != path {
                continue;
            }
            let parsed = image.parse_object().ok()?;
            return Some(Arc::new(macho_to_code_image(&parsed)));
        }
        None
    }
}

fn svma_for(binary: &LoadedBinary, address: u64) -> u64 {
    address.wrapping_sub(binary.base_avma).wrapping_add(binary.text_svma)
}

fn avma_for_svma(base_avma: u64, text_svma: u64, svma: u64) -> u64 {
    svma.wrapping_sub(text_svma).wrapping_add(base_avma)
}

fn short_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn load_image(path: &str) -> Option<Arc<CodeImage>> {
    if path.starts_with('[') || path.is_empty() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let bytes = Arc::new(bytes);
    let segments = parse_segments(&bytes)?;
    Some(Arc::new(CodeImage { bytes, segments }))
}

fn parse_segments(bytes: &[u8]) -> Option<Vec<CodeSegment>> {
    use object::{Object, ObjectSegment};
    let file = object::File::parse(bytes).ok()?;
    let mut segments = Vec::new();
    for seg in file.segments() {
        let (file_offset, file_size) = seg.file_range();
        segments.push(CodeSegment {
            address: seg.address(),
            size: seg.size(),
            file_offset,
            file_size,
        });
    }
    Some(segments)
}

fn macho_to_code_image(file: &object::File) -> CodeImage {
    use object::{Object, ObjectSegment};
    let mut combined: Vec<u8> = Vec::new();
    let mut segments: Vec<CodeSegment> = Vec::new();
    for seg in file.segments() {
        let data = match seg.data() {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.is_empty() {
            continue;
        }
        let file_offset = combined.len() as u64;
        combined.extend_from_slice(data);
        segments.push(CodeSegment {
            address: seg.address(),
            size: seg.size(),
            file_offset,
            file_size: data.len() as u64,
        });
    }
    CodeImage {
        bytes: Arc::new(combined),
        segments,
    }
}

fn open_local_dyld_cache(arch: &str) -> Option<DyldCacheBundle> {
    let suffixes: &[&str] = match arch {
        "aarch64" => &["arm64e", "arm64"],
        "amd64" => &["x86_64h", "x86_64"],
        _ => return None,
    };
    let prefixes: &[&str] = &[
        "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
        "/System/Cryptexes/OS/System/Library/dyld",
        "/System/Library/dyld",
    ];
    for prefix in prefixes {
        for suffix in suffixes {
            let main_path =
                std::path::Path::new(prefix).join(format!("dyld_shared_cache_{}", suffix));
            if !main_path.exists() {
                continue;
            }
            if let Ok(bundle) = open_dyld_bundle(&main_path) {
                return Some(bundle);
            }
        }
    }
    None
}

fn open_dyld_bundle(main_path: &std::path::Path) -> std::io::Result<DyldCacheBundle> {
    let main_file = std::fs::File::open(main_path)?;
    let main = unsafe { memmap2::Mmap::map(&main_file)? };

    let parent = main_path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no parent"))?;
    let stem = main_path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no file name"))?
        .to_string_lossy()
        .into_owned();

    let mut subcaches = Vec::new();
    for i in 1.. {
        let p = parent.join(format!("{}.{}", stem, i));
        if !p.exists() {
            break;
        }
        let f = std::fs::File::open(&p)?;
        subcaches.push(unsafe { memmap2::Mmap::map(&f)? });
    }
    let symbols = parent.join(format!("{}.symbols", stem));
    if symbols.exists() {
        let f = std::fs::File::open(&symbols)?;
        subcaches.push(unsafe { memmap2::Mmap::map(&f)? });
    }
    Ok(DyldCacheBundle { main, subcaches })
}
