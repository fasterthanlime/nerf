//! `task_info(TASK_DYLD_INFO)` + `dyld_all_image_infos` walk.
//!
//! Layouts mirrored from Apple's dyld source
//! (`include/mach-o/dyld_images.h`) — the format hasn't changed
//! incompatibly since macOS 10.7-ish. We read just enough to
//! discover each loaded image's path + load address.

use std::ffi::CStr;

use mach2::kern_return::KERN_SUCCESS;
use mach2::message::mach_msg_type_number_t;
use mach2::port::mach_port_t;
use mach2::task::task_info;
use mach2::task_info::{TASK_DYLD_INFO, task_info_t};
use mach2::vm::mach_vm_read_overwrite;
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

/// One loaded image as exposed by dyld. Currently a thin record
/// — the unwind-section parsing layer will hang off this in a
/// follow-up commit.
#[derive(Clone, Debug)]
pub struct ImageEntry {
    /// Install path or filesystem path the dynamic loader resolved
    /// the image to.
    pub path: String,
    /// AVMA the image's Mach-O header is mapped at in the target.
    pub load_address: u64,
    /// Last-modified timestamp dyld observed when it loaded the
    /// image. Useful as a poor-man's cache key — pairs of
    /// (path, mtime) are stable across a single run.
    pub file_mod_date: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("task_info(TASK_DYLD_INFO) failed: kr={0}")]
    TaskInfo(i32),
    #[error("mach_vm_read of {what} at {addr:#x} (len {len}) failed: kr={kr}")]
    MachVmRead {
        what: &'static str,
        addr: u64,
        len: usize,
        kr: i32,
    },
    #[error("dyld reported all_image_info_addr=0 — has dyld initialised?")]
    NoImageInfoAddr,
    #[error("invalid path bytes for image at {load_address:#x}")]
    BadPath { load_address: u64 },
}

/// Holds a Mach task port and produces image lists on demand.
/// The port is borrowed (not owned) — the caller is responsible
/// for releasing it, since they're the one who acquired it via
/// `task_for_pid`.
#[derive(Copy, Clone)]
pub struct TargetImageWalker {
    task: mach_port_t,
}

impl TargetImageWalker {
    pub fn new(task: mach_port_t) -> Self {
        Self { task }
    }

    /// Snapshot the target's currently-loaded image list.
    ///
    /// Implementation:
    ///   1. `task_info(TASK_DYLD_INFO)` → AVMA of
    ///      `dyld_all_image_infos`.
    ///   2. `mach_vm_read` that struct.
    ///   3. `mach_vm_read` the image array (length given by the
    ///      struct).
    ///   4. For each `dyld_image_info`: read the path string +
    ///      pull `imageLoadAddress` and `imageFileModDate`.
    ///
    /// Cost: ~4 syscalls + N path reads. With the typical
    /// macOS process loading ~3000 images (most in the cache),
    /// that's ~3000 syscalls per snapshot. Fine for one-shot
    /// session-start enumeration; if we ever want to call this
    /// at the PET frequency we'd batch path reads or watch
    /// `infoArrayChangeTimestamp` to skip the work entirely.
    pub fn enumerate(&self) -> Result<Vec<ImageEntry>, WalkError> {
        let info = self.read_task_dyld_info()?;
        if info.all_image_info_addr == 0 {
            return Err(WalkError::NoImageInfoAddr);
        }
        let header = self.read_struct::<DyldAllImageInfos>(
            info.all_image_info_addr,
            "dyld_all_image_infos",
        )?;
        if header.info_array == 0 || header.info_array_count == 0 {
            return Ok(Vec::new());
        }

        let mut images: Vec<DyldImageInfo> =
            vec![DyldImageInfo::default(); header.info_array_count as usize];
        let bytes_total = std::mem::size_of_val(images.as_slice());
        self.read_into(
            header.info_array,
            "dyld image array",
            // SAFETY: `images` is a unique mut Vec; we cast to
            // its raw byte buffer for the read.
            unsafe { std::slice::from_raw_parts_mut(images.as_mut_ptr().cast::<u8>(), bytes_total) },
        )?;

        let mut out: Vec<ImageEntry> = Vec::with_capacity(images.len());
        for img in &images {
            if img.image_load_address == 0 || img.image_file_path == 0 {
                continue;
            }
            let path = match self.read_c_string(img.image_file_path) {
                Some(p) => p,
                None => {
                    tracing::debug!(
                        load_address = img.image_load_address,
                        "skipping image: path read failed"
                    );
                    continue;
                }
            };
            out.push(ImageEntry {
                path,
                load_address: img.image_load_address,
                file_mod_date: img.image_file_mod_date,
            });
        }
        Ok(out)
    }

