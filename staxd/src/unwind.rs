//! Minimal framehop wiring used by the race-against-return probe.
//!
//! Lives in staxd because that's the only process with the target's
//! Mach task port (the unprivileged client side has no way to call
//! `task_for_pid`). The output is raw return addresses — symbol
//! resolution happens client-side, in the same pipeline that
//! symbolicates kperf samples (`stax-mac-kperf-parse` →
//! BinaryRegistry). One symbolicator, one demangler, one resolver.
//!
//! Mirrors the shape of `stax-shade::walker` but pulled in here so
//! staxd doesn't depend on stax-shade.

#![cfg(target_os = "macos")]

use framehop::aarch64::{CacheAarch64, UnwinderAarch64};
use mach2::kern_return::KERN_SUCCESS;
use mach2::port::mach_port_t;

/// Memory accessor for framehop. `mach_port_t` is a u32 — copying
/// it doesn't duplicate the underlying right.
#[derive(Copy, Clone)]
pub struct MachStackReader {
    pub task: mach_port_t,
}

impl MachStackReader {
    fn read_exact(&self, addr: u64, buf: &mut [u8]) -> bool {
        use mach2::vm::mach_vm_read_overwrite;
        use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

        let mut got: mach_vm_size_t = 0;
        // SAFETY: buf is a unique mut slice; addr is opaque integer
        // to the kernel; got is an out-pointer.
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

    fn read_u64(&self, addr: u64) -> Option<u64> {
        let mut buf = [0u8; 8];
        if self.read_exact(addr, &mut buf) {
            Some(u64::from_le_bytes(buf))
        } else {
            None
        }
    }
}

/// Snapshot of the target's loaded modules + the framehop unwinder
/// built from them. Owned by the probe worker for the lifetime of
/// the session.
pub struct TargetUnwinder {
    pub unwinder: UnwinderAarch64<Vec<u8>>,
    pub cache: CacheAarch64,
    pub reader: MachStackReader,
    pub stats: UnwinderStats,
}

#[derive(Default, Debug)]
pub struct UnwinderStats {
    pub images_total: usize,
    pub modules_added: usize,
    pub with_unwind_info: usize,
    pub with_eh_frame: usize,
}

/// Enumerate target images via stax-target-images and build a
/// framehop unwinder. Returns `None` if image enumeration failed
/// outright (probe falls back to a plain FP walk in that case).
pub fn build(task: mach_port_t) -> Option<TargetUnwinder> {
    use framehop::Unwinder;
    use framehop::{ExplicitModuleSectionInfo, Module};

    let walker = stax_target_images::TargetImageWalker::new(task);
    let images = match walker.enumerate() {
        Ok(images) => images,
        Err(e) => {
            tracing::warn!("probe: dyld walk failed: {e}");
            return None;
        }
    };

    let mut unwinder: UnwinderAarch64<Vec<u8>> = UnwinderAarch64::new();
    let mut stats = UnwinderStats {
        images_total: images.len(),
        ..Default::default()
    };

    for img in images {
        let Some(sections) = img.sections else { continue };
        let Some(text_avma) = sections.text_avma.clone() else {
            continue;
        };
        let text_svma = sections.avma_to_svma(&text_avma);
        let base_svma = text_svma.start;

        let got_svma = sections
            .got_avma
            .as_ref()
            .map(|r| sections.avma_to_svma(r));
        let eh_frame_svma = sections
            .eh_frame
            .as_ref()
            .map(|s| sections.avma_to_svma(&s.avma));
        let eh_frame_hdr_svma = sections
            .eh_frame_hdr
            .as_ref()
            .map(|s| sections.avma_to_svma(&s.avma));

        let info = ExplicitModuleSectionInfo {
            base_svma,
            text_segment_svma: Some(text_svma.clone()),
            got_svma,
            unwind_info: sections.unwind_info.map(|s| s.bytes),
            eh_frame_svma,
            eh_frame: sections.eh_frame.map(|s| s.bytes),
            eh_frame_hdr_svma,
            eh_frame_hdr: sections.eh_frame_hdr.map(|s| s.bytes),
            ..Default::default()
        };
        let has_unwind = info.unwind_info.is_some();
        let has_eh_frame = info.eh_frame.is_some();

        let module = Module::new(img.path, text_avma, img.load_address, info);
        unwinder.add_module(module);
        stats.modules_added += 1;
        if has_unwind {
            stats.with_unwind_info += 1;
        }
        if has_eh_frame {
            stats.with_eh_frame += 1;
        }
    }

    Some(TargetUnwinder {
        unwinder,
        cache: CacheAarch64::new(),
        reader: MachStackReader { task },
        stats,
    })
}

/// Walk a single thread's stack via framehop. `pc/lr/sp/fp` come
/// from a fresh `thread_get_state(ARM_THREAD_STATE64)` — caller is
/// responsible for having the thread suspended. Returns the list
/// of return addresses framehop produced (PAC-bearing — strip at
/// the call site if needed).
///
/// The leaf PC is *not* included in the returned list (matches FP
/// walk shape). framehop's first frame is the caller of `pc`,
/// equivalent to the saved LR.
pub fn walk(tu: &mut TargetUnwinder, pc: u64, lr: u64, sp: u64, fp: u64, max: usize) -> Vec<u64> {
    use framehop::Unwinder;
    use framehop::aarch64::UnwindRegsAarch64;

    let reader = tu.reader;
    let mut read_stack = |addr: u64| reader.read_u64(addr).ok_or(());

    let mut iter = tu.unwinder.iter_frames(
        pc,
        UnwindRegsAarch64::new(lr, sp, fp),
        &mut tu.cache,
        &mut read_stack,
    );

    let mut frames: Vec<u64> = Vec::with_capacity(max);
    let mut first = true;
    loop {
        match iter.next() {
            Ok(Some(frame)) => {
                // framehop's first frame is the leaf PC itself.
                // Skip it so the returned list is "return addresses
                // only" — matches the FP-walk shape used elsewhere.
                if first {
                    first = false;
                    continue;
                }
                frames.push(frame.address());
                if frames.len() >= max {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    frames
}
