//! Shared client-side pipeline: takes raw `KdBuf` records (from
//! either the in-process `KERN_KDREADTR` drain or a remote daemon's
//! `Tx<KdBufBatch>` stream) and emits the same fully-resolved events
//! to a [`SampleSink`].
//!
//! Owns:
//!   * the kperf-record [`Parser`] (assembles `KdBuf` → `Sample`),
//!   * the SCHED-driven [`CpuIntervalTracker`] (on/off-CPU intervals,
//!     wakeup edges, last-on-CPU stack cache for off-CPU attribution),
//!   * the libproc [`ImageScanner`] (BinaryLoaded/Unloaded events,
//!     periodic rescan),
//!   * the libproc [`ThreadNameCache`] (ThreadName events, periodic
//!     rescan),
//!   * the on-disk [`KernelImage`] + [`SlideEstimator`] (sample-driven
//!     KASLR slide derivation, `/proc/kallsyms` blob at end-of-session),
//!   * the `/tmp/jit-<pid>.dump` [`JitdumpTailer`] + first-time
//!     emission (synthetic `BinaryLoaded` for each JIT'd function),
//!   * diagnostic counters (parser stats, drained-record total,
//!     PMU sums, DBG_PERF histogram).
//!
//! Both drivers share the same lifecycle:
//!
//!     let mut pipe = Pipeline::new(config, shared_cache, sink);
//!     loop {
//!         pipe.tick(sink);                 // periodic libproc scans
//!         pipe.process_records(&records, sink);  // batched parse
//!     }
//!     pipe.finish(sink);                   // kallsyms + summary
//!
//! The in-process driver feeds `process_records` the slice it just
//! drained from `KERN_KDREADTR`; the daemon-driven driver feeds it
//! whatever arrived in the latest `KdBufBatch`. Sample / interval /
//! wakeup emission, off-CPU stack caching, slide observation, and the
//! "drop empty-user samples" filter live here so neither driver
//! diverges.

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use stax_mac_capture::proc_maps::MachOSymbol;
use stax_mac_capture::recorder::ThreadNameCache;
use stax_mac_capture::sample_sink::{CpuIntervalEvent, CpuIntervalKind};
use stax_mac_capture::{
    BinaryLoadedEvent, JitdumpEvent, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};
use stax_mac_kperf_sys::kdebug::{
    self, kdbg_class, kdbg_code, kdbg_func, kdbg_subclass, KdBuf, DBG_MACH, DBG_PERF,
};

use crate::image_scan::ImageScanner;
use crate::jitdump_tail::JitdumpTailer;
use crate::kernel_symbols::{KernelImage, SlideEstimator};
use crate::libproc;
use crate::offcpu::{CpuIntervalTracker, PendingKind};
use crate::parser::Parser;

const IMAGE_RESCAN_PERIOD: Duration = Duration::from_millis(250);
const THREAD_NAME_RESCAN_PERIOD: Duration = Duration::from_millis(50);
const JITDUMP_PROBE_PERIOD: Duration = Duration::from_millis(500);

/// Inputs the caller has resolved before constructing the pipeline:
/// the pid being recorded, the sample frequency (only used for stats
/// today), and the indices into the per-sample `pmc` slice where
/// configurable counters land. Pass `None` for the indices when no
/// configurable counters were programmed.
pub struct PipelineConfig {
    pub pid: u32,
    pub frequency_hz: u32,
    pub pmc_idx_l1d: Option<usize>,
    pub pmc_idx_brmiss: Option<usize>,
}

/// All of the per-session client-side state, factored out so the
/// in-process recorder and the staxd-client both build it identically.
pub struct Pipeline {
    config: PipelineConfig,
    parser: Parser,
    offcpu: CpuIntervalTracker,
    images: ImageScanner,
    thread_names: ThreadNameCache,
    /// Set of kernel thread_ids the parser has actually observed in
    /// the kperf stream. We resolve names against *these* tids
    /// rather than libproc's `PROC_PIDLISTTHREADS` (which returns
    /// Mach thread-handles — wrong keyspace for kperf's tids and
    /// silently leaves every thread unnamed in the live registry).
    seen_tids: std::collections::HashSet<u32>,
    kernel_image: Option<KernelImage>,
    slide_est: Option<SlideEstimator>,
    jitdump_path: PathBuf,
    jitdump_tailer: Option<JitdumpTailer>,
    jitdump_emitted: bool,
    next_image: Instant,
    next_thread: Instant,
    next_jitdump: Instant,
    histogram: BTreeMap<(u8, u16, u32), u64>,
    total_drained: u64,
    pmu_total_cycles: u64,
    pmu_total_insns: u64,
    pmu_samples: u64,
}

