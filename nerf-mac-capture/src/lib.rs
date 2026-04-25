//! macOS sampling backend for nerf.
//!
//! The Mach-based capture plumbing is vendored from samply
//! (<https://github.com/mstange/samply>, MIT OR Apache-2.0). See the crate's
//! `LICENSE-MIT` / `LICENSE-APACHE`.
//!
//! On non-macOS targets this crate is intentionally empty so the workspace
//! still builds cross-platform.

#![cfg(target_os = "macos")]
