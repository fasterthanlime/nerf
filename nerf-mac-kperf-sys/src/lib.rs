//! Privileged-side bindings for macOS sampling: Apple's `kperf` /
//! `kperfdata` private frameworks plus the `KERN_KDEBUG` sysctl
//! surface. Calling anything in this crate at runtime requires either
//! root or an Apple-private entitlement we can't sign with; the
//! intent is that this is the only crate the eventual `nperfd`
//! daemon needs to link from the kperf side. The unprivileged
//! parsing / symbolication / libproc half lives in
//! `nerf-mac-kperf-parse` and never touches anything in here at
//! runtime — it only re-uses `KdBuf`, the debugid encoding, and the
//! sampler/kpc constants as inert data.
//!
//! On non-macOS targets this crate is intentionally empty.

#![cfg(target_os = "macos")]

pub mod bindings;
pub mod error;
pub mod kdebug;
pub mod pmu_events;

pub use error::Error;
