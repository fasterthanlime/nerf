//! Framehop-driven user-stack walker.
//!
//! ## Why
//!
//! kperf walks user stacks in-kernel using frame pointers. That
//! works for code that keeps FPs (most C/C++/Swift). It silently
//! truncates or skips frames in code that doesn't:
//!
//! - Rust release builds without `-C force-frame-pointers=yes`
//! - JIT'd code with custom prologues (vox-jit, cranelift output,
//!   any tracing JIT we'd want to profile)
//! - Hand-written assembly
//!
//! Framehop reconstructs stacks from the on-disk unwind tables
//! (`__unwind_info`, `__compact_unwind`, `.eh_frame`) without
//! needing FPs to be present. To drive it we need three things:
//!
//! 1. **Initial register state** — IP/SP/FP/LR/etc at the moment
//!    of the sample. Got via `thread_get_state(ARM_THREAD_STATE64)`
//!    on a suspended thread.
//! 2. **Stack memory** — for the unwinder to read CFA expressions
//!    and saved registers off the stack. Got via `mach_vm_read`
//!    against the target task port.
//! 3. **Per-module unwind sections** — `__unwind_info` /
//!    `__compact_unwind` / `.eh_frame` blobs for every binary
//!    loaded in the target, addressable by AVMA. We don't have
//!    those yet on the shade side; the next commit wires
//!    target-image enumeration (read `dyld_all_image_infos` from
//!    the target's memory and lazily fetch unwind sections per
//!    image).
//!
//! ## What this commit ships
//!
//! Foundational types so the framehop dep lands and the
//! integration shape is in tree:
//!
//! - `MachStackReader` — `framehop::MemoryRead`-style
//!   accessor backed by `mach_vm_read`. Hot-path: read 8 bytes
//!   at a time from the suspended target.
//! - `walk_thread_snapshot` — public entry point that, given a
//!   target task port + thread port, would suspend the thread,
//!   pull `ARM_THREAD_STATE64`, and feed framehop. Currently a
//!   stub that returns the IP only — wiring the per-module
//!   unwinder is the next slice.
//!
//! No periodic walking yet, no integration with the
//! `stax-shade-proto::Shade` service, no streaming back to
//! stax-server. Those land on top.

#![cfg(target_os = "macos")]

use mach2::kern_return::KERN_SUCCESS;
use mach2::port::mach_port_t;

/// Memory accessor backed by `mach_vm_read_overwrite` against
/// the target task. Holds the task port (a Mach right we acquired
/// via `task_for_pid`) by value. Cheap to copy — `mach_port_t` is
/// a `u32` underneath; the right itself is reference-counted in
/// the kernel.
#[derive(Copy, Clone)]
#[allow(dead_code)] // wired in the periodic-walker commit
pub struct MachStackReader {
    pub task: mach_port_t,
}

impl MachStackReader {
    /// Read exactly `buf.len()` bytes starting at `addr` (target
    /// AVMA) into `buf`. Returns `false` on partial reads or any
    /// kernel-side failure (unmapped page, protection violation,
    /// task port revoked, …) — framehop treats unreadable stack
    /// memory as a hard wall, which is the correct conservative
    /// answer.
    #[allow(dead_code)] // wired in the periodic-walker commit
    pub fn read_exact(&self, addr: u64, buf: &mut [u8]) -> bool {
        use mach2::vm::mach_vm_read_overwrite;
        use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

        let mut got: mach_vm_size_t = 0;
        // SAFETY: buf is a unique mut slice; addr is treated as
        // an opaque integer by the kernel; got is an out-pointer.
        let kr = unsafe {
            mach_vm_read_overwrite(
                self.task,
                addr as mach_vm_address_t,
                buf.len() as mach_vm_size_t,
                buf.as_mut_ptr() as mach_vm_address_t,
                &mut got,
            )
        };
        kr == KERN_SUCCESS && got as usize == buf.len()
    }

    /// Convenience: 8-byte aligned u64 read. The unwinder calls
    /// this for nearly every CFA / saved-register lookup, so
    /// optimising it later (vmap a stack window once per sample
    /// instead of one syscall per quad) is on the table.
    #[allow(dead_code)] // wired in the periodic-walker commit
    pub fn read_u64(&self, addr: u64) -> Option<u64> {
        let mut buf = [0u8; 8];
        if self.read_exact(addr, &mut buf) {
            Some(u64::from_le_bytes(buf))
        } else {
            None
        }
    }
}

/// Pull the ARM64 register state for one thread of `task`. The
/// thread must already be suspended (or in a state where the
/// kernel will return a coherent register set — a thread on its
/// own kernel stack returns the user-space state at the syscall
/// boundary, which is what we want for a profiler).
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)] // wired in the periodic-walker commit
pub fn thread_state_arm64(thread: mach_port_t) -> Option<mach2::structs::arm_thread_state64_t> {
    use mach2::structs::arm_thread_state64_t;
    use mach2::thread_act::thread_get_state;
    use mach2::thread_status::ARM_THREAD_STATE64;

    let mut state: arm_thread_state64_t = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<arm_thread_state64_t>() / std::mem::size_of::<u32>())
        as mach2::message::mach_msg_type_number_t;
    // SAFETY: state is a fresh zeroed struct of the right size;
    // count is set to its u32-word length per the Mach contract.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_THREAD_STATE64,
            (&mut state) as *mut _ as *mut u32,
            &mut count,
        )
    };
    if kr == KERN_SUCCESS {
        Some(state)
    } else {
        None
    }
}

/// Placeholder for the future periodic walker. Currently
/// constructs the unwinder type so downstream code knows the
/// shape (`UnwinderAarch64`); does not yet load module unwind
/// sections, so calling it would produce an empty stack — the
/// caller is expected to plumb in a real `ModuleProvider` first.
///
/// Lives here so the next commit (target-image enumeration via
/// `dyld_all_image_infos`) has a clear seam to slot into.
#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
pub fn build_unwinder() -> framehop::aarch64::UnwinderAarch64<Vec<u8>> {
    framehop::aarch64::UnwinderAarch64::new()
}
