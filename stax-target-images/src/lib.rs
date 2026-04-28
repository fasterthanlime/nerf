//! Walk a target Mach task's loaded image registry without
//! relying on `task_threads` / `proc_regionfilename` heuristics.
//!
//! ## What this crate does
//!
//! Given a Mach task port (acquired by the caller via
//! `task_for_pid`, requires `cs.debugger`), enumerate every dyld
//! image the target has loaded and for each one return:
//!
//! - install path (e.g. `/usr/lib/libsystem_c.dylib`)
//! - runtime load address (AVMA of `__TEXT`)
//! - `__TEXT` size
//! - LC_UUID if present
//!
//! Per-image Mach-O parsing — turning the in-memory header into
//! a fully-populated `framehop::ModuleSectionInfo` for the
//! unwinder — lands on top of this skeleton in a follow-up
//! commit. The interface is shaped now so callers can wire the
//! walker into a periodic sampling loop and get the path /
//! address pairs they need for symbolication today, then pick
//! up unwind sections when they're available.
//!
//! ## Why a separate crate
//!
//! The dyld-walking + Mach-O-parsing concern is self-contained:
//! it touches `mach2` + raw FFI + a Mach-O parser, and nothing
//! else in the stax architecture needs those things. Keeping it
//! out of `stax-shade` lets the shade focus on its lifecycle
//! (registration, sampling timer, peek/poke surface) without
//! also being a Mach-O parser. Lets the walker be unit-tested
//! against a known-good binary in isolation.
//!
//! ## How dyld exposes its image list
//!
//! `task_info(target, TASK_DYLD_INFO, …)` returns a struct
//! containing `all_image_info_addr`: the AVMA inside the target
//! of dyld's process-wide `dyld_all_image_infos` block. That
//! block has stable layout (defined in xnu's `dyld_images.h`)
//! and includes a pointer + count for the array of currently-
//! mapped images. We `mach_vm_read` the block, then read the
//! image array, then for each entry follow `imageFilePath` to
//! get the install name and `imageLoadAddress` to get the AVMA.
//!
//! Apple Silicon caveat: most images live inside the dyld_shared
//! _cache. Their `imageLoadAddress` still points into the
//! process's address space — the cache is mapped — so the
//! caller can `mach_vm_read` against those addresses normally.

#![cfg(target_os = "macos")]

mod dyld;

pub use dyld::{ImageEntry, TargetImageWalker, WalkError};
