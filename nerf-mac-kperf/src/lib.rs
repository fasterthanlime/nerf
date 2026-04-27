//! macOS sampling backend driving Apple's private `kperf` /
//! `kperfdata` frameworks via PET (Profile Every Thread). Drains
//! samples from the kdebug ringbuffer.
//!
//! Intentionally separate from `nerf-mac-capture` (the
//! suspend-and-sample backend): the lifecycle is process-wide rather
//! than task-local, the kernel walks frame pointers itself instead of
//! framehop, and kernel stacks come for free.
//!
//! This is the orchestration crate. It depends on
//! [`nerf-mac-kperf-sys`] (the privileged half — framework loading,
//! kdebug sysctls, PMU config) and [`nerf-mac-kperf-parse`] (the
//! unprivileged half — sample assembly, libproc, symbols), and ties
//! them together in `record()`. The split is in preparation for an
//! eventual `nperfd` daemon: the daemon will link `-sys` and stream
//! `KdBuf` records to clients that only need `-parse`.
//!
//! References:
//! - mperf (<https://github.com/tmcgilchrist/mperf>, MIT) for the PET
//!   driver shape.
//! - ibireme's kpc_demo.c (public domain) for the kpc/kperf sequence.
//!
//! On non-macOS targets this crate is intentionally empty.

#![cfg(target_os = "macos")]

pub mod recorder;

// Re-export the sub-crates so existing consumers (nperf-core,
// examples, downstream crates) can keep saying
// `nerf_mac_kperf::kdebug::*` etc. without churn.
pub use nerf_mac_kperf_parse::{image_scan, jitdump_tail, kernel_symbols, libproc, offcpu, parser};
pub use nerf_mac_kperf_sys::{bindings, error, kdebug, pmu_events, Error};

pub use nperf_mac_shared_cache as shared_cache;

pub use recorder::{record, RecordOptions};
