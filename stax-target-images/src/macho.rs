//! Hand-rolled Mach-O 64-bit parser, scoped to the bits a
//! framehop unwinder cares about. Reads through a Mach task
//! port — the in-memory image's header + load commands — and
//! locates `__TEXT`, `__unwind_info`, `__compact_unwind`,
//! `__eh_frame`, `__eh_frame_hdr`, `__got`, plus `LC_UUID`.
//!
//! Why hand-roll rather than pull in `object`: the parsing
//! scope is tiny (one struct per load command we care about,
//! one section iteration), the layouts are stable, and avoiding
//! an `object` dep keeps `stax-target-images` independent of
//! whatever version churn happens in the rest of the workspace.
//!
//! All struct layouts mirror Apple's `<mach-o/loader.h>`.

use std::ops::Range;

use crate::dyld::WalkError;
use mach2::kern_return::KERN_SUCCESS;
use mach2::port::mach_port_t;
use mach2::vm::mach_vm_read_overwrite;
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

/// One parsed section: where it lives in the target's address
/// space (AVMA) plus its bytes. framehop's
/// `ExplicitModuleSectionInfo` wants both for DWARF CFI (it
/// needs the svma to resolve eh_frame-relative addresses), so
/// we keep them paired.
#[derive(Clone, Debug)]
pub struct SectionData {
    pub avma: Range<u64>,
    pub bytes: Vec<u8>,
}

/// What we extract from one image. AVMA ranges are runtime
/// (already slid). Section bytes for the small unwind sections
/// are eagerly read because the unwinder needs them on every
/// walk; `__TEXT` and `__got` only get their AVMA ranges
/// recorded — callers that need bytes for disassembly /
/// register lookup can `mach_vm_read` on demand.
#[derive(Clone, Debug, Default)]
pub struct MachoSections {
    pub uuid: Option<[u8; 16]>,
    /// `runtime_load_address - first_segment.vmaddr`. Same value
    /// for every section in the image; we keep it on the struct
    /// so consumers can translate svma↔avma without re-parsing.
    pub slide: i64,
    /// AVMA range of the `__TEXT` segment.
    pub text_avma: Option<Range<u64>>,
    /// AVMA range of the `__got` section (PC-relative addressing
    /// hints used by some unwind tables).
    pub got_avma: Option<Range<u64>>,
    /// `__unwind_info` (Apple's compact-unwind format — what
    /// `__LINKEDIT/__unwind_info` carries on Apple Silicon).
    pub unwind_info: Option<SectionData>,
    /// `__compact_unwind` (older format, still emitted by some
    /// older toolchains).
    pub compact_unwind: Option<SectionData>,
    /// `__eh_frame` (DWARF unwind, the GCC/clang fallback).
    pub eh_frame: Option<SectionData>,
    /// `__eh_frame_hdr` (binary search index over eh_frame).
    pub eh_frame_hdr: Option<SectionData>,
}

impl MachoSections {
    /// Translate an AVMA range to its SVMA counterpart using the
    /// recorded slide. SVMA = AVMA - slide. Used at unwinder
    /// build time to feed framehop, which keys everything by
    /// SVMA.
    pub fn avma_to_svma(&self, range: &Range<u64>) -> Range<u64> {
        let start = (range.start as i64).wrapping_sub(self.slide) as u64;
        let end = (range.end as i64).wrapping_sub(self.slide) as u64;
        start..end
    }
}

