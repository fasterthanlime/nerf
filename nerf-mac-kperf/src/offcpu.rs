//! Off-CPU tracking via `DBG_MACH_SCHED` kdebug records.
//!
//! Each context switch fires one of:
//!   * `MACH_SCHED` (subclass=0x40, code=0x0) -- a thread-thread switch.
//!     `arg2` holds the on-coming thread's tid.
//!   * `MACH_STKHANDOFF` (subclass=0x40, code=0x8) -- the same thing
//!     but on the fast path used by `thread_handoff`.
//!   * `MACH_MAKERUNNABLE` (subclass=0x40, code=0x4) -- a blocked
//!     thread became runnable. `arg1` holds the woken-up tid.
//!
//! For each (off, on) tid pair we know:
//!   - who came on-CPU at this timestamp (record's `arg2`)
//!   - who went off-CPU is the *previous* on-going thread on the
//!     same cpuid; we track the per-cpu current tid for that.
//!
//! Off-CPU stacks are borrowed from the most recent PET sample for
//! the thread. kperf doesn't sample on context switches (we'd need
//! a kperf-on-kdbg trigger action for that, which is a TODO), so
//! the "where" of an off-CPU interval is approximated by "where the
//! thread was at the previous on-CPU sample." Good enough to show
//! e.g. an audio thread blocked in `mach_msg_overwrite_trap` --
//! that frame survives across the deschedule and is on the stack
//! at every PET tick leading up to it.

use std::collections::HashMap;

use crate::kdebug::{kdbg_code, kdbg_subclass, mach_sched, KdBuf, KDBG_TIMESTAMP_MASK};

#[derive(Default)]
pub struct OffCpuTracker {
    /// Last-known on-CPU thread per cpuid. Mapping cpuid -> tid.
    on_cpu: HashMap<u32, u64>,
    /// Per-thread state: cached on-CPU stack + accumulated off-CPU
    /// duration + timestamp of the most recent off->on transition we
    /// still need to close (None once the thread is on-CPU).
    threads: HashMap<u64, ThreadState>,
    sched_count: u64,
    stkhandoff_count: u64,
    makerunnable_count: u64,
    /// Closed off-CPU intervals waiting to be expanded into samples
    /// by the drain loop.
    pending: Vec<OffCpuInterval>,
}

#[derive(Default)]
struct ThreadState {
    total_off_ns: u64,
    last_off_ns: Option<u64>,
    /// Last on-CPU stack we sampled for this thread; used as the
    /// stack for synthetic off-CPU samples until the thread runs
    /// again and gets a fresh sample.
    last_user_stack: Vec<u64>,
    last_kernel_stack: Vec<u64>,
}

/// One closed off-CPU interval, ready to be expanded into N
/// synthetic samples at the kperf sampling period.
pub struct OffCpuInterval {
    pub tid: u32,
    pub off_ns: u64,
    pub on_ns: u64,
    pub user_stack: Vec<u64>,
    pub kernel_stack: Vec<u64>,
}

impl OffCpuTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cache the most recent on-CPU stack for a thread. Called on
    /// every kperf PET sample. The cached stack is what we attribute
    /// any subsequent off-CPU interval to.
    pub fn note_sample(&mut self, tid: u32, user: &[u64], kernel: &[u64]) {
        let st = self.threads.entry(tid as u64).or_default();
        st.last_user_stack.clear();
        st.last_user_stack.extend_from_slice(user);
        st.last_kernel_stack.clear();
        st.last_kernel_stack.extend_from_slice(kernel);
    }

    pub fn feed(&mut self, rec: &KdBuf) {
        let subclass = kdbg_subclass(rec.debugid);
        if subclass != crate::kdebug::DBG_MACH_SCHED {
            return;
        }
        let code = kdbg_code(rec.debugid);
        let ts = rec.timestamp & KDBG_TIMESTAMP_MASK;
        match code {
            mach_sched::SCHED | mach_sched::STKHANDOFF => {
                if code == mach_sched::SCHED {
                    self.sched_count += 1;
                } else {
                    self.stkhandoff_count += 1;
                }
                let new_tid = rec.arg2;
                let cpu = rec.cpuid;

                // Whoever was on this cpu before is now off.
                if let Some(prev_tid) = self.on_cpu.insert(cpu, new_tid) {
                    if prev_tid != 0 && prev_tid != new_tid {
                        let st = self.threads.entry(prev_tid).or_default();
                        st.last_off_ns = Some(ts);
                    }
                }

                // The new on-coming thread closes its off-CPU
                // interval (if any). Push to pending so the drain
                // loop can synthesize wall-clock samples covering the
                // gap.
                if new_tid != 0 {
                    let st = self.threads.entry(new_tid).or_default();
                    if let Some(off_ns) = st.last_off_ns.take() {
                        let interval_ns = ts.saturating_sub(off_ns);
                        st.total_off_ns = st.total_off_ns.saturating_add(interval_ns);
                        // Only emit synthetic samples if we have a
                        // stack to attribute them to. Without a prior
                        // on-CPU sample for this thread we'd just be
                        // emitting empty stacks.
                        if !st.last_user_stack.is_empty() || !st.last_kernel_stack.is_empty() {
                            self.pending.push(OffCpuInterval {
                                tid: new_tid as u32,
                                off_ns,
                                on_ns: ts,
                                user_stack: st.last_user_stack.clone(),
                                kernel_stack: st.last_kernel_stack.clone(),
                            });
                        }
                    }
                }
            }
            mach_sched::MAKERUNNABLE => {
                self.makerunnable_count += 1;
            }
            _ => {}
        }
    }

    /// Take any closed off-CPU intervals that haven't been expanded
    /// into samples yet. Caller is expected to drain into a sample
    /// sink at the kperf sampling cadence.
    pub fn drain_pending(&mut self) -> Vec<OffCpuInterval> {
        std::mem::take(&mut self.pending)
    }

    pub fn log_summary(&self) {
        log::info!(
            "off-cpu: SCHED={} STKHANDOFF={} MAKERUNNABLE={} threads_seen={}",
            self.sched_count,
            self.stkhandoff_count,
            self.makerunnable_count,
            self.threads.len()
        );
        // Top-10 threads by total off-CPU time.
        let mut by_off: Vec<(u64, u64)> =
            self.threads.iter().map(|(&t, st)| (t, st.total_off_ns)).collect();
        by_off.sort_by(|a, b| b.1.cmp(&a.1));
        for (tid, total_off_ns) in by_off.iter().take(10) {
            let total_off_ms = (*total_off_ns as f64) / 1_000_000.0;
            log::info!("  tid={tid} off-cpu={total_off_ms:.2}ms");
        }
    }
}
