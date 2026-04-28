//! Runtime-loaded bindings for Apple's private `kperf` and
//! `kperfdata` frameworks.
//!
//! Both ship in `/System/Library/PrivateFrameworks/` and have stable
//! public symbols across M1-M4 (and Intel) per mperf's experience.
//! Loading via `dlopen` rather than linking avoids any build-time
//! dependency on private headers.
//!
//! Symbol set is taken verbatim from mperf
//! (<https://github.com/tmcgilchrist/mperf>, MIT) which derived it
//! from ibireme's kpc_demo.c (public domain).

use std::ffi::{c_char, c_int, c_void};

use crate::error::Error;

const KPERF_PATH: &[u8] = b"/System/Library/PrivateFrameworks/kperf.framework/kperf\0";
const KPERFDATA_PATH: &[u8] = b"/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata\0";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const KPC_CLASS_FIXED: u32 = 0;
pub const KPC_CLASS_CONFIGURABLE: u32 = 1;
pub const KPC_CLASS_FIXED_MASK: u32 = 1 << KPC_CLASS_FIXED;
pub const KPC_CLASS_CONFIGURABLE_MASK: u32 = 1 << KPC_CLASS_CONFIGURABLE;
pub const KPC_MAX_COUNTERS: usize = 32;

/// Bits accepted by `kperf_action_samplers_set`.
pub mod sampler {
    pub const TH_INFO: u32 = 1 << 0;
    pub const TH_SNAPSHOT: u32 = 1 << 1;
    pub const KSTACK: u32 = 1 << 2;
    pub const USTACK: u32 = 1 << 3;
    pub const PMC_THREAD: u32 = 1 << 4;
    pub const PMC_CPU: u32 = 1 << 5;
    pub const PMC_CONFIG: u32 = 1 << 6;
    pub const MEMINFO: u32 = 1 << 7;
    pub const TH_SCHEDULING: u32 = 1 << 8;
    pub const TH_DISPATCH: u32 = 1 << 9;
    pub const TK_SNAPSHOT: u32 = 1 << 10;
    pub const SYS_MEM: u32 = 1 << 11;
    pub const TH_INSCYC: u32 = 1 << 12;
    pub const TK_INFO: u32 = 1 << 13;
}

pub const KPERF_ACTION_MAX: u32 = 32;
pub const KPERF_TIMER_MAX: u32 = 8;

pub type KpcConfig = u64;

// ---------------------------------------------------------------------------
// kperfdata opaque types
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct KpepDb {
    _private: [u8; 0],
}

#[repr(C)]
pub struct KpepConfig {
    _private: [u8; 0],
}

/// Public layout of `kpep_event` (per mperf / kpc_demo).
#[repr(C)]
pub struct KpepEvent {
    pub name: *const c_char,
    pub description: *const c_char,
    pub errata: *const c_char,
    pub alias: *const c_char,
    pub fallback: *const c_char,
    pub mask: u32,
    pub number: u8,
    pub umask: u8,
    pub reserved: u8,
    pub is_fixed: u8,
}

// ---------------------------------------------------------------------------
// Function pointer table
// ---------------------------------------------------------------------------

/// Resolved function pointers from both private frameworks. Loaded
/// once and cached.
#[allow(non_snake_case)]
pub struct Frameworks {
    // kpc (in kperf.framework)
    pub kpc_force_all_ctrs_get: unsafe extern "C" fn(*mut c_int) -> c_int,
    pub kpc_force_all_ctrs_set: unsafe extern "C" fn(c_int) -> c_int,
    pub kpc_get_counter_count: unsafe extern "C" fn(u32) -> u32,
    pub kpc_get_config_count: unsafe extern "C" fn(u32) -> u32,
    pub kpc_set_config: unsafe extern "C" fn(u32, *mut KpcConfig) -> c_int,
    pub kpc_get_thread_counters: unsafe extern "C" fn(u32, u32, *mut u64) -> c_int,
    pub kpc_set_counting: unsafe extern "C" fn(u32) -> c_int,
    pub kpc_set_thread_counting: unsafe extern "C" fn(u32) -> c_int,