/// Parse the Mach-O image mapped at `load_address` in `task`.
/// Returns `None` (with a debug log) for non-Mach-O magic, fat
/// binaries (we'd need to pick a slice — Apple Silicon processes
/// load thin slices into memory, so this should never happen),
/// or unreadable headers.
pub fn parse_image(task: mach_port_t, load_address: u64) -> Option<MachoSections> {
    let mut header_bytes = [0u8; std::mem::size_of::<MachHeader64>()];
    if !read_exact(task, load_address, &mut header_bytes) {
        tracing::debug!(
            load_address = format!("{load_address:#x}"),
            "macho: header read failed"
        );
        return None;
    }
    // SAFETY: header_bytes is the exact size of MachHeader64 and
    // the layout is repr(C). Reading through a packed copy is the
    // standard pattern for unaligned bytes-from-the-wire.
    let header: MachHeader64 = unsafe { std::ptr::read_unaligned(header_bytes.as_ptr().cast()) };
    if header.magic != MH_MAGIC_64 {
        tracing::debug!(
            load_address = format!("{load_address:#x}"),
            magic = format!("{:#x}", header.magic),
            "macho: not a 64-bit Mach-O"
        );
        return None;
    }

    let cmds_len = header.sizeofcmds as usize;
    let mut cmds = vec![0u8; cmds_len];
    let cmds_addr = load_address + std::mem::size_of::<MachHeader64>() as u64;
    if !read_exact(task, cmds_addr, &mut cmds) {
        tracing::debug!(
            load_address = format!("{load_address:#x}"),
            cmds_len,
            "macho: load-commands read failed"
        );
        return None;
    }

    let mut out = MachoSections::default();
    // Section names we care about, paired with which field on
    // MachoSections to populate. Bytes get read separately
    // after the header walk so we can do them all under the same
    // task port.
    let mut wanted: Vec<(&'static [u8], BytesSlot)> = vec![
        (b"__unwind_info\0\0\0", BytesSlot::UnwindInfo),
        (b"__compact_unwind", BytesSlot::CompactUnwind),
        (b"__eh_frame\0\0\0\0\0\0", BytesSlot::EhFrame),
        (b"__eh_frame_hdr\0\0", BytesSlot::EhFrameHdr),
    ];
    // Pending section reads: (avma, len, BytesSlot).
    let mut pending: Vec<(u64, u64, BytesSlot)> = Vec::new();

    let mut cursor = 0usize;
    for _ in 0..header.ncmds {
        if cursor + std::mem::size_of::<LoadCommand>() > cmds.len() {
            break;
        }
        // SAFETY: bounds checked above.
        let lc: LoadCommand = unsafe { std::ptr::read_unaligned(cmds.as_ptr().add(cursor).cast()) };
        let lc_size = lc.cmdsize as usize;
        if lc_size == 0 || cursor + lc_size > cmds.len() {
            break;
        }
        match lc.cmd {
            LC_SEGMENT_64 => {
                if cursor + std::mem::size_of::<SegmentCommand64>() > cmds.len() {
                    break;
                }
                let seg: SegmentCommand64 =
                    unsafe { std::ptr::read_unaligned(cmds.as_ptr().add(cursor).cast()) };
                // Compute the slide off __TEXT, not the first
                // segment. Main executables on macOS lead with
                // __PAGEZERO (vmaddr=0, vmsize=4GB) — basing the
                // slide on it would give `slide = load_address`,
                // which then projects all subsequent sections way
                // above where they're actually mapped. The Mach-O
                // header lives at the start of __TEXT, so
                // `slide = load_address - __TEXT.vmaddr` is the
                // identity that actually holds for every kind of
                // image (main exec, dylib, dyld-shared-cache slot).
                if name_eq(&seg.segname, b"__TEXT") {
                    out.slide = (load_address as i64).wrapping_sub(seg.vmaddr as i64);
                    let start = (seg.vmaddr as i64).wrapping_add(out.slide) as u64;
                    out.text_avma = Some(start..start + seg.vmsize);
                }
                let mut sect_off = cursor + std::mem::size_of::<SegmentCommand64>();
                for _ in 0..seg.nsects {
                    if sect_off + std::mem::size_of::<Section64>() > cmds.len() {
                        break;
                    }
                    let sect: Section64 =
                        unsafe { std::ptr::read_unaligned(cmds.as_ptr().add(sect_off).cast()) };
                    let avma_start = (sect.addr as i64).wrapping_add(out.slide) as u64;
                    if name_eq(&sect.sectname, b"__got") {
                        out.got_avma = Some(avma_start..avma_start + sect.size);
                    }
                    if let Some(slot) = match_wanted(&mut wanted, &sect.sectname) {
                        pending.push((avma_start, sect.size, slot));
                    }
                    sect_off += std::mem::size_of::<Section64>();
                }
            }
            LC_UUID => {
                if cursor + std::mem::size_of::<UuidCommand>() <= cmds.len() {
                    let u: UuidCommand =
                        unsafe { std::ptr::read_unaligned(cmds.as_ptr().add(cursor).cast()) };
                    out.uuid = Some(u.uuid);
                }
            }
            _ => {}
        }
        cursor += lc_size;
    }

    // Now drain pending section reads. Bound the per-section
    // size to a sanity cap (8 MiB) — eh_frame in pathological
    // binaries can get large, but anything past that is more
    // likely a corrupt header than legitimate.
    const SECTION_SIZE_CAP: u64 = 8 * 1024 * 1024;
    for (avma, size, slot) in pending {
        if size == 0 || size > SECTION_SIZE_CAP {
            continue;
        }
        let mut buf = vec![0u8; size as usize];
        if read_exact(task, avma, &mut buf) {
            slot.assign(
                &mut out,
                SectionData {
                    avma: avma..avma + size,
                    bytes: buf,
                },
            );
        } else {
            tracing::debug!(
                avma = format!("{avma:#x}"),
                size,
                ?slot,
                "macho: section read failed"
            );
        }
    }

    Some(out)
}

