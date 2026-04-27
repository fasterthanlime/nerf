//! Thin wrappers around `sysctl(CTL_KERN, KERN_KDEBUG, op, ...)`.
//!
//! kdebug is the per-CPU ringbuffer of fixed-size events that
//! kperf's PET sampler writes its records into. We use the same
//! interface to allocate the ring, install a debugid filter,
//! enable/disable the ring, and drain it with `KERN_KDREADTR`.

use std::io;
use std::mem;

use libc::{c_int, c_void, sysctl, sysctlbyname};

use crate::error::Error;

// ---------------------------------------------------------------------------
// sysctl identifiers
// ---------------------------------------------------------------------------

pub const CTL_KERN: c_int = 1;
/// `KERN_KDEBUG` — value confirmed against the macOS 26.4 SDK at
/// `/Applications/Xcode.app/.../usr/include/sys/sysctl.h`. xnu's
/// open-source mirror sometimes lists 38 in older or out-of-tree
/// docs; the deployed value is 24.
pub const KERN_KDEBUG: c_int = 24;

pub const KERN_KDEFLAGS: c_int = 1;
pub const KERN_KDDFLAGS: c_int = 2;
pub const KERN_KDENABLE: c_int = 3;
pub const KERN_KDSETBUF: c_int = 4;
pub const KERN_KDGETBUF: c_int = 5;
pub const KERN_KDSETUP: c_int = 6;
pub const KERN_KDREMOVE: c_int = 7;
pub const KERN_KDSETREG: c_int = 8;
pub const KERN_KDGETREG: c_int = 9;
pub const KERN_KDREADTR: c_int = 10;
pub const KERN_KDPIDTR: c_int = 11;
pub const KERN_KDTHRMAP: c_int = 12;
pub const KERN_KDPIDEX: c_int = 14;
pub const KERN_KDWRITETR: c_int = 17;
pub const KERN_KDWRITEMAP: c_int = 18;
pub const KERN_KDREADCURTHRMAP: c_int = 21;
pub const KERN_KDSET_TYPEFILTER: c_int = 22;
pub const KERN_KDCPUMAP: c_int = 24;

// ---------------------------------------------------------------------------
// debugid encoding
// ---------------------------------------------------------------------------

pub const KDBG_CLASS_MASK: u32 = 0xff00_0000;
pub const KDBG_CLASS_SHIFT: u32 = 24;
pub const KDBG_SUBCLASS_MASK: u32 = 0x00ff_0000;
pub const KDBG_SUBCLASS_SHIFT: u32 = 16;
pub const KDBG_CODE_MASK: u32 = 0x0000_fffc;
pub const KDBG_CODE_SHIFT: u32 = 2;
pub const KDBG_FUNC_MASK: u32 = 0x0000_0003;
pub const KDBG_EVENTID_MASK: u32 = 0xffff_fffc;

pub const DBG_FUNC_NONE: u32 = 0;
pub const DBG_FUNC_START: u32 = 1;
pub const DBG_FUNC_END: u32 = 2;

/// `kd_buf::timestamp` packs the 56-bit kernel mach-time value in
/// the low bits and the cpuid in the high 8. Mask off the cpuid
/// before treating it as time.
pub const KDBG_TIMESTAMP_MASK: u64 = 0x00ff_ffff_ffff_ffff;

#[inline]
pub const fn kdbg_eventid(class: u8, subclass: u8, code: u16) -> u32 {
    ((class as u32) << KDBG_CLASS_SHIFT)
        | ((subclass as u32) << KDBG_SUBCLASS_SHIFT)
        | (((code & 0x3fff) as u32) << KDBG_CODE_SHIFT)
}

#[inline]
pub const fn kdbg_class(debugid: u32) -> u8 {
    ((debugid & KDBG_CLASS_MASK) >> KDBG_CLASS_SHIFT) as u8
}

#[inline]
pub const fn kdbg_subclass(debugid: u32) -> u8 {
    ((debugid & KDBG_SUBCLASS_MASK) >> KDBG_SUBCLASS_SHIFT) as u8
}

#[inline]
pub const fn kdbg_code(debugid: u32) -> u16 {
    ((debugid & KDBG_CODE_MASK) >> KDBG_CODE_SHIFT) as u16
}

#[inline]
pub const fn kdbg_func(debugid: u32) -> u32 {
    debugid & KDBG_FUNC_MASK
}

// ---------------------------------------------------------------------------
// Filter (kd_regtype) types
// ---------------------------------------------------------------------------

pub const KDBG_CLASSTYPE: u32 = 0x10000;
pub const KDBG_SUBCLSTYPE: u32 = 0x20000;
pub const KDBG_RANGETYPE: u32 = 0x40000;
pub const KDBG_TYPENONE: u32 = 0x80000;

pub const KDBG_RANGECHECK: u32 = 0x100000;
pub const KDBG_VALCHECK: u32 = 0x200000;