    // kperf (PET / sampling control)
    pub kperf_action_count_set: unsafe extern "C" fn(u32) -> c_int,
    pub kperf_action_count_get: unsafe extern "C" fn(*mut u32) -> c_int,
    pub kperf_action_samplers_set: unsafe extern "C" fn(u32, u32) -> c_int,
    pub kperf_action_samplers_get: unsafe extern "C" fn(u32, *mut u32) -> c_int,
    pub kperf_action_filter_set_by_pid: unsafe extern "C" fn(u32, i32) -> c_int,
    pub kperf_timer_count_set: unsafe extern "C" fn(u32) -> c_int,
    pub kperf_timer_count_get: unsafe extern "C" fn(*mut u32) -> c_int,
    pub kperf_timer_period_set: unsafe extern "C" fn(u32, u64) -> c_int,
    pub kperf_timer_period_get: unsafe extern "C" fn(u32, *mut u64) -> c_int,
    pub kperf_timer_action_set: unsafe extern "C" fn(u32, u32) -> c_int,
    pub kperf_timer_action_get: unsafe extern "C" fn(u32, *mut u32) -> c_int,
    pub kperf_timer_pet_set: unsafe extern "C" fn(u32) -> c_int,
    pub kperf_timer_pet_get: unsafe extern "C" fn(*mut u32) -> c_int,
    pub kperf_sample_set: unsafe extern "C" fn(u32) -> c_int,
    pub kperf_sample_get: unsafe extern "C" fn(*mut u32) -> c_int,
    pub kperf_reset: unsafe extern "C" fn() -> c_int,
    pub kperf_ns_to_ticks: unsafe extern "C" fn(u64) -> u64,
    pub kperf_ticks_to_ns: unsafe extern "C" fn(u64) -> u64,
    pub kperf_tick_frequency: unsafe extern "C" fn() -> u64,

    // kperfdata (event database + config builder)
    pub kpep_db_create: unsafe extern "C" fn(*const c_char, *mut *mut KpepDb) -> c_int,
    pub kpep_db_free: unsafe extern "C" fn(*mut KpepDb),
    pub kpep_db_name: unsafe extern "C" fn(*mut KpepDb, *mut *const c_char) -> c_int,
    pub kpep_db_event:
        unsafe extern "C" fn(*mut KpepDb, *const c_char, *mut *mut KpepEvent) -> c_int,
    pub kpep_db_events_count: unsafe extern "C" fn(*mut KpepDb, *mut usize) -> c_int,
    pub kpep_db_events: unsafe extern "C" fn(*mut KpepDb, *mut *mut KpepEvent, usize) -> c_int,
    pub kpep_config_create: unsafe extern "C" fn(*mut KpepDb, *mut *mut KpepConfig) -> c_int,
    pub kpep_config_free: unsafe extern "C" fn(*mut KpepConfig),
    pub kpep_config_add_event:
        unsafe extern "C" fn(*mut KpepConfig, *mut *mut KpepEvent, u32, *mut u32) -> c_int,
    pub kpep_config_force_counters: unsafe extern "C" fn(*mut KpepConfig) -> c_int,
    pub kpep_config_kpc_classes: unsafe extern "C" fn(*mut KpepConfig, *mut u32) -> c_int,
    pub kpep_config_kpc_count: unsafe extern "C" fn(*mut KpepConfig, *mut usize) -> c_int,
    pub kpep_config_kpc_map: unsafe extern "C" fn(*mut KpepConfig, *mut usize, usize) -> c_int,
    pub kpep_config_kpc: unsafe extern "C" fn(*mut KpepConfig, *mut KpcConfig, usize) -> c_int,
}

