//! Configurable PMU event setup via the private `kperfdata`
//! framework's `kpep_db` event database.
//!
//! Apple Silicon CPUs ship with two fixed counters (cycles +
//! instructions retired) and ~8 configurable counters that can be
//! programmed for any event the chip supports (cache misses, branch
//! mispredicts, TLB misses, frontend stalls, ...). The kpep_db
//! exposes the per-chip event catalog by name; we look up a small
//! curated set, build a `kpep_config`, and hand the encoded `u64`
//! configs to `kpc_set_config`. After that, the same `PMC_THREAD`
//! sampler that already drives our cycles/instructions reads also
//! reads the configurable counters at every PET tick, and they flow
//! through the existing parser as additional values in
//! `PERF_KPC_DATA_THREAD` records.
//!
//! The event names below are the Apple-published "alias" form
//! (`L1D_CACHE_MISS_LD`, `BRANCH_MISPRED_NONSPEC`, ...) which the
//! kpep_db translates into per-chip encodings. If a name is missing
//! on the host (e.g. an older or much newer chip), the lookup just
//! fails and we proceed with whatever did resolve.

use std::ffi::CString;
use std::ptr;

use crate::bindings::{Frameworks, KpcConfig, KpepConfig, KpepDb, KpepEvent, KPC_MAX_COUNTERS};

/// One configurable event we'd like to sample. The order in this
/// list is also the order in which the values appear in the
/// per-sample `pmc` slice after the FIXED counters (cycles,
/// instructions). Pinned + curated for the demo; later we can plumb
/// CLI flags / live UI selection to make it configurable.
pub const REQUESTED_EVENTS: &[PmuEvent] = &[
    PmuEvent {
        slot: PmuSlot::L1DCacheMissLoad,
        // Try several alias names; kpep_db's event table uses
        // different spellings across chip families.
        candidates: &[
            "L1D_CACHE_MISS_LD",
            "L1D_CACHE_MISS_LD_NONSPEC",
            "L1D_LOAD_MISS",
            "MEM_INST_RETIRED.L1_MISS_LOAD",
        ],
    },
    PmuEvent {
        slot: PmuSlot::BranchMispredict,
        candidates: &[
            "BRANCH_MISPRED_NONSPEC",
            "BRANCH_MISPREDICT",
            "BR_MIS_PRED",
            "BR_MIS_PRED_RETIRED",
        ],
    },
];

#[derive(Clone, Copy, Debug)]
pub enum PmuSlot {
    L1DCacheMissLoad,
    BranchMispredict,
}

pub struct PmuEvent {
    pub slot: PmuSlot,
    pub candidates: &'static [&'static str],
}

/// Result of configuring the PMU. `configs` is the array of u64
/// counter configs to pass to `kpc_set_config`; `class_mask` is the
/// bitwise-OR of `KPC_CLASS_*_MASK` for the counter classes we
/// enabled. `slot_to_pmc_index` maps each `PmuSlot` we successfully
/// resolved to its index in the per-sample `pmc` slice (counting
/// from 0 = first FIXED counter).
pub struct ConfiguredPmu {
    pub configs: Vec<KpcConfig>,
    pub class_mask: u32,
    /// Number of counters in the FIXED class (offset from which the
    /// configurable counters start in the sample's pmc slice).
    pub fixed_count: usize,
    /// `slot_to_pmc_index[i]` = index in the sample's `pmc` slice
    /// where this slot's counter value lives. `None` if the event
    /// failed to resolve on this chip.
    pub slot_indices: [Option<usize>; PMU_SLOTS],
}

pub const PMU_SLOTS: usize = 2;

