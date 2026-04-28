//! State machine that turns the kdebug record stream emitted by
//! kperf's PET sampler into one `Sample` per (timestamp, tid).
//!
//! The records come in this order, per thread, per PET tick:
//!
//! ```text
//!   PERF_GEN_EVENT | DBG_FUNC_START           begin sample (tid in arg5)
//!     [PERF_TI_DATA ...]                      thread info, ignored here
//!     PERF_CS_UHDR  arg1=flags arg2=nframes-async arg3=async_idx arg4=async_nframes
//!     PERF_CS_UDATA arg1..arg4 = up to 4 frames    (repeats until nframes consumed)
//!     [PERF_CS_KHDR + PERF_CS_KDATA ...]      kernel side, same shape
//!     [PERF_CS_ERROR arg1=where arg2=errno]   walk failure
//!   PERF_GEN_EVENT | DBG_FUNC_END             end sample
//! ```
//!
//! `PERF_GEN_EVENT` (subclass `PERF_GENERIC`, code 0) is emitted by
//! `kperf_sample_internal` around *every* sample regardless of
//! trigger (timer, PET, PMI), so it's the universal boundary.
//!
//! See xnu `osfmk/kperf/{buffer.h, callstack.c, kperf.c, pet.c}`.

use stax_mac_kperf_sys::kdebug::{
    kdbg_class, kdbg_code, kdbg_func, kdbg_subclass, perf, KdBuf, DBG_FUNC_END, DBG_FUNC_START,
    DBG_PERF, KDBG_TIMESTAMP_MASK,
};

/// One completed sample. Lifetime tied to the parser's internal
/// frame buffer; copy out before feeding the next record if you
/// need to keep it.
pub struct Sample<'a> {
    pub timestamp_ns: u64,
    pub tid: u32,
    /// Callee-most first.
    pub user_backtrace: &'a [u64],
    /// Kernel frames, callee-most first. Empty if no kernel walk
    /// was attempted or it failed.
    pub kernel_backtrace: &'a [u64],
    /// Per-thread CPU performance counter deltas at this PET tick.
    /// On Apple Silicon's fixed counters: index 0 is cycles, index 1
    /// is instructions retired. Empty if PMC_THREAD wasn't in the
    /// sampler bits or the kernel didn't emit a record for this
    /// sample.
    pub pmc: &'a [u64],
}

#[derive(Default)]
pub struct ParserStats {
    pub samples_emitted: u64,
    pub samples_started: u64,
    pub samples_orphaned: u64,
    pub user_walk_errors: u64,
    pub kernel_walk_errors: u64,
}

pub struct Parser {
    state: State,
    user_frames: Vec<u64>,
    kernel_frames: Vec<u64>,
    pmc: Vec<u64>,
    user_remaining: u32,
    kernel_remaining: u32,
    pub stats: ParserStats,
}

