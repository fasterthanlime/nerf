//! Out-of-process introspection via libproc. Region enumeration and
//! per-thread name/id, all keyed by PID -- no Mach task port required.
//!
//! Lets us drop the `task_for_pid` + `mach_vm_read` path used by
//! `stax-mac-capture::proc_maps` so the kperf child-launch flow keeps
//! working when the parent (root) has dropped the child to a non-root
//! uid: AMFI/task_for_pid policy denies the cross-uid task port even
//! to root, but `proc_pidinfo` is gated only on read-permission and
//! goes through with no fanfare.

use std::ffi::c_void;

use libc::{c_int, ESRCH};

// libc 0.2.186 is missing the PROC_PID* constants and the
// `proc_regionwithpathinfo` struct we need. Declare them here.

const PROC_PIDLISTTHREADS: c_int = 6;
const PROC_PIDREGIONPATHINFO: c_int = 8;

const MAXPATHLEN: usize = 1024;

/// Mirror of `<sys/proc_info.h>` `struct proc_regioninfo`. Layout
/// matches the kernel's verbatim under `#[repr(C)]`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcRegionInfo {
    pri_protection: u32,
    pri_max_protection: u32,
    pri_inheritance: u32,
    pri_flags: u32,
    pri_offset: u64,
    pri_behavior: u32,
    pri_user_wired_count: u32,
    pri_user_tag: u32,
    pri_pages_resident: u32,
    pri_pages_shared_now_private: u32,
    pri_pages_swapped_out: u32,
    pri_pages_dirtied: u32,
    pri_ref_count: u32,
    pri_shadow_depth: u32,
    pri_share_mode: u32,
    pri_private_pages_resident: u32,
    pri_shared_pages_resident: u32,
    pri_obj_id: u32,
    pri_depth: u32,
    pri_address: u64,
    pri_size: u64,
}

/// Mirror of `<sys/proc_info.h>` `struct vinfo_stat`. We don't read
/// any of these fields; they're declared so the layout matches the
/// kernel exactly (in particular, `vst_dev` is uint32_t, not uint64_t
/// -- a transcription error here truncates every path returned in
/// `proc_regionwithpathinfo` by 8 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct VinfoStat {
    vst_dev: u32,
    vst_mode: u16,
    vst_nlink: u16,
    vst_ino: u64,
    vst_uid: u32,
    vst_gid: u32,
    vst_atime: i64,
    vst_atimensec: i64,
    vst_mtime: i64,
    vst_mtimensec: i64,
    vst_ctime: i64,
    vst_ctimensec: i64,
    vst_birthtime: i64,
    vst_birthtimensec: i64,
    vst_size: i64,
    vst_blocks: i64,
    vst_blksize: i32,
    vst_flags: u32,
    vst_gen: u32,
    vst_rdev: u32,
    vst_qspare: [i64; 2],
}

/// Mirror of `<sys/proc_info.h>` `struct vnode_info`.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct VnodeInfo {
    vi_stat: VinfoStat,
    vi_type: i32,
    vi_pad: i32,
    vi_fsid: [i32; 2],
}

/// Mirror of `<sys/proc_info.h>` `struct vnode_info_path`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfoPath {
    vip_vi: VnodeInfo,
    vip_path: [u8; MAXPATHLEN],
}

/// Mirror of `<sys/proc_info.h>` `struct proc_regionwithpathinfo`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcRegionWithPathInfo {
    prp_prinfo: ProcRegionInfo,
    prp_vip: VnodeInfoPath,
}

// Catch any future struct drift before we ship a silently-truncated
// path bug again.
const _: () = {
    assert!(std::mem::size_of::<VinfoStat>() == 136);
    assert!(std::mem::size_of::<VnodeInfo>() == 152);
    assert!(std::mem::size_of::<VnodeInfoPath>() == 1176);
    assert!(std::mem::size_of::<ProcRegionInfo>() == 96);
    assert!(std::mem::size_of::<ProcRegionWithPathInfo>() == 1272);
};

const VM_PROT_READ: u32 = 0x1;
#[allow(dead_code)]
const VM_PROT_WRITE: u32 = 0x2;
const VM_PROT_EXECUTE: u32 = 0x4;

extern "C" {
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
}

/// One entry from a libproc region walk.
#[derive(Clone, Debug)]
pub struct Region {
    pub address: u64,
    pub size: u64,
    pub is_executable: bool,
    pub is_readable: bool,
    /// Filesystem path of the backing vnode, or empty for anonymous
    /// regions (typical for JIT'd code). Truncated at the first NUL.
    pub path: String,
}