impl Pipeline {
    /// Build the pipeline and emit the initial libproc scans
    /// (BinaryLoaded for each currently-loaded image, ThreadName for
    /// each thread). Loading the kernel image is best-effort: if it
    /// fails we just don't run the slide estimator and skip kallsyms
    /// at finish.
    pub fn new<S: SampleSink>(
        config: PipelineConfig,
        shared_cache: Option<Arc<stax_mac_shared_cache::SharedCache>>,
        sink: &mut S,
    ) -> Self {
        if let Some(sc) = shared_cache.clone() {
            sink.on_macho_byte_source(sc);
        }
        let mut images = ImageScanner::new(shared_cache);
        let mut thread_names = ThreadNameCache::new();

        let t0 = Instant::now();
        images.rescan(config.pid, sink);
        log::info!("initial image scan took {:?}", t0.elapsed());

        let kernel_image = match KernelImage::load() {
            Ok(img) => img,
            Err(err) => {
                log::warn!("kernel image load failed: {err:?}");
                None
            }
        };
        let slide_est = kernel_image
            .as_ref()
            .map(|img| SlideEstimator::new(img.exec_segments.clone()));

        // Initial scan pulls names for whatever tids libproc tells
        // us about; subsequent scans follow the kperf-observed set.
        scan_thread_names(config.pid, sink, &mut thread_names);

        let now = Instant::now();
        let jitdump_path = PathBuf::from(format!("/tmp/jit-{}.dump", config.pid));
        Self {
            config,
            parser: Parser::new(),
            offcpu: CpuIntervalTracker::new(),
            images,
            thread_names,
            seen_tids: std::collections::HashSet::new(),
            kernel_image,
            slide_est,
            jitdump_path,
            jitdump_tailer: None,
            jitdump_emitted: false,
            next_image: now + IMAGE_RESCAN_PERIOD,
            next_thread: now + THREAD_NAME_RESCAN_PERIOD,
            next_jitdump: now + JITDUMP_PROBE_PERIOD,
            histogram: BTreeMap::new(),
            total_drained: 0,
            pmu_total_cycles: 0,
            pmu_total_insns: 0,
            pmu_samples: 0,
        }
    }

    /// Run the periodic libproc + jitdump tasks. Cheap when nothing
    /// is due; the in-process driver calls this at every drain-loop
    /// wake-up, the daemon-driven driver calls it on every receive
    /// timeout / batch.
    pub fn tick<S: SampleSink>(&mut self, sink: &mut S) {
        let now = Instant::now();
        if now >= self.next_image {
            self.images.rescan(self.config.pid, sink);
            self.next_image = now + IMAGE_RESCAN_PERIOD;
        }
        if now >= self.next_thread {
            scan_thread_names_for_observed(
                self.config.pid,
                sink,
                &mut self.thread_names,
                &self.seen_tids,
            );
            self.next_thread = now + THREAD_NAME_RESCAN_PERIOD;
        }
        if !self.jitdump_emitted && now >= self.next_jitdump {
            if self.jitdump_path.exists() {
                sink.on_jitdump(JitdumpEvent {
                    pid: self.config.pid,
                    path: &self.jitdump_path,
                });
                self.jitdump_emitted = true;
                match JitdumpTailer::open(&self.jitdump_path) {
                    Ok(t) => {
                        log::info!(
                            "jitdump_tail: opened {} for live tailing",
                            self.jitdump_path.display()
                        );
                        self.jitdump_tailer = Some(t);
                    }
                    Err(err) => log::warn!(
                        "jitdump_tail: failed to open {}: {err}",
                        self.jitdump_path.display()
                    ),
                }
            } else {
                self.next_jitdump = now + JITDUMP_PROBE_PERIOD;
            }
        }
        if let Some(t) = self.jitdump_tailer.as_mut() {
            match t.tick() {
                Ok(records) => {
                    for r in records {
                        // Synthetic BinaryLoaded per JIT'd function.
                        // base_avma == text_svma since JIT code has
                        // no relocatable layout.
                        let path = format!("[jit] {}", r.name);
                        let symbols = vec![MachOSymbol {
                            start_svma: r.avma,
                            end_svma: r.avma + r.code_size,
                            name: r.name.into_bytes(),
                        }];
                        sink.on_binary_loaded(BinaryLoadedEvent {
                            pid: self.config.pid,
                            base_avma: r.avma,
                            vmsize: r.code_size,
                            text_svma: r.avma,
                            path: &path,
                            uuid: None,
                            arch: host_arch_str(),
                            is_executable: false,
                            symbols: &symbols,
                            text_bytes: Some(&r.code),
                        });
                    }
                }
                Err(err) => log::warn!("jitdump_tail tick: {err}"),
            }
        }
    }

