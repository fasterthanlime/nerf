//! CPU interval tracker driven by `DBG_MACH_SCHED` kdebug records.
//!
//! Every context switch on the system fires one of:
//!   * `MACH_SCHED` (subclass=0x40, code=0x0) -- a thread-thread switch.
//!     `arg2` holds the on-coming thread's tid.
//!   * `MACH_STKHANDOFF` (subclass=0x40, code=0x8) -- the same thing
//!     but on the fast path used by `thread_handoff`.
//!   * `MACH_MAKERUNNABLE` (subclass=0x40, code=0x4) -- a blocked
//!     thread became runnable. `arg2` holds the woken-up tid.
//!
//! We track the per-CPU current tid, and on every transition close
//! the *outgoing* thread's on-CPU interval and (if we'd opened one
//! before) the *incoming* thread's off-CPU interval. Both kinds get
//! drained out of `drain_pending` so the recorder can forward them
//! to its sink as ground-truth duration intervals.
//!
//! Off-CPU stacks are borrowed from the most recent PET sample for
//! the thread. kperf doesn't sample on context switches (we'd need a
//! kperf-on-kdbg trigger action for that, which is a TODO), so the
//! "where" of an off-CPU interval is approximated by "where the
//! thread was at the previous on-CPU sample." Good enough to show
//! e.g. an audio thread blocked in `mach_msg_overwrite_trap` -- that
//! frame survives across the deschedule and is on the stack at every
//! PET tick leading up to it.

use std::collections::HashMap;

use crate::kdebug::{kdbg_code, kdbg_subclass, mach_sched, KdBuf, KDBG_TIMESTAMP_MASK};

#[derive(Default)]
pub struct CpuIntervalTracker {
    /// Last-known on-CPU thread per cpuid. Mapping cpuid -> tid.
    on_cpu: HashMap<u32, u64>,
    threads: HashMap<u64, ThreadState>,
    sched_count: u64,
    stkhandoff_count: u64,
    makerunnable_count: u64,
    /// Closed CPU intervals waiting to be drained into the sink.
    /// Both on-CPU and off-CPU live here; the variant is on the
    /// item itself.
    pending: Vec<PendingInterval>,
    /// Wakeup events captured this batch. Drained by the recorder
    /// after each kdebug pull, paired with the waker's last PET
    /// stack and emitted via `SampleSink::on_wakeup`.
    pending_wakeups: Vec<PendingWakeup>,
}

#[derive(Default)]
struct ThreadState {
    total_on_ns: u64,
    total_off_ns: u64,
    /// Open on-CPU interval start, set whenever the thread comes on
    /// a CPU and cleared when it leaves. `None` while off-CPU.
    last_on_ns: Option<u64>,
    /// Open off-CPU interval start, set whenever the thread leaves a
    /// CPU and cleared when it comes back. `None` while on-CPU.
    last_off_ns: Option<u64>,
    /// Last on-CPU stack we sampled for this thread; the "where" of
    /// any subsequent off-CPU interval. Updated on every PET tick.
    last_user_stack: Vec<u64>,
    last_kernel_stack: Vec<u64>,
    /// Wakeup that brought this thread out of its current off-CPU
    /// state. Populated when MAKERUNNABLE fires with this thread as
    /// the wakee; consumed (and cleared) when the thread comes back
    /// on-CPU and we close its off-CPU interval. Lets us answer
    /// "who unblocked this thread?" per interval, not just per-tid.
    pending_wakeup: Option<PendingWakerAttribution>,
}

#[derive(Clone)]
struct PendingWakerAttribution {
    waker_tid: u32,
    waker_user_stack: Vec<u64>,
}

/// One closed CPU interval, fed to the recorder's sink.
pub struct PendingInterval {
    pub tid: u32,
    pub start_ns: u64,
    pub end_ns: u64,
    pub kind: PendingKind,
}