enum State {
    Idle,
    InSample { tid: u32, timestamp_ns: u64 },
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            user_frames: Vec::with_capacity(128),
            kernel_frames: Vec::with_capacity(32),
            pmc: Vec::with_capacity(4),
            user_remaining: 0,
            kernel_remaining: 0,
            stats: ParserStats::default(),
        }
    }

    /// Feed one record. If a sample completes, `emit` is called
    /// with a borrowed [`Sample`] view of the parser's internal
    /// frame buffers.
    pub fn feed(&mut self, rec: &KdBuf, mut emit: impl FnMut(Sample<'_>)) {
        let class = kdbg_class(rec.debugid);
        if class != DBG_PERF {
            return;
        }
        let subclass = kdbg_subclass(rec.debugid);
        let code = kdbg_code(rec.debugid);
        let func = kdbg_func(rec.debugid);

        match (subclass, code, func) {
            // -- Sample boundary --------------------------------------------
            (perf::sc::GENERIC, 0, DBG_FUNC_START) => {
                if matches!(self.state, State::InSample { .. }) {
                    self.stats.samples_orphaned += 1;
                }
                self.user_frames.clear();
                self.kernel_frames.clear();
                self.pmc.clear();
                self.user_remaining = 0;
                self.kernel_remaining = 0;
                self.state = State::InSample {
                    tid: rec.arg5 as u32,
                    timestamp_ns: rec.timestamp & KDBG_TIMESTAMP_MASK,
                };
                self.stats.samples_started += 1;
            }
            (perf::sc::GENERIC, 0, DBG_FUNC_END) => {
                if let State::InSample { tid, timestamp_ns } = self.state {
                    // Trim trailing zeros from the PMC slice: a 4-arg
                    // record covers up to 4 counters, but Apple
                    // Silicon's fixed class only populates 2 (cycles,
                    // instructions). Anything past the last non-zero
                    // is padding.
                    let pmc_len = self
                        .pmc
                        .iter()
                        .rposition(|&v| v != 0)
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    // Truncate the user stack at the first NULL frame.
                    // kperf's frame-pointer walker pushes 0 as a
                    // "no more frames" sentinel when it reaches the
                    // bottom of the call chain (typically just past
                    // _pthread_start), but it also doesn't always
                    // stop there — JIT'd code with malformed frame
                    // pointers can send the walker into a loop, so
                    // we sometimes see [..., real_root, 0, garbage,
                    // garbage_loop_back_to_leaf]. Everything from the
                    // NULL onward is unreliable; drop it.
                    let user_len = self
                        .user_frames
                        .iter()
                        .position(|&a| a == 0)
                        .unwrap_or(self.user_frames.len());
                    let kernel_len = self
                        .kernel_frames
                        .iter()
                        .position(|&a| a == 0)
                        .unwrap_or(self.kernel_frames.len());
                    emit(Sample {
                        timestamp_ns,
                        tid,
                        user_backtrace: &self.user_frames[..user_len],
                        kernel_backtrace: &self.kernel_frames[..kernel_len],
                        pmc: &self.pmc[..pmc_len],
                    });
                    self.stats.samples_emitted += 1;
                }
                self.state = State::Idle;
            }

            // -- User stack -------------------------------------------------
            (perf::sc::CALLSTACK, perf::cs::UHDR, _) => {
                if matches!(self.state, State::InSample { .. }) {
                    let main = rec.arg2 as u32;
                    let async_n = rec.arg4 as u32;
                    self.user_remaining = main.saturating_add(async_n);
                    self.user_frames.reserve(self.user_remaining as usize);
                }
            }
            (perf::sc::CALLSTACK, perf::cs::UDATA, _) => {
                if matches!(self.state, State::InSample { .. }) {
                    self.append_chunk(rec, /* user = */ true);
                }
            }

            // -- Kernel stack -----------------------------------------------
            (perf::sc::CALLSTACK, perf::cs::KHDR, _) => {
                if matches!(self.state, State::InSample { .. }) {
                    let main = rec.arg2 as u32;
                    let async_n = rec.arg4 as u32;
                    self.kernel_remaining = main.saturating_add(async_n);
                    self.kernel_frames.reserve(self.kernel_remaining as usize);
                }
            }
            (perf::sc::CALLSTACK, perf::cs::KDATA, _) => {
                if matches!(self.state, State::InSample { .. }) {
                    self.append_chunk(rec, /* user = */ false);
                }
            }

            // -- PMU counter values -----------------------------------------
            (perf::sc::KPC, perf::kpc::DATA_THREAD, _) => {
                if matches!(self.state, State::InSample { .. }) {
                    self.pmc.push(rec.arg1);
                    self.pmc.push(rec.arg2);
                    self.pmc.push(rec.arg3);
                    self.pmc.push(rec.arg4);
                }
            }

            // -- Walk failure -----------------------------------------------
            (perf::sc::CALLSTACK, perf::cs::ERROR, _) => {
                // arg1 = where (USAMPLE/KSAMPLE), arg2 = errno
                let from_kernel = rec.arg1 == perf::cs::KSAMPLE as u64;
                if from_kernel {
                    self.stats.kernel_walk_errors += 1;
                } else {
                    self.stats.user_walk_errors += 1;
                }
            }

            _ => {}
        }
    }

    fn append_chunk(&mut self, rec: &KdBuf, user: bool) {
        let chunk = [rec.arg1, rec.arg2, rec.arg3, rec.arg4];
        let (frames, remaining) = if user {
            (&mut self.user_frames, &mut self.user_remaining)
        } else {
            (&mut self.kernel_frames, &mut self.kernel_remaining)
        };
        let take = (*remaining as usize).min(4);
        frames.extend_from_slice(&chunk[..take]);
        *remaining = remaining.saturating_sub(4);
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}
