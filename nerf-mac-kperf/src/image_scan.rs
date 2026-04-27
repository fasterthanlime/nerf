//! Image enumeration without `task_for_pid`. Walks the target's
//! regions via libproc, coalesces by backing path, and parses each
//! on-disk Mach-O via `object` to extract LC_UUID + LC_SYMTAB and the
//! `__TEXT` SVMA. Diffs against the previous scan and emits
//! BinaryLoaded / BinaryUnloaded events into the sample sink.
//!
//! Shared-cache dylibs (libsystem_*, CoreFoundation, etc.) currently
//! land here with no symbols: the path libproc returns
//! (`/usr/lib/libsystem_malloc.dylib`, ...) is a phantom on Apple
//! Silicon and the bytes live inside `dyld_shared_cache_arm64e*`,
//! which we don't yet parse. Symbol resolution for those addresses
//! is a follow-up.

use std::collections::{HashMap, HashSet};

use nerf_mac_capture::proc_maps::MachOSymbol;
use nerf_mac_capture::{BinaryLoadedEvent, BinaryUnloadedEvent, SampleSink};
use object::read::macho::MachOFile64;
use object::{Endianness, Object, ObjectKind, ObjectSegment, ObjectSymbol};

use nperf_mac_shared_cache::SharedCache;

use crate::libproc;

/// One loaded image, retained between scans so `BinaryLoadedEvent`
/// can borrow `&[MachOSymbol]` from us.
pub struct LoadedImage {
    pub path: String,
    pub base_avma: u64,
    pub vmsize: u64,
    pub text_svma: u64,
    pub uuid: Option<[u8; 16]>,
    pub arch: Option<&'static str>,
    pub is_executable: bool,
    pub symbols: Vec<MachOSymbol>,
    /// `true` for entries seeded from the dyld_shared_cache. Those
    /// don't appear in libproc walks, so the diff-against-libproc
    /// removal pass needs to skip them or it'd unload everything in
    /// the cache on the first rescan.
    pub from_cache: bool,
}

/// Walks libproc and emits BinaryLoaded/BinaryUnloaded for additions
/// and removals since the previous scan.
pub struct ImageScanner {
    known: HashMap<(String, u64), LoadedImage>,
    shared_cache: Option<std::sync::Arc<SharedCache>>,
}

impl ImageScanner {
    pub fn new(shared_cache: Option<std::sync::Arc<SharedCache>>) -> Self {
        Self {
            known: HashMap::new(),
            shared_cache,
        }
    }

