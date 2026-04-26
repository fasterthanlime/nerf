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

use crate::libproc;
use crate::shared_cache::SharedCache;

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
}

/// Walks libproc and emits BinaryLoaded/BinaryUnloaded for additions
/// and removals since the previous scan.
pub struct ImageScanner {
    known: HashMap<(String, u64), LoadedImage>,
    shared_cache: Option<SharedCache>,
}

impl ImageScanner {
    pub fn new() -> Self {
        Self {
            known: HashMap::new(),
            shared_cache: SharedCache::for_host(),
        }
    }

    pub fn rescan<S: SampleSink>(&mut self, pid: u32, sink: &mut S) {
        let regions = libproc::enumerate_regions(pid);
        let exec_count = regions.iter().filter(|r| r.is_executable && !r.path.is_empty()).count();
        let first_scan = self.known.is_empty();
        if first_scan {
            log::info!(
                "image_scan: pid={pid} -> {} regions returned, {} vnode-backed executable",
                regions.len(),
                exec_count
            );
            // Surface the distinct executable paths so we can tell at a
            // glance whether shared-cache dylibs come through as their
            // install names, as the cache file itself, or not at all.
            let mut distinct: std::collections::BTreeSet<&str> = Default::default();
            for r in &regions {
                if r.is_executable && !r.path.is_empty() {
                    distinct.insert(r.path.as_str());
                }
            }
            for path in &distinct {
                log::info!("image_scan: exec path {path}");
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
            .keys()
            .filter(|k| !current.contains(*k))
            .cloned()
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
            let img = build_image(&path, base_avma, region_size, self.shared_cache.as_ref());
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