#[derive(Copy, Clone, Debug)]
enum BytesSlot {
    UnwindInfo,
    CompactUnwind,
    EhFrame,
    EhFrameHdr,
}

impl BytesSlot {
    fn assign(self, dst: &mut MachoSections, data: SectionData) {
        match self {
            BytesSlot::UnwindInfo => dst.unwind_info = Some(data),
            BytesSlot::CompactUnwind => dst.compact_unwind = Some(data),
            BytesSlot::EhFrame => dst.eh_frame = Some(data),
            BytesSlot::EhFrameHdr => dst.eh_frame_hdr = Some(data),
        }
    }
}

fn match_wanted(
    wanted: &mut Vec<(&'static [u8], BytesSlot)>,
    sectname: &[u8; 16],
) -> Option<BytesSlot> {
    let pos = wanted.iter().position(|(n, _)| name_eq(sectname, n))?;
    Some(wanted.remove(pos).1)
}

fn name_eq(field: &[u8; 16], wanted: &[u8]) -> bool {
    // Section / segment names are NUL-padded to 16 bytes. Caller
    // can pass the wanted name with or without trailing NULs.
    let wanted = match wanted.iter().position(|&b| b == 0) {
        Some(n) => &wanted[..n],
        None => wanted,
    };
    let actual_len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..actual_len] == wanted
}

fn read_exact(task: mach_port_t, addr: u64, buf: &mut [u8]) -> bool {
    let mut got: mach_vm_size_t = 0;
    // SAFETY: `buf` is a unique mut slice; addr is opaque to
    // the kernel.
    let kr = unsafe {
        mach_vm_read_overwrite(
            task,
            addr as mach_vm_address_t,
            buf.len() as mach_vm_size_t,
            buf.as_mut_ptr() as mach_vm_address_t,
            &mut got,
        )
    };
    kr == KERN_SUCCESS && got as usize == buf.len()
}

#[allow(dead_code)] // surfaced through public API as a generic kr-conversion helper
fn err_for_kr(kr: i32, what: &'static str, addr: u64, len: usize) -> WalkError {
    WalkError::MachVmRead {
        what,
        addr,
        len,
        kr,
    }
}

// ---------------------------------------------------------------------------
// Mach-O 64-bit layouts (mirror of <mach-o/loader.h>).
// ---------------------------------------------------------------------------

const MH_MAGIC_64: u32 = 0xFEEDFACF;
const LC_SEGMENT_64: u32 = 0x19;
const LC_UUID: u32 = 0x1B;

#[repr(C)]
#[derive(Copy, Clone)]
struct MachHeader64 {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct LoadCommand {
    cmd: u32,
    cmdsize: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct SegmentCommand64 {
    cmd: u32,
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Section64 {
    sectname: [u8; 16],
    segname: [u8; 16],
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    reloff: u32,
    nreloc: u32,
    flags: u32,
    reserved1: u32,
    reserved2: u32,
    reserved3: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UuidCommand {
    cmd: u32,
    cmdsize: u32,
    uuid: [u8; 16],
}
