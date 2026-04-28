//! Thin wrapper over the `object` crate for the Mach-O fields
//! `BinaryData::load` needs (architecture, endianness, bitness, segments,
//! UUID). Symbols and DWARF are not handled here — the macOS recorder
//! writes pre-resolved `MachOSymbolTable` packets into the archive, and
//! source-level DWARF lives in `.dSYM` bundles which annotate doesn't yet
//! consume.

use std::io;

use object::macho;
use object::read::macho::{FatArch, FatHeader, MachHeader, MachOFile64};
use object::{Object, ObjectKind, ObjectSegment};

use crate::types::{Bitness, Endianness};

#[derive(Debug)]
pub struct Segment {
    pub vmaddr: u64,
    pub vmsize: u64,
    pub fileoff: u64,
    pub filesize: u64,
    pub is_readable: bool,
    pub is_writable: bool,
    pub is_executable: bool,
}

pub struct MachO {
    pub architecture: &'static str,
    pub endianness: Endianness,
    pub bitness: Bitness,
    pub is_shared_object: bool,
    pub segments: Vec<Segment>,
    pub uuid: Option<[u8; 16]>,
}

const HOST_CPUTYPE: u32 = if cfg!(target_arch = "aarch64") {
    macho::CPU_TYPE_ARM64
} else if cfg!(target_arch = "x86_64") {
    macho::CPU_TYPE_X86_64
} else {
    0
};

/// Return the host-arch slice of a Mach-O blob. If `bytes` starts with
/// a fat magic, find the slice matching the host cputype and return it;
/// otherwise return `bytes` unchanged. Used by addr2line context
/// construction so callers don't choke on `cafebabe` headers (e.g.
/// `/usr/lib/dyld` is shipped as a fat binary on Apple Silicon and
/// `object::File::parse` can't parse the fat wrapper directly).
pub fn host_thin_slice(bytes: &[u8]) -> io::Result<&[u8]> {
    let magic = match magic_be(bytes) {
        Some(m) => m,
        None => return Ok(bytes),
    };
    match magic {
        macho::FAT_MAGIC | macho::FAT_CIGAM => {
            let arches = FatHeader::parse_arch32(bytes).map_err(other)?;
            let arch = arches
                .iter()
                .find(|a| a.cputype() == HOST_CPUTYPE)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Mach-O: fat archive has no slice for host cputype 0x{:x}",
                            HOST_CPUTYPE
                        ),
                    )
                })?;
            arch.data(bytes).map_err(other)
        }
        macho::FAT_MAGIC_64 | macho::FAT_CIGAM_64 => {
            let arches = FatHeader::parse_arch64(bytes).map_err(other)?;
            let arch = arches
                .iter()
                .find(|a| a.cputype() == HOST_CPUTYPE)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Mach-O: fat archive has no slice for host cputype 0x{:x}",
                            HOST_CPUTYPE
                        ),
                    )
                })?;
            arch.data(bytes).map_err(other)
        }
        _ => Ok(bytes),
    }
}

/// True if `bytes` starts with a Mach-O magic we recognize (thin 64-bit or
/// any flavor of fat).
pub fn is_macho(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    let m = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    matches!(
        m,
        macho::MH_MAGIC_64
            | macho::MH_CIGAM_64
            | macho::FAT_MAGIC
            | macho::FAT_CIGAM
            | macho::FAT_MAGIC_64
            | macho::FAT_CIGAM_64
    )
}

fn magic_be(bytes: &[u8]) -> Option<u32> {
    bytes
        .get(0..4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parse `blob`, transparently selecting the host-arch slice from a fat
/// archive. We pick the host cputype because `BinaryData::load_from_fs` is
/// always called on the same host as the binary it inspects.
pub fn parse(blob: &[u8]) -> io::Result<MachO> {
    parse_thin(host_thin_slice(blob)?)
}

fn parse_thin(blob: &[u8]) -> io::Result<MachO> {
    // 32-bit Mach-O is unsupported — modern macOS is 64-bit only.
    let header = macho::MachHeader64::<object::Endianness>::parse(blob, 0).map_err(other)?;
    let endian = header.endian().map_err(other)?;
    let file = MachOFile64::<'_, object::Endianness>::parse(blob).map_err(other)?;

    let architecture = match header.cputype(endian) {
        macho::CPU_TYPE_X86_64 => "amd64",
        macho::CPU_TYPE_ARM64 => "aarch64",
        cputype => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Mach-O: unsupported cputype 0x{:x}", cputype),
            ))
        }
    };

    // PIE executables get is_shared_object=false; their slide is recovered
    // from the runtime memory mapping by the analysis pipeline.
    let is_shared_object = !matches!(file.kind(), ObjectKind::Executable);

    let mut segments = Vec::new();
    for seg in file.segments() {
        let (fileoff, filesize) = seg.file_range();
        let prot = seg.flags();
        let initprot = match prot {
            object::SegmentFlags::MachO { initprot, .. } => initprot,
            _ => 0,
        };
        segments.push(Segment {
            vmaddr: seg.address(),
            vmsize: seg.size(),
            fileoff,
            filesize,
            is_readable: initprot & macho::VM_PROT_READ != 0,
            is_writable: initprot & macho::VM_PROT_WRITE != 0,
            is_executable: initprot & macho::VM_PROT_EXECUTE != 0,
        });
    }

    let uuid = file.mach_uuid().map_err(other)?;

    Ok(MachO {
        architecture,
        endianness: Endianness::LittleEndian,
        bitness: Bitness::B64,
        is_shared_object,
        segments,
        uuid,
    })
}

fn other(err: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("Mach-O: {}", err))
}
