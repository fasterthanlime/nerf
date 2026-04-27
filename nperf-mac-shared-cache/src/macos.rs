//! macOS implementation. See the crate-level docs for the leak
//! rationale.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};

use memmap2::Mmap;
use nerf_mac_capture::proc_maps::MachOSymbol;
use nerf_mac_capture::MachOByteSource;
use object::read::macho::DyldCache;
use object::{Endianness, Object, ObjectSegment, ObjectSymbol};

/// Per-image symbol set returned by `lookup`. `text_svma` is in
/// dyld-cache address space (the same space `MachOSymbol::start_svma`
/// lives in), so the analysis side reconstructs the slide as
/// `base_avma - text_svma` exactly the way it does for on-disk Mach-O.
pub struct SharedCacheImage {
    pub text_svma: u64,
    pub text_vmsize: u64,
    pub uuid: Option<[u8; 16]>,
    pub symbols: Vec<MachOSymbol>,
}

/// One cache image enriched with the runtime address it's mapped at
/// in the target process. Slide is system-wide on Apple Silicon, so
/// the same value applies to every process on this boot.
pub struct CacheRuntimeImage {
    pub install_name: String,
    pub runtime_avma: u64,
    pub text_svma: u64,
    pub vmsize: u64,
    pub uuid: Option<[u8; 16]>,
    pub symbols: Vec<MachOSymbol>,
}

pub struct SharedCache {
    cache: &'static DyldCache<'static, Endianness>,
    /// `runtime_avma - text_svma` for any cache image. Computed once
    /// from our own process's image table -- same value applies to
    /// every other process sharing this cache.
    runtime_slide: Option<i64>,
}

impl SharedCache {
    /// Open the host's shared cache. Returns `None` (with diagnostic
    /// logging) if no compatible cache is found; recording proceeds
    /// without shared-cache symbols rather than failing outright.
    pub fn for_host() -> Option<Self> {
        for path in candidate_main_caches() {
            match try_open(&path) {
                Ok(cache) => {
                    log::info!("dyld_shared_cache opened: {path}");
                    return Some(cache);
                }
                Err(err) => {
                    log::debug!("dyld_shared_cache try_open({path}): {err}");
                }
            }
        }
        log::warn!("no dyld_shared_cache found; libsystem/CoreFoundation symbols will be unresolved");
        None
    }

    /// Slide between cache-stored SVMAs and runtime AVMAs in the
    /// target process. `None` if we couldn't find any cache image in
    /// our own process to anchor against -- which would be very weird
    /// (every macOS process links libSystem).
    pub fn runtime_slide(&self) -> Option<i64> {
        self.runtime_slide
    }

    /// Iterate every image in the cache, parse its Mach-O once, and
    /// hand back a `CacheRuntimeImage` enriched with the target's
    /// runtime address. Drops images that fail to parse or lack a
    /// `__TEXT` segment.
    ///
    /// Parallelised across `NPERF_DYLD_WORKERS` threads (default 8):
    /// per-image LC_SYMTAB parsing is pure CPU work over independent
    /// slices of immutable cache memory, so this scales linearly with
    /// cores. Each worker owns its own output Vec and we concatenate
    /// at join time.
    pub fn enumerate_runtime_images(&self) -> Vec<CacheRuntimeImage> {
        let Some(slide) = self.runtime_slide else {
            return Vec::new();
        };
        let images: Vec<_> = self.cache.images().collect();
        let n_workers = std::env::var("NPERF_DYLD_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1);
        let chunk_size = images.len().div_ceil(n_workers).max(1);

        let mut out: Vec<CacheRuntimeImage> = Vec::with_capacity(images.len());
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in images.chunks(chunk_size) {
                handles.push(scope.spawn(move || {
                    let mut local: Vec<CacheRuntimeImage> = Vec::with_capacity(chunk.len());
                    for image in chunk {
                        let Ok(path) = image.path() else { continue };
                        let Ok(file) = image.parse_object() else { continue };
                        let Some(text) = file
                            .segments()
                            .find(|s| matches!(s.name(), Ok(Some(n)) if n == "__TEXT"))
                        else {
                            continue;
                        };
                        let text_svma = text.address();
                        let vmsize = text.size();
                        let uuid = file.mach_uuid().ok().flatten();
                        let symbols = collect_symbols(&file);
                        let runtime_avma = (text_svma as i64).wrapping_add(slide) as u64;
                        local.push(CacheRuntimeImage {
                            install_name: path.to_owned(),
                            runtime_avma,
                            text_svma,
                            vmsize,
                            uuid,
                            symbols,
                        });
                    }
                    local
                }));
            }
            for h in handles {
                if let Ok(local) = h.join() {
                    out.extend(local);
                }
            }
        });
        out
    }

    /// Resolve `install_name` to LC_SYMTAB symbols + __TEXT extents.
    /// Linear scan; fine for the few-thousand-entry case.
    pub fn lookup(&self, install_name: &str) -> Option<SharedCacheImage> {
        for image in self.cache.images() {
            let Ok(path) = image.path() else { continue };
            if path != install_name {
                continue;
            }
            let file = match image.parse_object() {
                Ok(f) => f,
                Err(err) => {
                    log::debug!(
                        "shared_cache: parse_object failed for {install_name}: {err}"
                    );
                    return None;
                }
            };
            let text = file
                .segments()
                .find(|s| matches!(s.name(), Ok(Some(name)) if name == "__TEXT"))?;
            let text_svma = text.address();
            let text_vmsize = text.size();
            let uuid = file.mach_uuid().ok().flatten();
            let symbols = collect_symbols(&file);
            return Some(SharedCacheImage {
                text_svma,
                text_vmsize,
                uuid,
                symbols,
            });
        }
        None
    }
}

