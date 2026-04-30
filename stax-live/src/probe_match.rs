pub(crate) const PROBE_PAIR_WINDOW_NS: u64 = 600_000;

#[cfg(target_os = "macos")]
pub(crate) fn mach_timebase_numer_denom() -> (u32, u32) {
    use std::sync::OnceLock;
    static TB: OnceLock<(u32, u32)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `mach_timebase_info` writes two u32 fields into our stack local.
        let _ = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        if info.denom == 0 {
            (1, 1)
        } else {
            (info.numer, info.denom)
        }
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn mach_timebase_numer_denom() -> (u32, u32) {
    (1, 1)
}

pub(crate) fn ticks_to_ns(ticks: i128) -> i128 {
    let (numer, denom) = mach_timebase_numer_denom();
    if denom == 0 {
        ticks
    } else {
        ticks * (numer as i128) / (denom as i128)
    }
}

pub(crate) fn elapsed_ticks_to_ns(later: u64, earlier: u64) -> u64 {
    if later < earlier {
        return 0;
    }
    ticks_to_ns((later - earlier) as i128)
        .max(0)
        .min(u64::MAX as i128) as u64
}

pub(crate) fn elapsed_ticks_to_ns_if_set(later: u64, earlier: u64) -> u64 {
    if later == 0 || earlier == 0 {
        0
    } else {
        elapsed_ticks_to_ns(later, earlier)
    }
}

pub(crate) fn abs_tick_delta_ns(a: u64, b: u64) -> u64 {
    ticks_to_ns((a as i128) - (b as i128))
        .unsigned_abs()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) fn longest_common_run(a: &[u64], b: &[u64]) -> usize {
    let mut best = 0usize;
    for i in 0..a.len() {
        for j in 0..b.len() {
            let mut k = 0usize;
            while i + k < a.len() && j + k < b.len() && a[i + k] == b[j + k] {
                k += 1;
            }
            best = best.max(k);
        }
    }
    best
}

pub(crate) fn logical_probe_stack(pc: u64, lr: u64, walked: &[u64]) -> Vec<u64> {
    let mut stack = Vec::with_capacity(1 + usize::from(lr != 0) + walked.len());
    stack.push(pc);
    if lr != 0 && lr != pc && walked.first().copied() != Some(lr) {
        stack.push(lr);
    }
    stack.extend_from_slice(walked);
    stack
}
