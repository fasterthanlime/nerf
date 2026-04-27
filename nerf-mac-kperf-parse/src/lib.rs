//! Unprivileged-side macOS sampling support: kdebug record parsing,
//! off-CPU correlation, libproc-based image enumeration, on-disk
//! kernel symbol resolution, and incremental jitdump tailing.
//!
//! Pairs with `nerf-mac-kperf-sys`, which is the privileged half
//! (kperf framework calls + KERN_KDEBUG sysctl). This crate consumes
//! the `KdBuf` record stream that `-sys` produces and turns it into
//! the higher-level events the rest of nerf works with.
//!
//! The split exists so that, when the eventual `nperfd` daemon
//! lands, the daemon links `-sys` and the client links `-parse` —
//! everything in here can run as the calling user with no kperf
//! privileges.
//!
//! On non-macOS targets this crate is intentionally empty.

#![cfg(target_os = "macos")]

pub mod image_scan;
pub mod jitdump_tail;
pub mod kernel_symbols;
pub mod libproc;
pub mod offcpu;
pub mod parser;