    /// Drive the parser + off-CPU tracker over a batch of records,
    /// emit `SampleEvent`s, `WakeupEvent`s, and `CpuIntervalEvent`s
    /// to the sink. The caller is responsible for sourcing the
    /// records (drain via `KERN_KDREADTR`, or receive via vox).
    pub fn process_records<S: SampleSink>(&mut self, records: &[KdBuf], sink: &mut S) {
        self.total_drained += records.len() as u64;
        let pid = self.config.pid;
        let pmc_idx_l1d = self.config.pmc_idx_l1d;
        let pmc_idx_brmiss = self.config.pmc_idx_brmiss;
        for rec in records {
            let class = kdbg_class(rec.debugid);
            if class == DBG_PERF {
                let key = (
                    kdbg_subclass(rec.debugid),
                    kdbg_code(rec.debugid),
                    kdbg_func(rec.debugid),
                );
                *self.histogram.entry(key).or_insert(0) += 1;
            } else if class == DBG_MACH && kdbg_subclass(rec.debugid) == kdebug::DBG_MACH_SCHED {
                self.offcpu.feed(rec);
                continue;
            }
            // Locals so the closure doesn't have to borrow from
            // &mut self twice (parser + slide_est + offcpu + counters).
            let slide_est = &mut self.slide_est;
            let offcpu = &mut self.offcpu;
            let pmu_samples = &mut self.pmu_samples;
            let pmu_total_cycles = &mut self.pmu_total_cycles;
            let pmu_total_insns = &mut self.pmu_total_insns;
            let seen_tids = &mut self.seen_tids;
            self.parser.feed(rec, |sample| {
                seen_tids.insert(sample.tid);
                if let Some(est) = slide_est.as_mut() {
                    // The deepest kernel frame is the most stable
                    // entry point, but any kernel-text PC works as
                    // a slide constraint.
                    for &avma in sample.kernel_backtrace {
                        est.observe(avma);
                    }
                }
                if !sample.pmc.is_empty() {
                    *pmu_samples += 1;
                    if let Some(&c) = sample.pmc.first() {
                        *pmu_total_cycles = pmu_total_cycles.saturating_add(c);
                    }
                    if let Some(&i) = sample.pmc.get(1) {
                        *pmu_total_insns = pmu_total_insns.saturating_add(i);
                    }
                }
                offcpu.note_sample(sample.tid, sample.user_backtrace, sample.kernel_backtrace);
                // With lightweight_pet=0 kperf brackets every thread
                // every tick, so blocked threads emit empty-stack
                // samples that just inflate the in-kernel residue.
                // Drop them at the source.
                if sample.user_backtrace.is_empty() {
                    return;
                }
                let cycles = sample.pmc.first().copied().unwrap_or(0);
                let instructions = sample.pmc.get(1).copied().unwrap_or(0);
                let l1d_misses = pmc_idx_l1d
                    .and_then(|i| sample.pmc.get(i).copied())
                    .unwrap_or(0);
                let branch_mispreds = pmc_idx_brmiss
                    .and_then(|i| sample.pmc.get(i).copied())
                    .unwrap_or(0);
                sink.on_sample(SampleEvent {
                    timestamp_ns: sample.timestamp_ns,
                    pid,
                    tid: sample.tid,
                    backtrace: sample.user_backtrace,
                    kernel_backtrace: sample.kernel_backtrace,
                    cycles,
                    instructions,
                    l1d_misses,
                    branch_mispreds,
                });
            });
        }

        for w in self.offcpu.drain_wakeups() {
            sink.on_wakeup(WakeupEvent {
                timestamp_ns: w.timestamp_ns,
                pid,
                waker_tid: w.waker_tid,
                wakee_tid: w.wakee_tid,
                waker_user_stack: &w.waker_user_stack,
                waker_kernel_stack: &w.waker_kernel_stack,
            });
        }
        for interval in self.offcpu.drain_pending() {
            if !self.seen_tids.contains(&interval.tid) {
                continue;
            }
            match interval.kind {
                PendingKind::OnCpu => {
                    sink.on_cpu_interval(CpuIntervalEvent {
                        pid,
                        tid: interval.tid,
                        start_ns: interval.start_ns,
                        end_ns: interval.end_ns,
                        kind: CpuIntervalKind::OnCpu,
                    });
                }
                PendingKind::OffCpu {
                    user_stack,
                    kernel_stack: _,
                    waker_tid,
                    waker_user_stack,
                } => {
                    sink.on_cpu_interval(CpuIntervalEvent {
                        pid,
                        tid: interval.tid,
                        start_ns: interval.start_ns,
                        end_ns: interval.end_ns,
                        kind: CpuIntervalKind::OffCpu {
                            stack: &user_stack,
                            waker_tid,
                            waker_user_stack: waker_user_stack.as_deref(),
                        },
                    });
                }
            }
        }
    }