pub enum PendingKind {
    OnCpu,
    OffCpu {
        /// Cached user stack at the moment the thread parked.
        user_stack: Vec<u64>,
        /// Cached kernel stack (currently unused by the live sink
        /// but kept for symmetry with the suspend-and-walk path).
        kernel_stack: Vec<u64>,
        /// Who woke this thread out of the off-CPU interval. `None`
        /// when no MAKERUNNABLE record landed in the interval window
        /// (e.g. timer/IO interrupt completing the wait without an
        /// explicit wake call, or the wake event was dropped from
        /// the kdebug ring before we drained it).
        waker_tid: Option<u32>,
        /// Cached waker stack at the moment of MAKERUNNABLE, leaf-first.
        waker_user_stack: Option<Vec<u64>>,
    },
}

/// One observed wakeup. Captured at `MACH_MAKERUNNABLE` time using
/// the *waker* thread's most recent PET stack -- so we know "thread
/// X got woken at time T by thread Y, here's where Y was when it
/// did the wake-up call." Pure differentiator: samply has no kernel
/// hook capable of producing this.
pub struct PendingWakeup {
    pub timestamp_ns: u64,
    pub waker_tid: u32,
    pub wakee_tid: u32,
    pub waker_user_stack: Vec<u64>,
    pub waker_kernel_stack: Vec<u64>,
}