// ---------------------------------------------------------------------------
// Event classes we care about
// ---------------------------------------------------------------------------

/// Mach (scheduler, IPC, VM, etc.). Includes context switches under
/// subclass `DBG_MACH_SCHED`.
pub const DBG_MACH: u8 = 1;
pub const DBG_MACH_SCHED: u8 = 0x40;

/// Specific scheduler codes within `DBG_MACH_SCHED`. From xnu
/// `osfmk/kern/sched_prim.h` / `mach/sched_prim.h`.
pub mod mach_sched {
    /// Context switch. arg1 = new pthread_id (continuation if 0),
    /// arg2 = new thread tid, arg3 = old thread runq priority,
    /// arg4 = old thread sched flags.
    pub const SCHED: u16 = 0x0;
    /// Stack-handoff context switch (fast path, used for
    /// thread_block_parameter on a target thread).
    pub const STKHANDOFF: u16 = 0x8;
    /// A blocked thread became runnable. arg1 = thread tid.
    pub const MAKERUNNABLE: u16 = 0x4;
    /// Thread is about to block. arg1 = wait_event,
    /// arg2 = wait_result, arg3 = wait_timeout, arg4 = thread tid.
    pub const BLOCK: u16 = 0x18;
    /// Thread is waiting (cond/sem/lock).
    pub const WAIT: u16 = 0x14;
}

pub const DBG_PERF: u8 = 37;

/// kperf event encoding, mirroring xnu's `osfmk/kperf/buffer.h`.
///
/// Subclasses of `DBG_PERF` (37):
pub mod perf {
    /// Subclass codes within `DBG_PERF`.
    pub mod sc {
        pub const GENERIC: u8 = 0;
        pub const THREADINFO: u8 = 1;
        pub const CALLSTACK: u8 = 2;
        pub const TIMER: u8 = 3;
        pub const PET: u8 = 4;
        pub const AST: u8 = 5;
        pub const KPC: u8 = 6;
        pub const KDBG: u8 = 7;
        pub const TASK: u8 = 8;
        pub const LAZY: u8 = 9;
        pub const MEMINFO: u8 = 10;
    }

    /// Codes within subclass `CALLSTACK` (PERF_CS_*).
    pub mod cs {
        pub const KSAMPLE: u16 = 0;
        pub const UPEND: u16 = 1;
        pub const USAMPLE: u16 = 2;
        /// Kernel stack data record. arg1..arg4 carry up to 4 frames.
        pub const KDATA: u16 = 3;
        /// User stack data record. arg1..arg4 carry up to 4 frames.
        pub const UDATA: u16 = 4;
        /// Kernel stack header. arg1=flags, arg2=nframes-async,
        /// arg3=async_index, arg4=async_nframes.
        pub const KHDR: u16 = 5;
        /// User stack header. Same arg layout as KHDR.
        pub const UHDR: u16 = 6;
        pub const ERROR: u16 = 7;
        pub const BACKTRACE: u16 = 8;
    }

    /// Codes within subclass `THREADINFO` (PERF_TI_*).
    pub mod ti {
        pub const SAMPLE: u16 = 0;
        pub const DATA: u16 = 1;
        pub const SCHEDSAMPLE: u16 = 6;
        pub const SCHEDDATA: u16 = 7;
        pub const SNAPSAMPLE: u16 = 8;
        pub const SNAPDATA: u16 = 9;
        pub const INSCYCDATA: u16 = 17;
    }

    /// Codes within subclass `PET` (PERF_PET_*).
    pub mod pet {
        pub const THREAD: u16 = 0;
        pub const ERROR: u16 = 1;
        pub const RUN: u16 = 2;
        pub const PAUSE: u16 = 3;
        pub const IDLE: u16 = 4;
        pub const SAMPLE: u16 = 5;
        pub const SCHED: u16 = 6;
        pub const END: u16 = 7;
        pub const SAMPLE_TASK: u16 = 8;
        /// `FUNC_START` brackets one thread's PET sample;
        /// `FUNC_END` closes it.
        pub const SAMPLE_THREAD: u16 = 9;
    }

    /// Codes within subclass `KPC` (PERF_KPC_*).
    pub mod kpc {
        pub const HNDLR: u16 = 0;
        pub const FCOUNTER: u16 = 1;
        pub const COUNTER: u16 = 2;
        pub const DATA: u16 = 3;
        pub const CONFIG: u16 = 4;
        pub const CFG_REG: u16 = 5;
        pub const DATA32: u16 = 6;
        pub const CFG_REG32: u16 = 7;
        /// arg1..arg4 carry up to 4 thread counter values; multiple
        /// records per sample if more than 4 counters are configured.
        pub const DATA_THREAD: u16 = 8;
        pub const DATA_THREAD32: u16 = 9;
    }
}