/// Initialise the PMU for the recording. Returns `None` (with a
/// log line) if the kpep_db can't be opened, or all configurable
/// events failed to resolve. In that case the caller should still
/// proceed with the FIXED class only.
pub fn configure(fw: &Frameworks) -> Option<ConfiguredPmu> {
    use crate::bindings::{
        KPC_CLASS_CONFIGURABLE, KPC_CLASS_CONFIGURABLE_MASK, KPC_CLASS_FIXED_MASK,
    };

    let mut db: *mut KpepDb = ptr::null_mut();
    let rc = unsafe { (fw.kpep_db_create)(ptr::null(), &mut db) };
    if rc != 0 || db.is_null() {
        log::warn!("kpep_db_create failed (rc={rc}); configurable PMU events disabled");
        return None;
    }
    let _db_guard = scopeguard(|| unsafe { (fw.kpep_db_free)(db) });

    let mut config: *mut KpepConfig = ptr::null_mut();
    let rc = unsafe { (fw.kpep_config_create)(db, &mut config) };
    if rc != 0 || config.is_null() {
        log::warn!("kpep_config_create failed (rc={rc}); configurable PMU events disabled");
        return None;
    }
    let _config_guard = scopeguard(|| unsafe { (fw.kpep_config_free)(config) });

    let mut slot_indices: [Option<usize>; PMU_SLOTS] = [None; PMU_SLOTS];
    let mut resolved_any = false;
    let fixed_count = unsafe { (fw.kpc_get_counter_count)(KPC_CLASS_FIXED_MASK) } as usize;
    let mut pmc_index_after_fixed = fixed_count;

    for entry in REQUESTED_EVENTS {
        let slot_idx = entry.slot as usize;
        let mut event_ptr: *mut KpepEvent = ptr::null_mut();
        let mut found_name: Option<&str> = None;
        for &name in entry.candidates {
            let cname = match CString::new(name) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rc = unsafe { (fw.kpep_db_event)(db, cname.as_ptr(), &mut event_ptr) };
            if rc == 0 && !event_ptr.is_null() {
                found_name = Some(name);
                break;
            }
        }
        let Some(name) = found_name else {
            log::info!(
                "pmu: event slot {:?} unavailable (none of {:?} resolved on this chip)",
                entry.slot,
                entry.candidates,
            );
            continue;
        };

        let mut idx_used: u32 = 0;
        let rc = unsafe { (fw.kpep_config_add_event)(config, &mut event_ptr, 0, &mut idx_used) };
        if rc != 0 {
            log::info!("pmu: kpep_config_add_event failed for {name} (rc={rc}); skipping");
            continue;
        }
        slot_indices[slot_idx] = Some(pmc_index_after_fixed);
        pmc_index_after_fixed += 1;
        resolved_any = true;
        log::info!(
            "pmu: event slot {:?} = {name} (counter index {})",
            entry.slot,
            pmc_index_after_fixed - 1
        );
    }

    if !resolved_any {
        log::warn!("pmu: no configurable events resolved; falling back to FIXED only");
        return None;
    }

    // Force the kpep config to actually allocate counters.
    let rc = unsafe { (fw.kpep_config_force_counters)(config) };
    if rc != 0 {
        log::warn!("kpep_config_force_counters failed (rc={rc}); configurable events disabled");
        return None;
    }

    // Translate the kpep config into the raw u64 configs that
    // `kpc_set_config` consumes. `kpep_config_kpc_count` tells us
    // how many counter slots the config occupies; `kpep_config_kpc`
    // fills our buffer.
    let mut count: usize = 0;
    let rc = unsafe { (fw.kpep_config_kpc_count)(config, &mut count) };
    if rc != 0 {
        log::warn!("kpep_config_kpc_count failed (rc={rc})");
        return None;
    }
    let mut configs: Vec<KpcConfig> = vec![0u64; KPC_MAX_COUNTERS];
    // kpep_config_kpc's third argument is the buffer size in BYTES,
    // not the number of entries. Passing the entry count makes kpep
    // think the buffer is too small and bail with rc=4.
    let buf_bytes = configs.len() * std::mem::size_of::<KpcConfig>();
    let rc = unsafe { (fw.kpep_config_kpc)(config, configs.as_mut_ptr(), buf_bytes) };
    if rc != 0 {
        log::warn!("kpep_config_kpc failed (rc={rc})");
        return None;
    }
    configs.truncate(count.min(KPC_MAX_COUNTERS));

    let mut class_mask = KPC_CLASS_FIXED_MASK;
    let mut config_classes: u32 = 0;
    let rc = unsafe { (fw.kpep_config_kpc_classes)(config, &mut config_classes) };
    if rc == 0 {
        class_mask |= config_classes;
    } else {
        // Fall back to enabling configurable class wholesale.
        class_mask |= KPC_CLASS_CONFIGURABLE_MASK;
    }
    let _ = KPC_CLASS_CONFIGURABLE; // keep reference for clarity

    Some(ConfiguredPmu {
        configs,
        class_mask,
        fixed_count,
        slot_indices,
    })
}

/// Trivial scope-exit helper so the kpep db/config pointers get
/// released on any return path without pulling in `scopeguard`.
fn scopeguard<F: FnOnce()>(f: F) -> ScopeGuard<F> {
    ScopeGuard(Some(f))
}

struct ScopeGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for ScopeGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}