impl CpuIntervalTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cache the most recent on-CPU stack for a thread. Called on
    /// every kperf PET sample. The cached stack is what we attribute
    /// any subsequent off-CPU interval to.
    ///
    /// Samples taken while the thread was in-kernel typically have
    /// `kernel` populated and `user` empty. If we wrote those over
    /// the cache, every later off-CPU interval would carry an empty
    /// user stack -- even though a perfectly good user stack was
    /// sampled moments before. Skip the update when there's no user
    /// side to record.
    pub fn note_sample(&mut self, tid: u32, user: &[u64], kernel: &[u64]) {
        if user.is_empty() {
            return;
        }
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

                // Whoever was on this cpu before is now off. Close
                // their on-CPU interval (if open) and mark them as
                // entering an off-CPU interval at `ts`.
                if let Some(prev_tid) = self.on_cpu.insert(cpu, new_tid) {
                    if prev_tid != 0 && prev_tid != new_tid {
                        let st = self.threads.entry(prev_tid).or_default();
                        if let Some(on_ns) = st.last_on_ns.take() {
                            let dur = ts.saturating_sub(on_ns);
                            if dur > 0 {
                                st.total_on_ns = st.total_on_ns.saturating_add(dur);
                                self.pending.push(PendingInterval {
                                    tid: prev_tid as u32,
                                    start_ns: on_ns,
                                    end_ns: ts,
                                    kind: PendingKind::OnCpu,
                                });
                            }
                        }
                        st.last_off_ns = Some(ts);
                    }
                }

                // The new on-coming thread closes its open off-CPU
                // interval (if any) and opens an on-CPU interval.
                if new_tid != 0 {
                    let st = self.threads.entry(new_tid).or_default();
                    if let Some(off_ns) = st.last_off_ns.take() {
                        let dur = ts.saturating_sub(off_ns);
                        if dur > 0 {
                            st.total_off_ns = st.total_off_ns.saturating_add(dur);
                            // Only emit if we have a user stack to
                            // attribute the blocked time to. Without
                            // one the interval is invisible to the
                            // user-side flame; keep the timing in
                            // total_off_ns for diagnostics but skip
                            // the sink.
                            if !st.last_user_stack.is_empty() {
                                let waker = st.pending_wakeup.take();
                                let (waker_tid, waker_user_stack) = match waker {
                                    Some(w) => (Some(w.waker_tid), Some(w.waker_user_stack)),
                                    None => (None, None),
                                };
                                self.pending.push(PendingInterval {
                                    tid: new_tid as u32,
                                    start_ns: off_ns,
                                    end_ns: ts,
                                    kind: PendingKind::OffCpu {
                                        user_stack: st.last_user_stack.clone(),
                                        kernel_stack: st.last_kernel_stack.clone(),
                                        waker_tid,
                                        waker_user_stack,
                                    },
                                });
                            } else {
                                // Discard stale wakeup record so we
                                // don't carry it into the next
                                // off-CPU interval.
                                st.pending_wakeup = None;
                            }
                        }
                    }
                    st.last_on_ns = Some(ts);
                }
            }
            mach_sched::MAKERUNNABLE => {
                self.makerunnable_count += 1;
                // The cpu emitting this record is currently running
                // the waker; arg2 is the wakee's tid (matches the
                // shape of MACH_SCHED's "new tid" argument). If the
                // waker has had at least one PET tick already we can
                // attribute the wake to whichever stack we cached
                // for it.
                let waker_tid = self.on_cpu.get(&rec.cpuid).copied().unwrap_or(0);
                let wakee_tid = rec.arg2;
                if waker_tid == 0 || wakee_tid == 0 || waker_tid == wakee_tid {
                    return;
                }
                let waker_user_stack = match self.threads.get(&waker_tid) {
                    Some(s) if !s.last_user_stack.is_empty()
                        || !s.last_kernel_stack.is_empty() =>
                    {
                        s.last_user_stack.clone()
                    }
                    _ => return,
                };
                let waker_kernel_stack = self
                    .threads
                    .get(&waker_tid)
                    .map(|s| s.last_kernel_stack.clone())
                    .unwrap_or_default();
                // Two consumers of this wakeup:
                // 1) The "who woke me?" panel aggregates these per
                //    wakee tid, top-N grouped by (waker_tid, waker_leaf).
                // 2) Per off-CPU *interval* attribution: stash the
                //    waker on the wakee's ThreadState so the next
                //    SCHED-on for that thread can stamp the closing
                //    interval with `waker_tid + waker_user_stack`.
                self.pending_wakeups.push(PendingWakeup {
                    timestamp_ns: ts,
                    waker_tid: waker_tid as u32,
                    wakee_tid: wakee_tid as u32,
                    waker_user_stack: waker_user_stack.clone(),
                    waker_kernel_stack,
                });
                let wakee = self.threads.entry(wakee_tid).or_default();
                wakee.pending_wakeup = Some(PendingWakerAttribution {
                    waker_tid: waker_tid as u32,
                    waker_user_stack,
                });
            }
            _ => {}
        }
    }

    /// Take any closed CPU intervals that haven't been forwarded
    /// yet. Includes both on-CPU and off-CPU.
    pub fn drain_pending(&mut self) -> Vec<PendingInterval> {
        std::mem::take(&mut self.pending)
    }

    /// Take any wakeup events captured this batch.
    pub fn drain_wakeups(&mut self) -> Vec<PendingWakeup> {
        std::mem::take(&mut self.pending_wakeups)
    }

    pub fn log_summary(&self) {
        log::info!(
            "cpu intervals: SCHED={} STKHANDOFF={} MAKERUNNABLE={} threads_seen={}",
            self.sched_count,
            self.stkhandoff_count,
            self.makerunnable_count,
            self.threads.len()
        );
        // Top-10 threads by total off-CPU time.
        let mut by_off: Vec<(u64, u64, u64)> = self
            .threads
            .iter()
            .map(|(&t, st)| (t, st.total_on_ns, st.total_off_ns))
            .collect();
        by_off.sort_by(|a, b| b.2.cmp(&a.2));
        for (tid, total_on_ns, total_off_ns) in by_off.iter().take(10) {
            let on_ms = (*total_on_ns as f64) / 1_000_000.0;
            let off_ms = (*total_off_ns as f64) / 1_000_000.0;
            log::info!("  tid={tid} on-cpu={on_ms:.2}ms off-cpu={off_ms:.2}ms");
        }
    }
}
