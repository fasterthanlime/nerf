//! macOS sampling backend driving Apple's private `kperf` /
//! `kperfdata` frameworks via PET (Profile Every Thread). Drains
//! samples from the kdebug ringbuffer.
//!
//! Intentionally separate from `nerf-mac-capture` (the
//! suspend-and-sample backend): the lifecycle is process-wide rather
//! than task-local, the kernel walks frame pointers itself instead of
//! framehop, and kernel stacks come for free.
//!
//! References:
//! - mperf (<https://github.com/tmcgilchrist/mperf>, MIT) for the PET
//!   driver shape.
//! - ibireme's kpc_demo.c (public domain) for the kpc/kperf sequence.
//!
//! On non-macOS targets this crate is intentionally empty.

#![cfg(target_os = "macos")]

pub mod bindings;
pub mod error;
pub mod image_scan;
pub mod jitdump_tail;
pub mod kdebug;
pub mod kernel_symbols;
pub mod libproc;
pub mod offcpu;
pub mod parser;
pub mod pmu_events;
pub mod recorder;

pub use nperf_mac_shared_cache as shared_cache;

pub use error::Error;

pub use recorder::{record, RecordOptions};