/// `MachOByteSource` impl: translate avma -> cache_svma via slide,
/// walk the parsed mapping table to find which subcache + file
/// offset, and return a slice straight into the leaked mmap. No
/// allocation, no copy.
impl MachOByteSource for SharedCache {
    fn fetch<'a>(&'a self, avma: u64, len: usize) -> Option<&'a [u8]> {
        let slide = self.runtime_slide?;
        let svma = (avma as i64).checked_sub(slide)? as u64;
        let (data, offset) = self.cache.data_and_offset_for_address(svma)?;
        let end = offset.checked_add(len as u64)?;
        // `data` is `&'static [u8]` (the leaked mmap of one
        // subcache file); slice straight in.
        let bytes: &'static [u8] = data;
        let off = usize::try_from(offset).ok()?;
        let end = usize::try_from(end).ok()?;
        if end > bytes.len() {
            return None;
        }
        Some(&bytes[off..end])
    }
}

fn candidate_main_caches() -> Vec<String> {
    let arches: &[&str] = if cfg!(target_arch = "aarch64") {
        &["arm64e"]
    } else if cfg!(target_arch = "x86_64") {
        &["x86_64h", "x86_64"]
    } else {
        &[]
    };
    let prefixes: &[&str] = &[
        // Apple Silicon (cryptex-mounted volume).
        "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
        // Intel (and pre-Big Sur).
        "/System/Library/dyld",
    ];
    let mut out = Vec::new();
    for prefix in prefixes {
        for arch in arches {
            out.push(format!("{prefix}/dyld_shared_cache_{arch}"));
        }
    }
    out
}

fn try_open(main_path: &str) -> Result<SharedCache, String> {
    let main_data = mmap_static(main_path)?;
    let suffixes = DyldCache::<Endianness>::subcache_suffixes(main_data)
        .map_err(|e| format!("subcache_suffixes: {e}"))?;
    let mut sub_data: Vec<&'static [u8]> = Vec::with_capacity(suffixes.len());
    for suffix in &suffixes {
        let path = format!("{main_path}{suffix}");
        sub_data.push(mmap_static(&path)?);
    }
    let cache = DyldCache::<Endianness>::parse(main_data, &sub_data)
        .map_err(|e| format!("DyldCache::parse: {e}"))?;
    let cache: &'static DyldCache<'static, Endianness> = Box::leak(Box::new(cache));
    let runtime_slide = compute_runtime_slide(cache);
    if runtime_slide.is_none() {
        log::warn!(
            "shared_cache: failed to anchor runtime slide; cache images won't be emitted"
        );
    }
    Ok(SharedCache {
        cache,
        runtime_slide,
    })
}

extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(image_index: u32) -> *const c_char;
    fn _dyld_get_image_header(image_index: u32) -> *const c_void;
}

/// Anchor the cache against our own process. The shared region is
/// system-wide on Apple Silicon, so every process sharing the same
/// cache UUID maps it at the same VM address -- which means the
/// slide we observe in *our* image list applies verbatim to the
/// target.
fn compute_runtime_slide(cache: &DyldCache<'static, Endianness>) -> Option<i64> {
    let mut by_name: HashMap<String, u64> = HashMap::with_capacity(4096);
    for image in cache.images() {
        let Ok(path) = image.path() else { continue };
        let Ok(file) = image.parse_object() else { continue };
        if let Some(text) = file
            .segments()
            .find(|s| matches!(s.name(), Ok(Some(n)) if n == "__TEXT"))
        {
            by_name.insert(path.to_owned(), text.address());
        }
    }

    let count = unsafe { _dyld_image_count() };
    for i in 0..count {
        let name_ptr = unsafe { _dyld_get_image_name(i) };
        if name_ptr.is_null() {
            continue;
        }
        let name = match unsafe { CStr::from_ptr(name_ptr).to_str() } {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(&cache_svma) = by_name.get(name) else {
            continue;
        };
        let runtime_addr = unsafe { _dyld_get_image_header(i) } as u64;
        let slide = runtime_addr as i64 - cache_svma as i64;
        log::info!(
            "shared_cache: anchor via {name} runtime={runtime_addr:#x} svma={cache_svma:#x} -> slide={slide:#x}"
        );
        return Some(slide);
    }
    None
}

/// Mmap a file and `Box::leak` so callers see a `&'static [u8]`.
/// See the crate-level docs for why we leak.
fn mmap_static(path: &str) -> Result<&'static [u8], String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| format!("mmap {path}: {e}"))?;
    let leaked: &'static Mmap = Box::leak(Box::new(mmap));
    Ok(&leaked[..])
}

fn collect_symbols<'data, O>(file: &O) -> Vec<MachOSymbol>
where
    O: Object<'data>,
{
    let mut raw: Vec<(u64, Vec<u8>)> = Vec::new();
    for sym in file.symbols() {
        let addr = sym.address();
        if addr == 0 {
            continue;
        }
        let Ok(name) = sym.name_bytes() else { continue };
        if name.is_empty() {
            continue;
        }
        raw.push((addr, name.to_vec()));
    }
    raw.sort_by_key(|(a, _)| *a);
    raw.dedup_by_key(|(a, _)| *a);
    let mut out: Vec<MachOSymbol> = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let start = raw[i].0;
        let end = raw.get(i + 1).map(|(a, _)| *a).unwrap_or(start + 4);
        let name = std::mem::take(&mut raw[i].1);
        out.push(MachOSymbol {
            start_svma: start,
            end_svma: end,
            name,
        });
    }
    out
}
