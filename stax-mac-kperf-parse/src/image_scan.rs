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
use std::ffi::c_void;

use object::read::macho::MachOFile64;
use object::{Endianness, Object, ObjectKind, ObjectSegment, ObjectSymbol};
use stax_mac_capture::proc_maps::MachOSymbol;
use stax_mac_capture::{BinaryLoadedEvent, BinaryUnloadedEvent, SampleSink};

use stax_mac_shared_cache::SharedCache;

use crate::libproc;

// ——— Mach FFI for fast dyld image-change detection ———

#[allow(non_camel_case_types)]
mod ffi {
    pub(super) type mach_port_t = u32;
    pub(super) type mach_vm_address_t = u64;
    pub(super) type mach_vm_size_t = u64;
    pub(super) type kern_return_t = i32;
    pub(super) type natural_t = u32;
    pub(super) type mach_msg_type_number_t = natural_t;
    pub(super) type integer_t = i32;
}
use ffi::*;

const KERN_SUCCESS: kern_return_t = 0;

const TASK_DYLD_INFO: i32 = 17;
const TASK_DYLD_INFO_COUNT: mach_msg_type_number_t = (std::mem::size_of::<TaskDyldInfo>()
    / std::mem::size_of::<natural_t>())
    as mach_msg_type_number_t;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TaskDyldInfo {
    all_image_info_addr: mach_vm_address_t,
    all_image_info_size: mach_vm_size_t,
    all_image_info_format: integer_t,
}

extern "C" {
    fn task_info(
        task: mach_port_t,
        flavor: i32,
        task_info_out: *mut c_void,
        task_info_out_cnt: *mut mach_msg_type_number_t,
    ) -> kern_return_t;

    fn mach_vm_read_overwrite(
        target_task: mach_port_t,
        address: mach_vm_address_t,
        size: mach_vm_size_t,
        data: mach_vm_address_t,
        out_size: *mut mach_vm_size_t,
    ) -> kern_return_t;
}

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
    /// Optional Mach task port for fast dyld image-change detection.
    /// When set, `rescan` checks `task_info(TASK_DYLD_INFO)` +
    /// `mach_vm_read_overwrite` to read the dyld image count before
    /// falling back to the full `proc_pidinfo` walk.
    task: Option<mach_port_t>,
    /// Last seen `dyld_all_image_infos.all_image_info_addr`.
    last_all_image_info_addr: u64,
    /// Last seen `dyld_all_image_infos.infoArrayCount`.
    last_image_count: u32,
}

impl ImageScanner {
    pub fn new(
        shared_cache: Option<std::sync::Arc<SharedCache>>,
        task: Option<mach_port_t>,
    ) -> Self {
        Self {
            known: HashMap::new(),
            shared_cache,
            task,
            last_all_image_info_addr: 0,
            last_image_count: 0,
        }
    }

    pub fn rescan<S: SampleSink>(&mut self, pid: u32, sink: &mut S) {
        // Fast path: if we have a task port, check whether dyld's
        // image count changed since last scan. This avoids the full
        // proc_pidinfo walk (~hundreds of syscalls per tick) in
        // steady state.
        if let Some(task) = self.task {
            if !self.dyld_images_changed(task) {
                return;
            }
        }

        let regions = libproc::enumerate_regions(pid);
        let exec_count = regions
            .iter()
            .filter(|r| r.is_executable && !r.path.is_empty())
            .count();
        let first_scan = self.known.is_empty();

        // We used to seed the dyld shared cache here — emitting one
        // BinaryLoaded per cache image, ~3500 images / ~14M symbols
        // synchronously into the sink. That hammered the recorder's
        // runtime for several seconds at session start, broke vox
        // keepalive, and shipped megabytes the server didn't need:
        // the server has its own SharedCache mapped locally and
        // resolves cache-resident addresses against it directly.

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
            if img.is_executable {
                log::warn!(
                    "image_scan: registering executable {:?} base_avma={:#x} \
                     end={:#x} vmsize={:#x} (region_size={:#x}) symbols={}",
                    img.path,
                    img.base_avma,
                    img.base_avma + img.vmsize,
                    img.vmsize,
                    region_size,
                    img.symbols.len(),
                );
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

    /// Returns `true` when dyld's image list has changed (or this is
    /// the first check). Uses `task_info(TASK_DYLD_INFO)` to find the
    /// `dyld_all_image_infos` struct, then reads `infoArrayCount` from
    /// the target's address space via `mach_vm_read_overwrite`.
    fn dyld_images_changed(&mut self, task: mach_port_t) -> bool {
        let mut info: TaskDyldInfo = TaskDyldInfo::default();
        let mut count = TASK_DYLD_INFO_COUNT;
        let kr = unsafe {
            task_info(
                task,
                TASK_DYLD_INFO,
                &mut info as *mut _ as *mut c_void,
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            // Can't read dyld info — fall back to full rescan.
            return true;
        }
        if info.all_image_info_addr == 0 {
            return true;
        }

        // Read just infoArrayCount (u32 at offset 4) from the target.
        let mut image_count: u32 = 0;
        let mut out_size: mach_vm_size_t = 0;
        let kr = unsafe {
            mach_vm_read_overwrite(
                task,
                info.all_image_info_addr + 4, // offsetof(infoArrayCount)
                4,                            // sizeof(u32)
                &mut image_count as *mut _ as mach_vm_address_t,
                &mut out_size,
            )
        };
        if kr != KERN_SUCCESS || out_size != 4 {
            // Read failed — fall back to full rescan.
            self.last_all_image_info_addr = 0;
            return true;
        }

        let changed = info.all_image_info_addr != self.last_all_image_info_addr
            || image_count != self.last_image_count;
        self.last_all_image_info_addr = info.all_image_info_addr;
        self.last_image_count = image_count;
        changed
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
    let file =
        MachOFile64::<Endianness, _>::parse(&bytes[..]).map_err(|e| format!("parse: {e}"))?;

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
