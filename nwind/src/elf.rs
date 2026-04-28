//! Thin ELF parser that the rest of nwind layers on top of `byteorder`.
//!
//! `binary.rs` and `symbols.rs` care about a small subset of the ELF
//! format: section/program headers, symbol tables, the gnu build-id note,
//! and a few well-known section names. We collapse 32-bit and 64-bit
//! into a single set of widened structs (everything to `u64`).
//!
//! `byteorder` is used directly because input slices (e.g. `include_bytes!`
//! in tests) aren't guaranteed to be 8-byte aligned, which rules out the
//! `object::pod::from_bytes` fast-cast path.
//!
//! ELF constants come from the `object::elf` module.

use std::ops::Range;

use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use object::elf::{self, ELFCLASS32, ELFCLASS64, ELFDATA2LSB, ELFDATA2MSB, SHT_STRTAB};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

#[allow(dead_code)] // every field is parsed for completeness; only some are read.
#[derive(Debug)]
pub struct Header {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct SectionHeader {
    pub sh_name: usize,
    pub sh_type: u32,
    pub sh_flags: u64,
    pub sh_addr: u64,
    pub sh_offset: u64,
    pub sh_size: u64,
    pub sh_link: u32,
    pub sh_info: u32,
    pub sh_addralign: u64,
    pub sh_entsize: u64,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

impl ProgramHeader {
    #[inline]
    pub fn is_read(&self) -> bool {
        self.p_flags & elf::PF_R != 0
    }
    #[inline]
    pub fn is_write(&self) -> bool {
        self.p_flags & elf::PF_W != 0
    }
    #[inline]
    pub fn is_executable(&self) -> bool {
        self.p_flags & elf::PF_X != 0
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct Sym {
    pub st_name: usize,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: usize,
    pub st_value: u64,
    pub st_size: u64,
}

impl Sym {
    /// Mirrors goblin's `Sym::is_function` — STT_FUNC in the lower 4 bits.
    #[inline]
    pub fn is_function(&self) -> bool {
        (self.st_info & 0x0f) == elf::STT_FUNC
    }
}

/// Thin string-table view over the bytes of a SHT_STRTAB section.
#[derive(Clone, Copy)]
pub struct Strtab<'a> {
    bytes: &'a [u8],
}

impl<'a> Strtab<'a> {
    #[inline]
    pub fn new(bytes: &'a [u8], _: u8) -> Self {
        Strtab { bytes }
    }

    #[inline]
    pub fn get(&self, offset: usize) -> Option<Result<&'a str, std::str::Utf8Error>> {
        if offset >= self.bytes.len() {
            return None;
        }
        let tail = &self.bytes[offset..];
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        Some(std::str::from_utf8(&tail[..end]))
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct Note<'a> {
    pub name: &'a [u8],
    pub n_type: u32,
    pub desc: &'a [u8],
}

/// Parsed ELF view over a byte slice. Endianness and bitness are
/// resolved at parse time and stored as plain fields, so call sites
/// don't need to dispatch over an enum.
pub struct Elf<'a> {
    bytes: &'a [u8],
    header: Header,
    endianness: Endian,
    is_64: bool,
}

impl<'a> Elf<'a> {
    #[inline]
    pub fn is_64_bit(&self) -> bool {
        self.is_64
    }

    #[inline]
    pub fn endianness(&self) -> Endian {
        self.endianness
    }

    #[inline]
    pub fn header(&self) -> &Header {
        &self.header
    }

    #[inline]
    pub fn section_headers(&self) -> SectionHeaderIter<'a> {
        let header = &self.header;
        SectionHeaderIter {
            bytes: self.bytes,
            offset: header.e_shoff as usize,
            stride: header.e_shentsize as usize,
            remaining: header.e_shnum as usize,
            endianness: self.endianness,
            is_64: self.is_64,
        }
    }

    #[inline]
    pub fn program_headers(&self) -> ProgramHeaderIter<'a> {
        let header = &self.header;
        ProgramHeaderIter {
            bytes: self.bytes,
            offset: header.e_phoff as usize,
            stride: header.e_phentsize as usize,
            remaining: header.e_phnum as usize,
            endianness: self.endianness,
            is_64: self.is_64,
        }
    }