/// Walk the target process's address space via
/// `proc_pidinfo(PROC_PIDREGIONPATHINFO)`. Returns one entry per
/// vnode-backed VM region, in ascending address order.
///
/// `PROC_PIDREGIONPATHINFO` advances internally over non-vnode
/// regions and returns the next vnode-backed one; we stop walking on
/// the first negative or zero return code (which the kernel uses to
/// signal "no more vnode-backed regions at or after `arg`"). Errors
/// other than ESRCH/EINVAL get logged but don't propagate.
pub fn enumerate_regions(pid: u32) -> Vec<Region> {
    let mut out = Vec::new();
    let mut addr: u64 = 0;
    let buf_size = std::mem::size_of::<ProcRegionWithPathInfo>() as c_int;
    loop {
        let mut info: ProcRegionWithPathInfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            proc_pidinfo(
                pid as c_int,
                PROC_PIDREGIONPATHINFO,
                addr,
                &mut info as *mut _ as *mut c_void,
                buf_size,
            )
        };
        if n <= 0 {
            if n < 0 {
                let err = std::io::Error::last_os_error();
                let raw = err.raw_os_error();
                if raw == Some(ESRCH) || raw == Some(libc::EINVAL) {
                    // Normal terminator: kernel walked off the end of
                    // vnode-backed regions, or process vanished.
                    log::debug!(
                        "enumerate_regions(pid={pid}): stop at addr={addr:#x}, errno={raw:?} (terminator)"
                    );
                } else {
                    log::warn!(
                        "enumerate_regions(pid={pid}): proc_pidinfo failed at addr={addr:#x}: {err}"
                    );
                }
            }
            break;
        }
        let pri = &info.prp_prinfo;
        let path_bytes = info.prp_vip.vip_path;
        let nul = path_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(path_bytes.len());
        let path = String::from_utf8_lossy(&path_bytes[..nul]).into_owned();
        out.push(Region {
            address: pri.pri_address,
            size: pri.pri_size,
            is_executable: pri.pri_protection & VM_PROT_EXECUTE != 0,
            is_readable: pri.pri_protection & VM_PROT_READ != 0,
            path,
        });
        let next = pri.pri_address.saturating_add(pri.pri_size);
        if next <= addr {
            log::warn!(
                "enumerate_regions(pid={pid}): kernel didn't advance past addr={addr:#x}; bailing"
            );
            break;
        }
        addr = next;
    }
    out
}

/// Mirror of `<sys/proc_info.h>` `struct proc_threadinfo`. libc's
/// `proc_threadinfo` exists but we redeclare with the fields we need
/// to keep the bindings localised.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcThreadInfoC {
    pth_user_time: u64,
    pth_system_time: u64,
    pth_cpu_usage: i32,
    pth_policy: i32,
    pth_run_state: i32,
    pth_flags: i32,
    pth_sleep_time: i32,
    pth_curpri: i32,
    pth_priority: i32,
    pth_maxpriority: i32,
    pth_name: [u8; 64], // MAXTHREADNAMESIZE
}

const PROC_PIDTHREADINFO: c_int = 5;
/// Like `PROC_PIDTHREADINFO` but the `arg` is a kernel
/// `thread_id` (the same value kperf shipped in `arg5` of every
/// PERF sample) instead of a Mach thread_handle. PROC_PIDTHREADINFO
/// silently ESRCH'd every lookup because the recorder fed it
/// kperf's thread_id, which the kernel parses as a thread_handle.
const PROC_PIDTHREADID64INFO: c_int = 14;

/// List the system-wide thread ids belonging to `pid`. The TIDs come
/// out of the kernel as 64-bit values; downstream code in stax
/// truncates to u32 to match the existing archive packet shape.
pub fn list_thread_ids(pid: u32) -> std::io::Result<Vec<u64>> {
    // Start with a generous buffer; resize if the kernel says it needs
    // more (proc_pidinfo returns the byte count it wants to write).
    let mut cap = 64usize;
    loop {
        let mut buf: Vec<u64> = vec![0; cap];
        let n = unsafe {
            proc_pidinfo(
                pid as c_int,
                PROC_PIDLISTTHREADS,
                0,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * std::mem::size_of::<u64>()) as c_int,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let bytes = n as usize;
        let count = bytes / std::mem::size_of::<u64>();
        // If the kernel filled the buffer exactly, it might have had
        // more to give -- grow and retry.
        if count == cap {
            cap *= 2;
            continue;
        }
        buf.truncate(count);
        return Ok(buf);
    }
}

/// Look up the name of one thread inside `pid` by Mach
/// thread-handle (what `PROC_PIDLISTTHREADS` returns). Kept for
/// the older shade-side path that walks via `list_thread_ids`.
pub fn thread_name(pid: u32, tid: u64) -> std::io::Result<Option<String>> {
    thread_name_inner(pid, PROC_PIDTHREADINFO, tid)
}

/// Look up the name of one thread inside `pid` by *kernel*
/// `thread_id` — the same identifier kperf records carry in
/// `arg5` of every PERF sample. Use this when correlating with
/// the kperf stream; `thread_name` keyed by Mach thread-handle
/// returns ESRCH for kperf tids and silently leaves every thread
/// (unnamed) in the live registry.
pub fn thread_name_by_id(pid: u32, thread_id: u64) -> std::io::Result<Option<String>> {
    thread_name_inner(pid, PROC_PIDTHREADID64INFO, thread_id)
}

fn thread_name_inner(pid: u32, flavor: c_int, arg: u64) -> std::io::Result<Option<String>> {
    let mut info: ProcThreadInfoC = unsafe { std::mem::zeroed() };
    let n = unsafe {
        proc_pidinfo(
            pid as c_int,
            flavor,
            arg,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ProcThreadInfoC>() as c_int,
        )
    };
    if n <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let nul = info
        .pth_name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(info.pth_name.len());
    if nul == 0 {
        Ok(None)
    } else {
        Ok(Some(
            String::from_utf8_lossy(&info.pth_name[..nul]).into_owned(),
        ))
    }
}