    fn read_task_dyld_info(&self) -> Result<TaskDyldInfo, WalkError> {
        let mut info = TaskDyldInfo::default();
        let mut count: mach_msg_type_number_t = TASK_DYLD_INFO_COUNT;
        // SAFETY: out-pointer + length match the kernel's
        // contract; flavour is the constant the struct
        // corresponds to.
        let kr = unsafe {
            task_info(
                self.task,
                TASK_DYLD_INFO,
                &mut info as *mut TaskDyldInfo as task_info_t,
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            return Err(WalkError::TaskInfo(kr));
        }
        Ok(info)
    }

    fn read_into(
        &self,
        addr: u64,
        what: &'static str,
        buf: &mut [u8],
    ) -> Result<(), WalkError> {
        let mut got: mach_vm_size_t = 0;
        // SAFETY: `buf` is a unique mut slice; addr is opaque
        // to the kernel; `got` is an out-pointer.
        let kr = unsafe {
            mach_vm_read_overwrite(
                self.task,
                addr as mach_vm_address_t,
                buf.len() as mach_vm_size_t,
                buf.as_mut_ptr() as mach_vm_address_t,
                &mut got,
            )
        };
        if kr != KERN_SUCCESS || got as usize != buf.len() {
            return Err(WalkError::MachVmRead {
                what,
                addr,
                len: buf.len(),
                kr,
            });
        }
        Ok(())
    }

    fn read_struct<T: Default + Copy>(
        &self,
        addr: u64,
        what: &'static str,
    ) -> Result<T, WalkError> {
        let mut t = T::default();
        let bytes = std::mem::size_of::<T>();
        // SAFETY: T is plain-old-data per its Default + Copy
        // requirement (we only construct repr(C) PODs from this
        // crate).
        let buf = unsafe {
            std::slice::from_raw_parts_mut((&mut t) as *mut T as *mut u8, bytes)
        };
        self.read_into(addr, what, buf)?;
        Ok(t)
    }

    /// Read a NUL-terminated UTF-8 string from the target. Walks
    /// up to 4096 bytes (PATH_MAX-ish); returns `None` if no NUL
    /// is found or the bytes aren't valid UTF-8.
    fn read_c_string(&self, addr: u64) -> Option<String> {
        let mut buf = [0u8; 4096];
        let mut got: mach_vm_size_t = 0;
        // SAFETY: same as `read_into`; we tolerate short reads
        // here because path strings sit at the end of mapped
        // pages and may straddle a boundary.
        let kr = unsafe {
            mach_vm_read_overwrite(
                self.task,
                addr as mach_vm_address_t,
                buf.len() as mach_vm_size_t,
                buf.as_mut_ptr() as mach_vm_address_t,
                &mut got,
            )
        };
        if kr != KERN_SUCCESS || got == 0 {
            return None;
        }
        let slice = &buf[..got as usize];
        let nul = slice.iter().position(|&b| b == 0)?;
        CStr::from_bytes_with_nul(&slice[..=nul])
            .ok()?
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

// ---------------------------------------------------------------------------
// FFI structs mirrored from Apple headers.
// ---------------------------------------------------------------------------

/// `task_dyld_info` from `<mach/task_info.h>`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct TaskDyldInfo {
    all_image_info_addr: u64,
    all_image_info_size: u64,
    all_image_info_format: i32,
    _pad: u32,
}

const TASK_DYLD_INFO_COUNT: mach_msg_type_number_t = (std::mem::size_of::<TaskDyldInfo>()
    / std::mem::size_of::<u32>()) as mach_msg_type_number_t;

/// First fields of `dyld_all_image_infos` from
/// `<mach-o/dyld_images.h>`. We read more than we strictly need
/// so the struct's size matches what the kernel populates,
/// which keeps `read_struct` happy.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct DyldAllImageInfos {
    version: u32,
    info_array_count: u32,
    info_array: u64,
    notification: u64,
    process_detached_from_shared_region: u8,
    libsystem_initialized: u8,
    _pad0: [u8; 6],
    dyld_image_load_address: u64,
    jit_info: u64,
    dyld_version: u64,
    error_message: u64,
    termination_flags: u64,
    core_symbolication_shm_page: u64,
    system_order_flag: u64,
    uuid_array_count: u64,
    uuid_array: u64,
    dyld_all_image_infos_address: u64,
    initial_image_count: u64,
    error_kind: u64,
    error_client_of_dylib_path: u64,
    error_target_dylib_path: u64,
    error_symbol: u64,
    shared_cache_slide: u64,
    shared_cache_uuid: [u8; 16],
    shared_cache_base_address: u64,
    info_array_change_timestamp: u64,
    dyld_path: u64,
    notify_ports: [u32; 8],
    _reserved: [u64; 9],
    compact_dyld_image_info_addr: u64,
    compact_dyld_image_info_size: u64,
    platform: u32,
}

/// `dyld_image_info` from `<mach-o/dyld_images.h>`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct DyldImageInfo {
    image_load_address: u64,
    image_file_path: u64,
    image_file_mod_date: u64,
}