    #[inline]
    pub fn get_section_header(&self, index: usize) -> Option<SectionHeader> {
        let header = &self.header;
        if index >= header.e_shnum as usize {
            return None;
        }
        let offset = header.e_shoff as usize + index * header.e_shentsize as usize;
        read_section_header(self.bytes, offset, self.endianness, self.is_64)
    }

    #[inline]
    pub fn get_section_body(&self, header: &SectionHeader) -> &'a [u8] {
        let start = header.sh_offset as usize;
        let end = start + header.sh_size as usize;
        &self.bytes[start..end]
    }

    #[inline]
    pub fn get_section_body_range(&self, header: &SectionHeader) -> Range<u64> {
        header.sh_offset..header.sh_offset + header.sh_size
    }

    #[inline]
    pub fn get_strtab(&self, header: &SectionHeader) -> Option<Strtab<'a>> {
        if header.sh_type != SHT_STRTAB {
            return None;
        }
        Some(Strtab::new(self.get_section_body(header), 0))
    }

    pub fn parse_note(&self, data: &'a [u8]) -> Option<Note<'a>> {
        // Layout: u32 namesz, u32 descsz, u32 type, name[namesz padded
        // to 4], desc[descsz padded to 4]. The build-id note is always
        // 4-byte aligned in the wild even on 64-bit binaries.
        if data.len() < 12 {
            return None;
        }
        let mut cur = data;
        let namesz = read_u32(&mut cur, self.endianness)? as usize;
        let descsz = read_u32(&mut cur, self.endianness)? as usize;
        let n_type = read_u32(&mut cur, self.endianness)?;
        let header_len: usize = 12;

        let name_start = header_len;
        let name_end = name_start.checked_add(namesz)?;
        let name = data.get(name_start..name_end)?;
        let name = name.split(|&b| b == 0).next()?;

        let aligned_namesz = align_up(namesz, 4);
        let desc_start = header_len.checked_add(aligned_namesz)?;
        let desc_end = desc_start.checked_add(descsz)?;
        let desc = data.get(desc_start..desc_end)?;

        Some(Note { name, n_type, desc })
    }
}

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[inline]
fn read_u16(bytes: &mut &[u8], endian: Endian) -> Option<u16> {
    Some(match endian {
        Endian::Little => bytes.read_u16::<LittleEndian>().ok()?,
        Endian::Big => bytes.read_u16::<BigEndian>().ok()?,
    })
}

#[inline]
fn read_u32(bytes: &mut &[u8], endian: Endian) -> Option<u32> {
    Some(match endian {
        Endian::Little => bytes.read_u32::<LittleEndian>().ok()?,
        Endian::Big => bytes.read_u32::<BigEndian>().ok()?,
    })
}

#[inline]
fn read_u64(bytes: &mut &[u8], endian: Endian) -> Option<u64> {
    Some(match endian {
        Endian::Little => bytes.read_u64::<LittleEndian>().ok()?,
        Endian::Big => bytes.read_u64::<BigEndian>().ok()?,
    })
}

#[inline]
fn read_u8(bytes: &mut &[u8]) -> Option<u8> {
    bytes.read_u8().ok()
}

fn read_section_header(
    src: &[u8],
    offset: usize,
    endian: Endian,
    is_64: bool,
) -> Option<SectionHeader> {
    let len = if is_64 { 64 } else { 40 };
    let mut buf = src.get(offset..offset + len)?;
    if is_64 {
        let sh_name = read_u32(&mut buf, endian)? as usize;
        let sh_type = read_u32(&mut buf, endian)?;
        let sh_flags = read_u64(&mut buf, endian)?;
        let sh_addr = read_u64(&mut buf, endian)?;
        let sh_offset = read_u64(&mut buf, endian)?;
        let sh_size = read_u64(&mut buf, endian)?;
        let sh_link = read_u32(&mut buf, endian)?;
        let sh_info = read_u32(&mut buf, endian)?;
        let sh_addralign = read_u64(&mut buf, endian)?;
        let sh_entsize = read_u64(&mut buf, endian)?;
        Some(SectionHeader {
            sh_name,
            sh_type,
            sh_flags,
            sh_addr,
            sh_offset,
            sh_size,
            sh_link,
            sh_info,
            sh_addralign,
            sh_entsize,
        })
    } else {
        let sh_name = read_u32(&mut buf, endian)? as usize;
        let sh_type = read_u32(&mut buf, endian)?;
        let sh_flags = read_u32(&mut buf, endian)? as u64;
        let sh_addr = read_u32(&mut buf, endian)? as u64;
        let sh_offset = read_u32(&mut buf, endian)? as u64;
        let sh_size = read_u32(&mut buf, endian)? as u64;
        let sh_link = read_u32(&mut buf, endian)?;
        let sh_info = read_u32(&mut buf, endian)?;
        let sh_addralign = read_u32(&mut buf, endian)? as u64;
        let sh_entsize = read_u32(&mut buf, endian)? as u64;
        Some(SectionHeader {
            sh_name,
            sh_type,
            sh_flags,
            sh_addr,
            sh_offset,
            sh_size,
            sh_link,
            sh_info,
            sh_addralign,
            sh_entsize,
        })
    }
}

