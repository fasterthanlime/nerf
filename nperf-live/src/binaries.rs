//! Live binary registry: tracks which images the target has loaded and
//! lazily fetches their on-disk bytes when the user asks for an
//! annotation. Symbol tables come from the sampler (Mach-O `nlist_64`s
//! pulled by `nerf-mac-capture`); the bytes come from disk or the
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
    pub symbols: Vec<LiveSymbolOwned>,
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

pub struct ResolvedAddress {
    pub binary_path: String,
    pub arch: Option<String>,
    pub function_name: String,
    /// Function start address, in the same AVMA space the user clicked.
    pub base_address: u64,
    /// AVMA space, end (exclusive).
    pub end_address: u64,
    /// Bytes of the function. Owned because we copy out of the cached
    /// `CodeImage` (whose `Arc<Vec<u8>>` we don't want to expose past the
    /// registry lock).
    pub bytes: Vec<u8>,
}

pub struct BinaryRegistry {
    /// Loaded images, keyed by base AVMA. Linear scan; tens of entries.
    by_base: Vec<LoadedBinary>,
    /// CodeImage cache keyed by binary path. `Option` so a failed load
    /// is remembered (don't keep re-trying the dyld cache for an image
    /// we already proved isn't there).
    images: std::collections::HashMap<String, Option<Arc<CodeImage>>>,
    /// Lazily-opened macOS dyld shared cache (one per arch).
    dyld_bundle: Option<Option<Arc<DyldCacheBundle>>>,
    dyld_arch: Option<String>,
}

struct DyldCacheBundle {
    main: memmap2::Mmap,
    subcaches: Vec<memmap2::Mmap>,
}

impl BinaryRegistry {
    pub fn new() -> Self {
        Self {
            by_base: Vec::new(),
            images: std::collections::HashMap::new(),
            dyld_bundle: None,
            dyld_arch: None,
        }
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
    }

    pub fn remove(&mut self, base_avma: u64) {
        self.by_base.retain(|b| b.base_avma != base_avma);
    }

    /// Resolve `address` to (function_name, binary_basename) without
    /// loading any image bytes. Used by top-N rendering where we want
    /// labels but don't need disassembly.
    pub fn lookup_symbol(&self, address: u64) -> Option<(String, String)> {
        let binary = self
            .by_base
            .iter()
            .find(|b| address >= b.base_avma && address < b.avma_end)?;
        let svma = svma_for(binary, address);
        let basename = short_path(&binary.path).to_owned();
        // Symbols are sorted by start_svma at insert time. partition_point
        // gives us the first symbol whose start_svma > svma; the candidate
        // containing svma is the one before that (if its end_svma > svma).
        let idx = binary.symbols.partition_point(|s| s.start_svma <= svma);
        let name = if idx > 0 {
            let candidate = &binary.symbols[idx - 1];
            if svma < candidate.end_svma {
                let raw = String::from_utf8_lossy(&candidate.name).into_owned();
                Some(demangle_name(&raw))
            } else {
                None
            }
        } else {
            None
        };
        name.map(|n| (n, basename.clone())).or_else(|| {
            // Binary is mapped but no symbol for this address — still
            // useful to show the basename so the user knows where the
            // sample landed.
            Some((format!("{}+{:#x}", basename, svma), basename))
        })
    }

    /// Resolve `address` (AVMA) into a function: which binary, which
    /// symbol, and the bytes of the function. Lazily loads the binary's
    /// `CodeImage` on first hit.
    pub fn resolve(&mut self, address: u64) -> Option<ResolvedAddress> {
        let binary_idx = self
            .by_base
            .iter()
            .position(|b| address >= b.base_avma && address < b.avma_end)?;

        // Snapshot the bits we need from the binary so we can drop the
        // borrow before touching `self.images` (which `&mut`s self).
        let (path, arch, base_avma, text_svma, sym_idx) = {
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
            )
        };

        let image = self.image_for(&path, arch.as_deref())?;

        // Re-borrow the binary now that the registry mutation is done.
        let binary = &self.by_base[binary_idx];
        let (function_name, fn_start_svma, fn_end_svma) = match sym_idx {
            Some(i) => {
                let s = &binary.symbols[i];
                let raw = String::from_utf8_lossy(&s.name).into_owned();
                (demangle_name(&raw), s.start_svma, s.end_svma)
            }
            None => {
                // No symbol — fall back to a small window around the
                // queried address so the user still sees something useful.
                let svma = svma_for(binary, address);
                let window = 64u64;
                (
                    format!("{}+{:#x}", short_path(&binary.path), svma),
                    svma.saturating_sub(window / 2),
                    svma.saturating_add(window / 2),
                )
            }
        };

        let len = fn_end_svma.saturating_sub(fn_start_svma) as usize;
        if len == 0 {
            return None;
        }
        let bytes = image.fetch(fn_start_svma, len)?.to_vec();

        let base_address = avma_for_svma(base_avma, text_svma, fn_start_svma);
        let end_address = avma_for_svma(base_avma, text_svma, fn_end_svma);
        Some(ResolvedAddress {
            binary_path: path,
            arch,
            function_name,
            base_address,
            end_address,
            bytes,
        })
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

fn demangle_name(raw: &str) -> String {
    // Mach-O typically has a leading underscore on C/C++/Rust symbols.
    let stripped = raw.strip_prefix('_').unwrap_or(raw);
    if let Ok(d) = rustc_demangle::try_demangle(stripped) {
        return format!("{:#}", d);
    }
    if let Ok(d) = cpp_demangle::Symbol::new(stripped) {
        if let Ok(s) = d.demangle(&cpp_demangle::DemangleOptions::default()) {
            return s;
        }
    }
    stripped.to_owned()
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
