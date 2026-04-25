//! macOS sampling backend for nerf.
//!
//! The Mach plumbing is vendored from samply
//! (<https://github.com/mstange/samply>, MIT OR Apache-2.0) at commit
//! `1920bd32c569de5650d1129eb035f43bd28ace27`. See `LICENSE-MIT` /
//! `LICENSE-APACHE` for the original copyright.
//!
//! This crate currently exposes only the leaf-level Mach utilities (FFI
//! bindings, kernel errors, thread/time helpers, IPC). The higher-level
//! sampling pipeline (`proc_maps`, `task_profiler`, `thread_profiler`,
//! `sampler`, `process_launcher`) is heavily coupled to samply's Firefox
//! profiler types and will be vendored + stripped in a subsequent step
//! (M2 of the macOS roadmap, see `notes/mac-roadmap.md`).
//!
//! On non-macOS targets this crate is intentionally empty so the workspace
//! still builds cross-platform.

#![cfg(target_os = "macos")]

#[allow(deref_nullptr)]
pub mod dyld_bindings;
pub mod error;
pub mod kernel_error;
pub mod mach_ipc;
pub mod thread_act;
pub mod thread_info;
pub mod time;