fn read_program_header(
    src: &[u8],
    offset: usize,
    endian: Endian,
    is_64: bool,
) -> Option<ProgramHeader> {
    let len = if is_64 { 56 } else { 32 };
    let mut buf = src.get(offset..offset + len)?;
    if is_64 {
        let p_type = read_u32(&mut buf, endian)?;
        let p_flags = read_u32(&mut buf, endian)?;
        let p_offset = read_u64(&mut buf, endian)?;
        let p_vaddr = read_u64(&mut buf, endian)?;
        let p_paddr = read_u64(&mut buf, endian)?;
        let p_filesz = read_u64(&mut buf, endian)?;
        let p_memsz = read_u64(&mut buf, endian)?;
        let p_align = read_u64(&mut buf, endian)?;
        Some(ProgramHeader {
            p_type,
            p_flags,
            p_offset,
            p_vaddr,
            p_paddr,
            p_filesz,
            p_memsz,
            p_align,
        })
    } else {
        // 32-bit Phdr: type, offset, vaddr, paddr, filesz, memsz, flags, align.
        let p_type = read_u32(&mut buf, endian)?;
        let p_offset = read_u32(&mut buf, endian)? as u64;
        let p_vaddr = read_u32(&mut buf, endian)? as u64;
        let p_paddr = read_u32(&mut buf, endian)? as u64;
        let p_filesz = read_u32(&mut buf, endian)? as u64;
        let p_memsz = read_u32(&mut buf, endian)? as u64;
        let p_flags = read_u32(&mut buf, endian)?;
        let p_align = read_u32(&mut buf, endian)? as u64;
        Some(ProgramHeader {
            p_type,
            p_flags,
            p_offset,
            p_vaddr,
            p_paddr,
            p_filesz,
            p_memsz,
            p_align,
        })
    }
}

pub struct SectionHeaderIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    stride: usize,
    remaining: usize,
    endianness: Endian,
    is_64: bool,
}

impl<'a> Iterator for SectionHeaderIter<'a> {
    type Item = SectionHeader;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let header = read_section_header(self.bytes, self.offset, self.endianness, self.is_64)?;
        self.offset += self.stride;
        self.remaining -= 1;
        Some(header)
    }
}

pub struct ProgramHeaderIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    stride: usize,
    remaining: usize,
    endianness: Endian,
    is_64: bool,
}

impl<'a> Iterator for ProgramHeaderIter<'a> {
    type Item = ProgramHeader;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let header = read_program_header(self.bytes, self.offset, self.endianness, self.is_64)?;
        self.offset += self.stride;
        self.remaining -= 1;
        Some(header)
    }
}

/// Iterator over either Elf32 or Elf64 symbol entries, picked by
/// `is_64` at construction time.
pub struct SymIter<'a> {
    bytes: &'a [u8],
    endianness: Endian,
    is_64: bool,
}

impl<'a> SymIter<'a> {
    #[inline]
    pub fn new(bytes: &'a [u8], endianness: Endian, is_64: bool) -> Self {
        SymIter {
            bytes,
            endianness,
            is_64,
        }
    }
}