    pub fn rescan<S: SampleSink>(&mut self, pid: u32, sink: &mut S) {
        let regions = libproc::enumerate_regions(pid);
        let exec_count = regions.iter().filter(|r| r.is_executable && !r.path.is_empty()).count();
        let first_scan = self.known.is_empty();

        // Seed the dyld shared cache once. libproc doesn't surface
        // cache regions (they live in a shared submap, no vnode path
        // resolution), so without this the entire 0x180000000+ range
        // shows up as `(no binary)` in the live UI.
        if first_scan {
            self.seed_shared_cache(pid, sink);
        }

        if first_scan {
            log::info!(
                "image_scan: pid={pid} -> {} regions returned, {} vnode-backed executable",
                regions.len(),
                exec_count
            );
            // Dump the distribution of paths libproc gave us. We split
            // by exec/non-exec so we can see whether shared-cache
            // regions come through under the cache file's path (and
            // whether they're marked executable).
            let mut by_path: std::collections::BTreeMap<&str, (u32, u32)> = Default::default();
            for r in &regions {
                let entry = by_path.entry(r.path.as_str()).or_default();
                if r.is_executable {
                    entry.0 += 1;
                } else {
                    entry.1 += 1;
                }
            }
            for (path, (exec, non_exec)) in &by_path {
                let label = if path.is_empty() { "<anonymous>" } else { path };
                log::info!("image_scan: path {label}: {exec} exec / {non_exec} non-exec regions");
            }
        }

        // Build the current set: one entry per (path, base_avma) for
        // the executable region of each loaded image. Multiple images
        // can share a path if dlopen happens twice; the avma
        // disambiguates.
        let mut current: HashSet<(String, u64)> = HashSet::new();
        let mut to_add: Vec<(String, u64, u64)> = Vec::new();
        for region in &regions {
            if region.path.is_empty() || !region.is_executable {
                continue;
            }
            let key = (region.path.clone(), region.address);
            if !current.insert(key.clone()) {
                continue;
            }
            if !self.known.contains_key(&key) {
                to_add.push((region.path.clone(), region.address, region.size));
            }
        }

        let to_remove: Vec<(String, u64)> = self
            .known
            .iter()
            .filter(|(k, img)| !img.from_cache && !current.contains(*k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in to_remove {
            if let Some(img) = self.known.remove(&key) {
                sink.on_binary_unloaded(BinaryUnloadedEvent {
                    pid,
                    base_avma: img.base_avma,
                    path: &img.path,
                });
            }
        }

        let to_add_count = to_add.len();
        let mut with_symbols = 0u32;
        let mut total_symbols = 0usize;
        for (path, base_avma, region_size) in to_add {
            let img = build_image(&path, base_avma, region_size, self.shared_cache.as_deref());
            if !img.symbols.is_empty() {
                with_symbols += 1;
                total_symbols += img.symbols.len();
            }
            sink.on_binary_loaded(BinaryLoadedEvent {
                pid,
                base_avma: img.base_avma,
                vmsize: img.vmsize,
                text_svma: img.text_svma,
                path: &img.path,
                uuid: img.uuid,
                arch: img.arch,
                is_executable: img.is_executable,
                symbols: &img.symbols,
                text_bytes: None,
            });
            self.known.insert((path, base_avma), img);
        }
        if first_scan {
            log::info!(
                "image_scan: emitted {to_add_count} BinaryLoaded ({with_symbols} with symbols, \
                 {total_symbols} total symbols)"
            );
        }
    }

    fn seed_shared_cache<S: SampleSink>(&mut self, pid: u32, sink: &mut S) {
        let Some(cache) = self.shared_cache.as_ref() else {
            return;
        };
        let t0 = std::time::Instant::now();
        let images = cache.enumerate_runtime_images();
        let count = images.len();
        let mut total_symbols = 0usize;
        for ci in images {
            total_symbols += ci.symbols.len();
            let img = LoadedImage {
                path: ci.install_name,
                base_avma: ci.runtime_avma,
                vmsize: ci.vmsize,
                text_svma: ci.text_svma,
                uuid: ci.uuid,
                arch: host_arch(),
                is_executable: false,
                symbols: ci.symbols,
                from_cache: true,
            };
            sink.on_binary_loaded(BinaryLoadedEvent {
                pid,
                base_avma: img.base_avma,
                vmsize: img.vmsize,
                text_svma: img.text_svma,
                path: &img.path,
                uuid: img.uuid,
                arch: img.arch,
                is_executable: img.is_executable,
                symbols: &img.symbols,
                text_bytes: None,
            });
            self.known.insert((img.path.clone(), img.base_avma), img);
        }
        log::info!(
            "shared_cache: seeded {count} images / {total_symbols} symbols in {:?}",
            t0.elapsed()
        );
        let _ = pid; // currently unused; logged for symmetry with libproc path
    }
}

/// Try the on-disk Mach-O at `path` first; fall back to the
/// dyld_shared_cache (which is where the bytes actually live for
/// `/usr/lib/...` paths on Apple Silicon); finally fall back to a
/// metadata-only entry so the analysis side at least knows the
/// region exists.
fn build_image(
    path: &str,
    base_avma: u64,
    region_size: u64,
    shared_cache: Option<&SharedCache>,
) -> LoadedImage {
    if let Ok(parsed) = parse_disk_macho(path) {
        return LoadedImage {
            path: path.to_owned(),
            base_avma,
            vmsize: parsed.text_vmsize.max(region_size),
            text_svma: parsed.text_svma,
            uuid: parsed.uuid,
            arch: parsed.arch,
            is_executable: parsed.is_executable,
            symbols: parsed.symbols,
            from_cache: false,
        };
    }
    if let Some(cache) = shared_cache {
        if let Some(img) = cache.lookup(path) {
            return LoadedImage {
                path: path.to_owned(),
                base_avma,
                vmsize: img.text_vmsize.max(region_size),
                text_svma: img.text_svma,
                uuid: img.uuid,
                // Cache is host-arch by construction in shared_cache.rs.
                arch: host_arch(),
                is_executable: false,
                symbols: img.symbols,
                from_cache: false,
            };
        }
    }
    log::trace!("no symbols for {path:?} (neither on-disk nor in dyld cache)");
    LoadedImage {
        path: path.to_owned(),
        base_avma,
        vmsize: region_size,
        text_svma: base_avma,
        uuid: None,
        arch: None,
        is_executable: false,
        symbols: Vec::new(),
        from_cache: false,
    }
}

fn host_arch() -> Option<&'static str> {
    if cfg!(target_arch = "aarch64") {
        Some("aarch64")
    } else if cfg!(target_arch = "x86_64") {
        Some("x86_64")
    } else {
        None
    }
}

struct ParsedMachO {
    text_svma: u64,
    text_vmsize: u64,
    uuid: Option<[u8; 16]>,
    arch: Option<&'static str>,
    is_executable: bool,
    symbols: Vec<MachOSymbol>,
}

fn parse_disk_macho(path: &str) -> Result<ParsedMachO, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("stat: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let file = MachOFile64::<Endianness, _>::parse(&bytes[..])
        .map_err(|e| format!("parse: {e}"))?;

    let text_segment = file
        .segments()
        .find(|s| matches!(s.name(), Ok(Some(name)) if name == "__TEXT"))
        .ok_or("no __TEXT segment")?;
    let text_svma = text_segment.address();
    let text_vmsize = text_segment.size();

    let uuid = file.mach_uuid().ok().flatten();
    let arch = match file.architecture() {
        object::Architecture::Aarch64 => Some("aarch64"),
        object::Architecture::X86_64 => Some("x86_64"),
        _ => None,
    };
    let is_executable = matches!(file.kind(), ObjectKind::Executable);

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

    let mut symbols: Vec<MachOSymbol> = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let start = raw[i].0;
        let end = raw.get(i + 1).map(|(a, _)| *a).unwrap_or(start + 4);
        let name = std::mem::take(&mut raw[i].1);
        symbols.push(MachOSymbol {
            start_svma: start,
            end_svma: end,
            name,
        });
    }

    Ok(ParsedMachO {
        text_svma,
        text_vmsize,
        uuid,
        arch,
        is_executable,
        symbols,
    })
}
