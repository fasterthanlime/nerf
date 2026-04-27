//! Parsed dyld_shared_cache + a typed byte-source trait that lets
//! consumers fetch raw `__TEXT` bytes for any address inside the
//! cache's runtime range. Shared between the recorder
//! (`nerf-mac-kperf`, for image enumeration during sampling) and
//! the live UI (`nperf-live`, for disassembly bytes when a sampled
//! address falls inside a system library and we have no on-disk
//! file or task port to read from).
//!
//! The cache is opened once per process via `Box::leak` so the
//! `'static` lifetime can flow cleanly through `Arc<dyn
//! MachOByteSource>` without a self-referential struct dance. See
//! the inline comments on `mmap_static` for the rationale.
//!
//! On non-macOS targets this crate is intentionally empty.

#![cfg_attr(not(target_os = "macos"), allow(unused))]

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::*;

// `MachOByteSource` itself lives in `nerf-mac-capture` so the
// `SampleSink` trait there can ferry an `Arc<dyn MachOByteSource>`
// from the recorder to the live UI. Re-export for convenience.
#[cfg(target_os = "macos")]
pub use nerf_mac_capture::MachOByteSource;