// ---------------------------------------------------------------------------
// Records and userland-visible structs
// ---------------------------------------------------------------------------

/// One kdebug record. Layout matches xnu's `kd_buf` on LP64.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KdBuf {
    pub timestamp: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    /// Holds the current thread id in samples emitted by kperf.
    pub arg5: u64,
    pub debugid: u32,
    pub cpuid: u32,
    pub unused: u64,
}

const _: () = assert!(mem::size_of::<KdBuf>() == 64);

/// `kd_regtype` — used for set-region / value-check filters.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KdRegtype {
    pub ty: u32,
    pub value1: u32,
    pub value2: u32,
    pub value3: u32,
    pub value4: u32,
}

/// Output of `KERN_KDGETBUF`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KbufInfo {
    pub nkdbufs: c_int,
    pub nolog: c_int,
    pub flags: c_int,
    pub nkdthreads: c_int,
    pub bufid: c_int,
}

// ---------------------------------------------------------------------------
// Operation wrappers
// ---------------------------------------------------------------------------

/// Tear down current trace buffer and config.
pub fn reset() -> Result<(), Error> {
    sysctl_op("KERN_KDREMOVE", &mut [CTL_KERN, KERN_KDEBUG, KERN_KDREMOVE])
}

/// Allocate the kernel-side ringbuffer. `nbufs` is in records, not bytes.
pub fn set_buf_size(nbufs: c_int) -> Result<(), Error> {
    sysctl_op(
        "KERN_KDSETBUF",
        &mut [CTL_KERN, KERN_KDEBUG, KERN_KDSETBUF, nbufs],
    )
}

/// Commit pending config (buffer size, filters) so subsequent
/// reads/enables operate on the live ring.
pub fn setup() -> Result<(), Error> {
    sysctl_op("KERN_KDSETUP", &mut [CTL_KERN, KERN_KDEBUG, KERN_KDSETUP])
}

/// Install a `kd_regtype` filter via `KERN_KDSETREG`.
pub fn set_filter(filter: &mut KdRegtype) -> Result<(), Error> {
    let mut mib = [CTL_KERN, KERN_KDEBUG, KERN_KDSETREG];
    let mut size = mem::size_of::<KdRegtype>();
    let rc = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            filter as *mut KdRegtype as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(Error::Sysctl {
            op: "KERN_KDSETREG",
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Start (`enable=true`) or stop tracing.
pub fn enable(enable: bool) -> Result<(), Error> {
    let v = if enable { 1 } else { 0 };
    sysctl_op(
        "KERN_KDENABLE",
        &mut [CTL_KERN, KERN_KDEBUG, KERN_KDENABLE, v],
    )
}

/// Read the kbufinfo struct (capacity, flags, thread-map size).
pub fn get_buf_info() -> Result<KbufInfo, Error> {
    let mut mib = [CTL_KERN, KERN_KDEBUG, KERN_KDGETBUF];
    let mut info = KbufInfo::default();
    let mut size = mem::size_of::<KbufInfo>();
    let rc = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            &mut info as *mut KbufInfo as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(Error::Sysctl {
            op: "KERN_KDGETBUF",
            source: io::Error::last_os_error(),
        });
    }
    Ok(info)
}

/// Drain up to `buf.len()` records from the kernel ringbuffer.
/// Returns the number of records actually written.
pub fn read_trace(buf: &mut [KdBuf]) -> Result<usize, Error> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut mib = [CTL_KERN, KERN_KDEBUG, KERN_KDREADTR];
    // The kernel uses `*size` on input as the count of records the
    // caller can accept and rewrites it to the number returned.
    let mut size = buf.len();
    let rc = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(Error::Sysctl {
            op: "KERN_KDREADTR",
            source: io::Error::last_os_error(),
        });
    }
    Ok(size)
}

/// Toggle kperf's lightweight-PET mode. Lightweight PET drops some
/// of the per-sample bookkeeping the heavier mode does and is what
/// mperf uses for stat-style measurement.
pub fn set_lightweight_pet(enabled: u32) -> Result<(), Error> {
    let mut value = enabled;
    let rc = unsafe {
        sysctlbyname(
            b"kperf.lightweight_pet\0".as_ptr() as *const _,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut c_void,
            mem::size_of::<u32>(),
        )
    };
    if rc < 0 {
        return Err(Error::Sysctl {
            op: "kperf.lightweight_pet",
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn sysctl_op(op: &'static str, mib: &mut [c_int]) -> Result<(), Error> {
    // The kdebug sysctl handler expects a non-null `oldlenp` even
    // for write-only ops; passing NULL there fails with EINVAL on
    // recent macOS releases. trace(1) and kdv both pass a
    // pointer-to-zero, so we mirror that.
    let mut zero: usize = 0;
    let rc = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut zero,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(Error::Sysctl {
            op,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}