    /// End-of-session: finalize the slide estimator and emit kallsyms,
    /// then log diagnostic summary (record total + parser stats +
    /// DBG_PERF histogram + off-CPU summary + PMU totals).
    pub fn finish<S: SampleSink>(self, sink: &mut S) {
        let Self {
            config,
            parser,
            offcpu,
            images: _,
            thread_names: _,
            seen_tids: _,
            kernel_image,
            slide_est,
            jitdump_path: _,
            jitdump_tailer: _,
            jitdump_emitted: _,
            next_image: _,
            next_thread: _,
            next_jitdump: _,
            histogram,
            total_drained,
            pmu_total_cycles,
            pmu_total_insns,
            pmu_samples,
        } = self;
        let _ = config;

        if let (Some(image), Some(est)) = (kernel_image, slide_est) {
            match est.finalize() {
                Some((slide, support)) => {
                    log::info!(
                        "kernel slide derived: {slide:#x} (support {:.1}% \
                         over {} sampled kernel addresses)",
                        support * 100.0,
                        est.observed_count(),
                    );
                    let kallsyms = image.format_kallsyms(slide);
                    sink.on_kallsyms(&kallsyms);
                }
                None => log::warn!("kernel slide estimator collected no votes; skipping kallsyms"),
            }
        }

        let s = &parser.stats;
        log::info!(
            "kdebug records drained: {total_drained}, samples \
             started/emitted/orphaned: {}/{}/{}, walk errors u/k: {}/{}",
            s.samples_started,
            s.samples_emitted,
            s.samples_orphaned,
            s.user_walk_errors,
            s.kernel_walk_errors,
        );
        log::info!("DBG_PERF histogram (subclass, code, func) -> count:");
        for ((sc, code, func), count) in &histogram {
            log::info!("  ({sc:>2}, {code:>3}, {func}) -> {count}");
        }
        offcpu.log_summary();

        if pmu_samples > 0 {
            let ipc = if pmu_total_cycles > 0 {
                pmu_total_insns as f64 / pmu_total_cycles as f64
            } else {
                0.0
            };
            log::info!(
                "PMU (fixed counters across {pmu_samples} samples): \
                 cycles={pmu_total_cycles} insns={pmu_total_insns} avg_ipc={ipc:.3}"
            );
        } else {
            log::warn!(
                "PMU: no per-sample counter records observed -- \
                 kperf likely didn't emit DBG_PERF/PERF_KPC/DATA_THREAD \
                 (kpc_set_thread_counting may need different gating, or PMC_THREAD \
                 might require kpc_force_all_ctrs_set permission this run lacks)"
            );
        }
    }

    pub fn total_drained(&self) -> u64 {
        self.total_drained
    }
}

fn scan_thread_names<S: SampleSink>(pid: u32, sink: &mut S, cache: &mut ThreadNameCache) {
    // Initial scan: ask libproc for whatever it knows. Later
    // scans (`scan_thread_names_for_observed`) follow kperf's tid
    // set, which is the only thing the live registry actually
    // gets keyed by.
    let tids = match libproc::list_thread_ids(pid) {
        Ok(t) => t,
        Err(_) => return,
    };
    for tid64 in tids {
        let tid = tid64 as u32;
        if let Ok(Some(name)) = libproc::thread_name(pid, tid64) {
            if cache.note_thread(tid, &name) {
                sink.on_thread_name(ThreadNameEvent {
                    pid,
                    tid,
                    name: &name,
                });
            }
        }
    }
}

/// Resolve names for the kernel `thread_id`s the parser has
/// actually observed in the kperf stream. `PROC_PIDLISTTHREADS`
/// returns Mach thread-handles, which are *not* the same as
/// kperf's tids — feeding those handles back into thread-name
/// lookup silently no-ops every entry. Iterating the observed-
/// tid set with the matching `thread_name_by_id` flavour is the
/// fix.
fn scan_thread_names_for_observed<S: SampleSink>(
    pid: u32,
    sink: &mut S,
    cache: &mut ThreadNameCache,
    seen: &std::collections::HashSet<u32>,
) {
    for &tid in seen {
        if let Ok(Some(name)) = libproc::thread_name_by_id(pid, tid as u64) {
            if cache.note_thread(tid, &name) {
                sink.on_thread_name(ThreadNameEvent {
                    pid,
                    tid,
                    name: &name,
                });
            }
        }
    }
}

fn host_arch_str() -> Option<&'static str> {
    if cfg!(target_arch = "aarch64") {
        Some("aarch64")
    } else if cfg!(target_arch = "x86_64") {
        Some("x86_64")
    } else {
        None
    }
}