impl<'a> Iterator for SymIter<'a> {
    type Item = Sym;
    fn next(&mut self) -> Option<Self::Item> {
        if self.is_64 {
            if self.bytes.len() < 24 {
                return None;
            }
            let mut buf = &self.bytes[..24];
            let st_name = read_u32(&mut buf, self.endianness)? as usize;
            let st_info = read_u8(&mut buf)?;
            let st_other = read_u8(&mut buf)?;
            let st_shndx = read_u16(&mut buf, self.endianness)? as usize;
            let st_value = read_u64(&mut buf, self.endianness)?;
            let st_size = read_u64(&mut buf, self.endianness)?;
            self.bytes = &self.bytes[24..];
            Some(Sym {
                st_name,
                st_info,
                st_other,
                st_shndx,
                st_value,
                st_size,
            })
        } else {
            if self.bytes.len() < 16 {
                return None;
            }
            let mut buf = &self.bytes[..16];
            let st_name = read_u32(&mut buf, self.endianness)? as usize;
            let st_value = read_u32(&mut buf, self.endianness)? as u64;
            let st_size = read_u32(&mut buf, self.endianness)? as u64;
            let st_info = read_u8(&mut buf)?;
            let st_other = read_u8(&mut buf)?;
            let st_shndx = read_u16(&mut buf, self.endianness)? as usize;
            self.bytes = &self.bytes[16..];
            Some(Sym {
                st_name,
                st_info,
                st_other,
                st_shndx,
                st_value,
                st_size,
            })
        }
    }
}

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;

pub fn parse(bytes: &[u8]) -> Result<Elf<'_>, &'static str> {
    if bytes.len() < 16 {
        return Err("not an ELF file");
    }
    let endianness = match bytes[EI_DATA] {
        ELFDATA2LSB => Endian::Little,
        ELFDATA2MSB => Endian::Big,
        _ => return Err("invalid endianness"),
    };
    let is_64 = match bytes[EI_CLASS] {
        ELFCLASS32 => false,
        ELFCLASS64 => true,
        _ => return Err("invalid bitness"),
    };

    let header_len = if is_64 { 64 } else { 52 };
    let raw = bytes.get(0..header_len).ok_or("ELF header truncated")?;
    let mut e_ident = [0u8; 16];
    e_ident.copy_from_slice(&raw[0..16]);

    let mut cursor = &raw[16..];
    let e_type = read_u16(&mut cursor, endianness).ok_or("e_type")?;
    let e_machine = read_u16(&mut cursor, endianness).ok_or("e_machine")?;
    let e_version = read_u32(&mut cursor, endianness).ok_or("e_version")?;
    let (e_entry, e_phoff, e_shoff) = if is_64 {
        let entry = read_u64(&mut cursor, endianness).ok_or("e_entry")?;
        let phoff = read_u64(&mut cursor, endianness).ok_or("e_phoff")?;
        let shoff = read_u64(&mut cursor, endianness).ok_or("e_shoff")?;
        (entry, phoff, shoff)
    } else {
        let entry = read_u32(&mut cursor, endianness).ok_or("e_entry")? as u64;
        let phoff = read_u32(&mut cursor, endianness).ok_or("e_phoff")? as u64;
        let shoff = read_u32(&mut cursor, endianness).ok_or("e_shoff")? as u64;
        (entry, phoff, shoff)
    };
    let e_flags = read_u32(&mut cursor, endianness).ok_or("e_flags")?;
    let e_ehsize = read_u16(&mut cursor, endianness).ok_or("e_ehsize")?;
    let e_phentsize = read_u16(&mut cursor, endianness).ok_or("e_phentsize")?;
    let e_phnum = read_u16(&mut cursor, endianness).ok_or("e_phnum")?;
    let e_shentsize = read_u16(&mut cursor, endianness).ok_or("e_shentsize")?;
    let e_shnum = read_u16(&mut cursor, endianness).ok_or("e_shnum")?;
    let e_shstrndx = read_u16(&mut cursor, endianness).ok_or("e_shstrndx")?;

    let header = Header {
        e_ident,
        e_type,
        e_machine,
        e_version,
        e_entry,
        e_phoff,
        e_shoff,
        e_flags,
        e_ehsize,
        e_phentsize,
        e_phnum,
        e_shentsize,
        e_shnum,
        e_shstrndx,
    };

    Ok(Elf {
        bytes,
        header,
        endianness,
        is_64,
    })
}