/// Load both frameworks and resolve every symbol we need. dlopen
/// keeps a refcount, so calling this repeatedly is cheap.
pub fn load() -> Result<Frameworks, Error> {
    let kperf = unsafe { dlopen_or_err(KPERF_PATH)? };
    let kperfdata = unsafe { dlopen_or_err(KPERFDATA_PATH)? };

    macro_rules! sym {
        ($lib:expr, $name:ident) => {
            unsafe { dlsym_or_err($lib, concat!(stringify!($name), "\0"))? }
        };
    }

    Ok(Frameworks {
        kpc_force_all_ctrs_get: sym!(kperf, kpc_force_all_ctrs_get),
        kpc_force_all_ctrs_set: sym!(kperf, kpc_force_all_ctrs_set),
        kpc_get_counter_count: sym!(kperf, kpc_get_counter_count),
        kpc_get_config_count: sym!(kperf, kpc_get_config_count),
        kpc_set_config: sym!(kperf, kpc_set_config),
        kpc_get_thread_counters: sym!(kperf, kpc_get_thread_counters),
        kpc_set_counting: sym!(kperf, kpc_set_counting),
        kpc_set_thread_counting: sym!(kperf, kpc_set_thread_counting),

        kperf_action_count_set: sym!(kperf, kperf_action_count_set),
        kperf_action_count_get: sym!(kperf, kperf_action_count_get),
        kperf_action_samplers_set: sym!(kperf, kperf_action_samplers_set),
        kperf_action_samplers_get: sym!(kperf, kperf_action_samplers_get),
        kperf_action_filter_set_by_pid: sym!(kperf, kperf_action_filter_set_by_pid),
        kperf_timer_count_set: sym!(kperf, kperf_timer_count_set),
        kperf_timer_count_get: sym!(kperf, kperf_timer_count_get),
        kperf_timer_period_set: sym!(kperf, kperf_timer_period_set),
        kperf_timer_period_get: sym!(kperf, kperf_timer_period_get),
        kperf_timer_action_set: sym!(kperf, kperf_timer_action_set),
        kperf_timer_action_get: sym!(kperf, kperf_timer_action_get),
        kperf_timer_pet_set: sym!(kperf, kperf_timer_pet_set),
        kperf_timer_pet_get: sym!(kperf, kperf_timer_pet_get),
        kperf_sample_set: sym!(kperf, kperf_sample_set),
        kperf_sample_get: sym!(kperf, kperf_sample_get),
        kperf_reset: sym!(kperf, kperf_reset),
        kperf_ns_to_ticks: sym!(kperf, kperf_ns_to_ticks),
        kperf_ticks_to_ns: sym!(kperf, kperf_ticks_to_ns),
        kperf_tick_frequency: sym!(kperf, kperf_tick_frequency),

        kpep_db_create: sym!(kperfdata, kpep_db_create),
        kpep_db_free: sym!(kperfdata, kpep_db_free),
        kpep_db_name: sym!(kperfdata, kpep_db_name),
        kpep_db_event: sym!(kperfdata, kpep_db_event),
        kpep_db_events_count: sym!(kperfdata, kpep_db_events_count),
        kpep_db_events: sym!(kperfdata, kpep_db_events),
        kpep_config_create: sym!(kperfdata, kpep_config_create),
        kpep_config_free: sym!(kperfdata, kpep_config_free),
        kpep_config_add_event: sym!(kperfdata, kpep_config_add_event),
        kpep_config_force_counters: sym!(kperfdata, kpep_config_force_counters),
        kpep_config_kpc_classes: sym!(kperfdata, kpep_config_kpc_classes),
        kpep_config_kpc_count: sym!(kperfdata, kpep_config_kpc_count),
        kpep_config_kpc_map: sym!(kperfdata, kpep_config_kpc_map),
        kpep_config_kpc: sym!(kperfdata, kpep_config_kpc),
    })
}

unsafe fn dlopen_or_err(path: &[u8]) -> Result<*mut c_void, Error> {
    let handle = libc::dlopen(path.as_ptr() as *const c_char, libc::RTLD_LAZY);
    if handle.is_null() {
        let msg = dlerror_string();
        return Err(Error::FrameworkLoad {
            path: bytes_to_path(path),
            msg,
        });
    }
    Ok(handle)
}

unsafe fn dlsym_or_err<T>(handle: *mut c_void, name: &str) -> Result<T, Error> {
    debug_assert!(name.ends_with('\0'));
    let sym = libc::dlsym(handle, name.as_ptr() as *const c_char);
    if sym.is_null() {
        let msg = dlerror_string();
        return Err(Error::SymbolMissing {
            name: name.trim_end_matches('\0').to_string(),
            msg,
        });
    }
    // Function pointers are the same size as `*mut c_void` on every
    // platform we run on; transmute_copy avoids the awkward cast.
    Ok(std::mem::transmute_copy::<*mut c_void, T>(&sym))
}

unsafe fn dlerror_string() -> String {
    let p = libc::dlerror();
    if p.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

fn bytes_to_path(bytes: &[u8]) -> String {
    let trimmed = bytes.split(|&b| b == 0).next().unwrap_or(bytes);
    String::from_utf8_lossy(trimmed).into_owned()
}
