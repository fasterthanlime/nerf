//! `nperf annotate` — disassembles hot functions with per-instruction sample
//! counts (perf-annotate style). Supports x86_64 and aarch64.
//!
//! Address-space discipline. Two virtual-address spaces show up here and they
//! must not be confused:
//!
//! * [`AbsoluteAddr`] — a runtime VA. What `sample.user_backtrace[i].address`
//!   carries, what `/proc/<pid>/maps` lists, what shows up next to JIT'd
//!   code. Equal to the program counter the kernel saw.
//!
//! * [`RelativeAddr`] — a binary-internal VA. What an ELF symbol table's
//!   `st_value` holds, what `LoadHeader::address` reports, what
//!   `nwind::ResolvedSymbol::relative_address` returns. For a non-PIE
//!   executable this coincides with the absolute address; for a PIE/DSO it
//!   differs by the per-mapping load offset.
//!
//! Native-code bookkeeping is done entirely in `RelativeAddr` (counts, range,
//! disassembly base) so we never have to track per-process load offsets for
//! libraries shared across mappings. JIT'd code has no relative space — its
//! addresses live wherever the JIT mmap'd them — so JIT counts stay in
//! `AbsoluteAddr`.
//!
//! Symbol resolution and demangling go through nwind's
//! [`IAddressSpace::lookup_symbol`] — this module does not run a demangler
//! itself.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use nwind::BinaryId;
use yaxpeax_arch::{Decoder, LengthedInstruction, U8Reader};
use yaxpeax_arm::armv8::a64::InstDecoder as Aarch64Decoder;
use yaxpeax_x86::amd64::InstDecoder as Amd64Decoder;

use crate::args::AnnotateArgs;
use crate::data_reader::{Binary, EventKind, read_data, repack_cli_args};

#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct AbsoluteAddr( u64 );

#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct RelativeAddr( u64 );

impl AbsoluteAddr { fn raw( self ) -> u64 { self.0 } }
impl RelativeAddr { fn raw( self ) -> u64 { self.0 } }

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum FuncSourceTag {
    Native( BinaryId ),
    Jit
}

type FuncKey = (FuncSourceTag, String);

enum FuncRecord {
    Native {
        range: Range< RelativeAddr >,
        counts: BTreeMap< RelativeAddr, u64 >,
        total: u64
    },
    Jit {
        range: Range< AbsoluteAddr >,
        counts: BTreeMap< AbsoluteAddr, u64 >,
        total: u64
    }
}

/// Per-binary runtime context used to resolve relative→absolute addresses
/// after the read pass. We hold onto one (pid, load_offset) per binary so
/// `IAddressSpace::decode_symbol_while` (which needs an absolute address)
/// can be called from the rendering phase. The pid is whichever process
/// first sampled inside that binary.
#[derive(Copy, Clone)]
struct BinaryAnchor {
    pid: u32,
    load_offset: u64
}

impl FuncRecord {
    fn total( &self ) -> u64 {
        match self {
            FuncRecord::Native { total, .. } | FuncRecord::Jit { total, .. } => *total
        }
    }
}

/// One executable segment of an on-disk binary, in its own SVMA space.
/// Both ELF `PT_LOAD` and Mach-O `LC_SEGMENT*` flatten to this shape.
#[derive(Clone, Copy, Debug)]
struct CodeSegment {
    address: u64,
    size: u64,
    file_offset: u64,
    file_size: u64,
}

/// Bytes + segment table for one binary, suitable for slicing out
/// instruction bytes by relative address. We hold the full file in
/// memory (`Vec<u8>`) so the lookup is one indirection — annotate is
/// post-processing, not a hot path.
struct CodeImage {
    bytes: Arc< Vec< u8 > >,
    segments: Vec< CodeSegment >
}

impl CodeImage {
    /// Slice out bytes for `range`, expressed in the binary's SVMA, by
    /// finding the segment that contains it and translating to a file
    /// offset.
    fn fetch( &self, range: &Range< RelativeAddr > ) -> Option< &[u8] > {
        let start = range.start.raw();
        let end = range.end.raw();
        let len = (end - start) as usize;
        for seg in &self.segments {
            let seg_end = seg.address.checked_add( seg.size )?;
            if seg.address <= start && end <= seg_end {
                let in_segment = start - seg.address;
                if in_segment.checked_add( len as u64 )? > seg.file_size {
                    return None;
                }
                let file_off = (seg.file_offset + in_segment) as usize;
                if file_off.checked_add( len )? > self.bytes.len() {
                    return None;
                }
                return Some( &self.bytes[ file_off..file_off + len ] );
            }
        }
        None
    }
}

/// Per-binary code-bytes cache. Tries (1) the embedded archive blob,
/// (2) the binary at its recorded path on disk, (3) the local macOS
/// dyld shared cache for system dylibs. ELF and Mach-O both work
/// through `object::File`; (3) is what makes
/// `libsystem_malloc.dylib`-style entries annotate even though no
/// real file exists at their install path.
///
/// Symbol lookup goes through `IAddressSpace`; this cache is purely
/// about *bytes to disassemble*.
struct CodeCache {
    by_binary: HashMap< BinaryId, Option< Arc< CodeImage > > >,
    dyld: DyldCacheRegistry
}

impl CodeCache {
    fn new( arch: &str ) -> Self {
        CodeCache {
            by_binary: HashMap::new(),
            dyld: DyldCacheRegistry::new( arch.to_owned() )
        }
    }

    fn get( &mut self, binary_id: &BinaryId, binary: &Binary ) -> Option< &Arc< CodeImage > > {
        if !self.by_binary.contains_key( binary_id ) {
            let image = build_image_from_archive( binary )
                .or_else( || build_image_from_disk( binary ) )
                .or_else( || self.dyld.image_for( binary.path() ).map( Arc::new ) );
            self.by_binary.insert( binary_id.clone(), image );
        }
        self.by_binary.get( binary_id ).and_then( |opt| opt.as_ref() )
    }
}

/// Lazy holder for the local macOS dyld shared cache.
///
/// The shared cache is a single (large) file plus a numbered chain of
/// `.1`, `.2`, … subcaches and an optional `.symbols` subcache. We mmap
/// each, then ask `object::read::macho::DyldCache` to assemble them so
/// `image.parse_object()` returns a Mach-O view whose segments yield
/// real bytes via `seg.data()` (already split across the right
/// subcache for us).
///
/// The first lookup probes a few well-known paths; failure is cached
/// so we don't keep stat'ing missing files for every binary.
struct DyldCacheRegistry {
    arch: String,
    /// `None` = not tried yet, `Some(None)` = tried and failed,
    /// `Some(Some(_))` = open.
    bundle: Option< Option< Arc< DyldCacheBundle > > >
}

struct DyldCacheBundle {
    main: memmap2::Mmap,
    subcaches: Vec< memmap2::Mmap >
}

impl DyldCacheRegistry {
    fn new( arch: String ) -> Self {
        DyldCacheRegistry { arch, bundle: None }
    }

    fn bundle( &mut self ) -> Option< Arc< DyldCacheBundle > > {
        if self.bundle.is_none() {
            self.bundle = Some( open_local_dyld_cache( &self.arch ).map( Arc::new ) );
        }
        self.bundle.as_ref().and_then( |opt| opt.clone() )
    }

    fn image_for( &mut self, install_path: &str ) -> Option< CodeImage > {
        let bundle = self.bundle()?;
        let main_data: &[u8] = &bundle.main;
        let sub_data: Vec< &[u8] > = bundle.subcaches.iter().map( |m| &m[..] ).collect();
        let cache = match object::read::macho::DyldCache::< object::Endianness >::parse( main_data, &sub_data ) {
            Ok( c ) => c,
            Err( err ) => {
                debug!( "annotate: failed to parse local dyld shared cache: {}", err );
                return None;
            }
        };

        for image in cache.images() {
            let path = match image.path() {
                Ok( p ) => p,
                Err( _ ) => continue
            };
            if path != install_path {
                continue;
            }
            let parsed = match image.parse_object() {
                Ok( f ) => f,
                Err( err ) => {
                    debug!( "annotate: dyld cache image '{}' parse_object failed: {}", install_path, err );
                    return None;
                }
            };
            debug!( "annotate: extracted '{}' from local dyld shared cache", install_path );
            return Some( macho_to_code_image( &parsed ) );
        }
        debug!( "annotate: install path '{}' not present in local dyld shared cache", install_path );
        None
    }
}

/// Probe the standard macOS install locations for the shared cache
/// file matching `arch` and mmap it (plus its `.N` and `.symbols`
/// subcaches, in the order the parser expects).
fn open_local_dyld_cache( arch: &str ) -> Option< DyldCacheBundle > {
    // arm64e is the actual Apple-Silicon variant; arm64 is a
    // backward-compat naming. Try the more specific one first.
    let suffixes: &[&str] = match arch {
        "aarch64" => &["arm64e", "arm64"],
        "amd64"   => &["x86_64h", "x86_64"],
        _ => return None,
    };
    let prefixes: &[&str] = &[
        "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
        "/System/Cryptexes/OS/System/Library/dyld",
        "/System/Library/dyld",
    ];

    for prefix in prefixes {
        for suffix in suffixes {
            let main_path = std::path::Path::new( prefix )
                .join( format!( "dyld_shared_cache_{}", suffix ) );
            if !main_path.exists() {
                continue;
            }
            match open_dyld_bundle( &main_path ) {
                Ok( bundle ) => {
                    debug!( "annotate: opened dyld shared cache at {:?} (+{} subcaches)",
                            main_path, bundle.subcaches.len() );
                    return Some( bundle );
                }
                Err( err ) => {
                    debug!( "annotate: failed to open dyld cache at {:?}: {}", main_path, err );
                }
            }
        }
    }
    None
}

fn open_dyld_bundle( main_path: &std::path::Path ) -> io::Result< DyldCacheBundle > {
    let main_file = std::fs::File::open( main_path )?;
    let main = unsafe { memmap2::Mmap::map( &main_file )? };

    let parent = main_path.parent().ok_or_else( || io::Error::new(
        io::ErrorKind::Other, "dyld cache path has no parent" ) )?;
    let stem = main_path.file_name().ok_or_else( || io::Error::new(
        io::ErrorKind::Other, "dyld cache path has no file name" ) )?
        .to_string_lossy().into_owned();

    let mut subcaches = Vec::new();
    // Numbered subcaches first, in order, then `.symbols` last.
    for i in 1.. {
        let p = parent.join( format!( "{}.{}", stem, i ) );
        if !p.exists() {
            break;
        }
        let f = std::fs::File::open( &p )?;
        subcaches.push( unsafe { memmap2::Mmap::map( &f )? } );
    }
    let symbols = parent.join( format!( "{}.symbols", stem ) );
    if symbols.exists() {
        let f = std::fs::File::open( &symbols )?;
        subcaches.push( unsafe { memmap2::Mmap::map( &f )? } );
    }

    Ok( DyldCacheBundle { main, subcaches } )
}

/// Copy each Mach-O segment's bytes into one packed buffer, producing
/// a `CodeImage` whose synthetic `file_offset`s point into that
/// buffer. We do this so a `CodeImage` always speaks the same shape
/// regardless of whether it was sourced from a regular file or from
/// a dyld cache (whose segments aren't contiguous in any one file).
fn macho_to_code_image( file: &object::File ) -> CodeImage {
    use object::{Object, ObjectSegment};
    let mut combined: Vec< u8 > = Vec::new();
    let mut segments: Vec< CodeSegment > = Vec::new();
    for seg in file.segments() {
        let data = match seg.data() {
            Ok( d ) => d,
            Err( _ ) => continue
        };
        if data.is_empty() {
            continue;
        }
        let file_offset = combined.len() as u64;
        combined.extend_from_slice( data );
        segments.push( CodeSegment {
            address: seg.address(),
            size: seg.size(),
            file_offset,
            file_size: data.len() as u64
        });
    }
    CodeImage { bytes: Arc::new( combined ), segments }
}

/// On-demand cache of source-file contents.
///
/// Lookup tries a sequence of candidate paths because DWARF's `file` is
/// often relative to the CU's `comp_dir` (DWARF 5 line programs do this
/// almost always), and that comp_dir was the build machine's path —
/// useless on the user's box. So we try, in order:
///
/// 1. The literal path DWARF gave us.
/// 2. `comp_dir / file`, when comp_dir is present.
/// 3. Each user-supplied `--source-path` prefix joined with the file
///    (full and basename), so users can plug in a Debian-style
///    `/usr/src/debug/glibc-2.41` and have things work out.
///
/// Both successful reads and "not found anywhere" results are cached,
/// keyed by the original DWARF path, so we don't re-try every probe per
/// instruction.
struct SourceCache {
    by_dwarf_path: HashMap< String, Option< Vec< String > > >,
    extra_prefixes: Vec< PathBuf >
}

impl SourceCache {
    fn new( extra_prefixes: Vec< PathBuf > ) -> Self {
        SourceCache { by_dwarf_path: HashMap::new(), extra_prefixes }
    }

    fn line( &mut self, dwarf_path: &str, comp_dir: Option< &str >, line: u64 ) -> Option< &str > {
        if !self.by_dwarf_path.contains_key( dwarf_path ) {
            let loaded = self.try_load( dwarf_path, comp_dir );
            self.by_dwarf_path.insert( dwarf_path.to_owned(), loaded );
        }
        let entry = self.by_dwarf_path.get( dwarf_path )?;
        let lines = entry.as_ref()?;
        let idx = (line as usize).checked_sub( 1 )?;
        lines.get( idx ).map( |s| s.as_str() )
    }

    fn try_load( &self, dwarf_path: &str, comp_dir: Option< &str > ) -> Option< Vec< String > > {
        fn strip_dot( s: &str ) -> &str { s.strip_prefix( "./" ).unwrap_or( s ) }
        let path = strip_dot( dwarf_path );
        let cd = comp_dir.map( strip_dot );

        // glibc records both `comp_dir = ./nptl` and `file = ./nptl/cancellation.c`,
        // so naively joining produces `nptl/nptl/cancellation.c` (doesn't exist).
        // When the file already starts with comp_dir, treat it as a source-root-
        // relative path and skip the join.
        let already_under_cd = match cd {
            Some( c ) => path.starts_with( c )
                && (path.len() == c.len() || path.as_bytes().get( c.len() ) == Some( &b'/' )),
            None => false,
        };

        let basename = std::path::Path::new( path )
            .file_name()
            .and_then( |n| n.to_str() )
            .filter( |b| *b != path );

        let mut candidates: Vec< PathBuf > = Vec::new();
        candidates.push( PathBuf::from( dwarf_path ) );
        if path != dwarf_path {
            candidates.push( PathBuf::from( path ) );
        }
        if let Some( c ) = cd {
            if !already_under_cd {
                candidates.push( PathBuf::from( c ).join( path ) );
            }
        }
        for prefix in &self.extra_prefixes {
            candidates.push( prefix.join( path ) );
            if let Some( c ) = cd {
                if !already_under_cd {
                    candidates.push( prefix.join( c ).join( path ) );
                }
            }
            if let Some( b ) = basename {
                candidates.push( prefix.join( b ) );
            }
        }

        for candidate in &candidates {
            if let Ok( contents ) = fs::read_to_string( candidate ) {
                debug!( "annotate: source for '{}' loaded from '{}'", dwarf_path, candidate.display() );
                return Some( contents.lines().map( str::to_owned ).collect() );
            }
        }
        None
    }
}

/// Build a `CodeImage` from an archive-embedded ELF blob (the
/// recorder writes these for `--offline` captures).
fn build_image_from_archive( binary: &Binary ) -> Option< Arc< CodeImage > > {
    let data = binary.data()?;
    let bytes: Arc< Vec< u8 > > = Arc::new( data.as_bytes().to_vec() );
    let segments = parse_segments( &bytes )?;
    Some( Arc::new( CodeImage { bytes, segments } ) )
}

/// Build a `CodeImage` by reading the binary off disk. Handles ELF and
/// Mach-O via `object::File`, so system libraries (libc.so.6,
/// libsystem_malloc.dylib, …) annotate without needing the recorder
/// to embed them.
fn build_image_from_disk( binary: &Binary ) -> Option< Arc< CodeImage > > {
    let path = binary.path();
    // Skip pseudo-paths like "[vdso]", "[heap]"; fs::read would just fail.
    if path.starts_with( '[' ) {
        return None;
    }
    let bytes = match fs::read( path ) {
        Ok( b ) => b,
        Err( err ) => {
            debug!( "annotate: could not open '{}' from disk: {}", path, err );
            return None;
        }
    };
    let bytes: Arc< Vec< u8 > > = Arc::new( bytes );
    let segments = parse_segments( &bytes )?;
    Some( Arc::new( CodeImage { bytes, segments } ) )
}

/// Parse `bytes` (ELF or Mach-O) and collect its loadable segments.
/// Returns `None` if the file isn't a recognized object format.
fn parse_segments( bytes: &[u8] ) -> Option< Vec< CodeSegment > > {
    use object::{Object, ObjectSegment};
    let file = match object::File::parse( bytes ) {
        Ok( f ) => f,
        Err( err ) => {
            debug!( "annotate: object::File::parse failed: {}", err );
            return None;
        }
    };
    let mut segments = Vec::new();
    for seg in file.segments() {
        let (file_offset, file_size) = seg.file_range();
        segments.push( CodeSegment {
            address: seg.address(),
            size: seg.size(),
            file_offset,
            file_size
        });
    }
    Some( segments )
}

fn format_hex_bytes( bytes: &[u8] ) -> String {
    let mut out = String::with_capacity( bytes.len() * 3 );
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push( ' ' );
        }
        let _ = write!( &mut out, "{:02x}", byte );
    }
    out
}

/// Resolved (file, line) for a single instruction, plus the CU's
/// compilation directory when DWARF carried it. We only care about the
/// bottom (innermost) frame for header purposes — that's the source the
/// user wrote that the instruction was generated from.
#[derive(Clone, PartialEq, Eq)]
struct LineInfo {
    file: String,
    line: u64,
    comp_dir: Option< String >,
}

/// Emit the source-line header (filename:line plus a trimmed snippet)
/// whenever the resolved location changes between instructions.
fn maybe_emit_source_header< W: Write, F >(
    addr: u64,
    last_line: &mut Option< LineInfo >,
    resolve_line: &mut Option< F >,
    source_cache: &mut Option< &mut SourceCache >,
    out: &mut W
) -> io::Result< () >
    where F: FnMut( u64 ) -> Option< LineInfo >
{
    if let Some( resolver ) = resolve_line.as_mut() {
        let info = resolver( addr );
        if info != *last_line {
            if let Some( ref info ) = info {
                let snippet = source_cache
                    .as_deref_mut()
                    .and_then( |cache| cache.line( &info.file, info.comp_dir.as_deref(), info.line ) )
                    .map( |s| s.trim().to_owned() )
                    .unwrap_or_default();
                let basename = std::path::Path::new( &info.file )
                    .file_name()
                    .and_then( |n| n.to_str() )
                    .unwrap_or( info.file.as_str() );
                writeln!( out, "  ; {}:{}  {}", basename, info.line, snippet )?;
            }
            *last_line = info;
        }
    }
    Ok(())
}

/// Disassemble a function's bytes and write per-instruction lines, marking
/// hot ones and (when `resolve_line` is provided) emitting a source-line
/// header whenever the resolved (file, line) changes.
fn disassemble_amd64< W: Write, F >(
    decoder: &Amd64Decoder,
    bytes: &[u8],
    base: u64,
    counts: &BTreeMap< u64, u64 >,
    resolve_line: Option< F >,
    source_cache: Option< &mut SourceCache >,
    out: &mut W
) -> io::Result< () >
    where F: FnMut( u64 ) -> Option< LineInfo >
{
    let mut offset: usize = 0;
    let mut last_line: Option< LineInfo > = None;
    let mut resolve_line = resolve_line;
    let mut source_cache = source_cache;

    while offset < bytes.len() {
        let addr = base + offset as u64;
        let count = counts.get( &addr ).copied().unwrap_or( 0 );
        let mark = if count > 0 { ">" } else { " " };

        maybe_emit_source_header( addr, &mut last_line, &mut resolve_line, &mut source_cache, out )?;

        match decoder.decode_slice( &bytes[ offset.. ] ) {
            Ok( instr ) => {
                let len = instr.len().to_const() as usize;
                let end = (offset + len).min( bytes.len() );
                let hex = format_hex_bytes( &bytes[ offset..end ] );
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  {}",
                          mark, count, addr, hex, instr )?;
                offset = end;
            }
            Err( err ) => {
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  <decode error: {}>",
                          mark, count, addr, format!( "{:02x}", bytes[ offset ] ), err )?;
                offset += 1;
            }
        }
    }
    Ok(())
}

/// aarch64 has fixed 4-byte instructions, so the loop just steps a word at
/// a time. (yaxpeax-arm does report a length, but it's always 4.)
fn disassemble_aarch64< W: Write, F >(
    decoder: &Aarch64Decoder,
    bytes: &[u8],
    base: u64,
    counts: &BTreeMap< u64, u64 >,
    resolve_line: Option< F >,
    source_cache: Option< &mut SourceCache >,
    out: &mut W
) -> io::Result< () >
    where F: FnMut( u64 ) -> Option< LineInfo >
{
    let mut offset: usize = 0;
    let mut last_line: Option< LineInfo > = None;
    let mut resolve_line = resolve_line;
    let mut source_cache = source_cache;

    while offset + 4 <= bytes.len() {
        let addr = base + offset as u64;
        let count = counts.get( &addr ).copied().unwrap_or( 0 );
        let mark = if count > 0 { ">" } else { " " };

        maybe_emit_source_header( addr, &mut last_line, &mut resolve_line, &mut source_cache, out )?;

        let inst_bytes = &bytes[ offset..offset + 4 ];
        let hex = format_hex_bytes( inst_bytes );
        let mut reader = U8Reader::new( inst_bytes );
        match decoder.decode( &mut reader ) {
            Ok( instr ) => {
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  {}",
                          mark, count, addr, hex, instr )?;
            }
            Err( err ) => {
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  <decode error: {}>",
                          mark, count, addr, hex, err )?;
            }
        }
        offset += 4;
    }
    Ok(())
}

pub fn main( args: AnnotateArgs ) -> Result< (), Box< dyn Error > > {
    // JIT code bytes live on `State` now: `read_data` populates them
    // from both the explicit `--jitdump` file (if any) and any
    // `jit-*.dump` blobs the recorder embedded in the archive
    // (FileBlob packets at session end). We just consume them here.
    let (_, read_data_args) = repack_cli_args( &args.collation_args );

    let mut funcs: HashMap< FuncKey, FuncRecord > = HashMap::new();
    // First (pid, load_offset) we observe per binary — used at render time
    // to drive line-info lookups through the process's address_space.
    let mut anchors: HashMap< BinaryId, BinaryAnchor > = HashMap::new();

    let state = read_data( read_data_args, |event| {
        let sample = match event.kind {
            EventKind::Sample( s ) => s,
            _ => return
        };
        let leaf = match sample.user_backtrace.first() {
            Some( f ) => f,
            None => return
        };
        let leaf_va = AbsoluteAddr( leaf.address );

        // Native? Resolve through the address space — one call gets us the
        // demangled name, binary-relative function range, and instruction's
        // own relative address. The region lookup just gives us the BinaryId
        // we need to key by (parallel `RangeMap`s — process.memory_regions
        // and address_space.regions are populated independently).
        if let Some( region ) = sample.process.memory_regions().get_value( leaf_va.raw() ) {
            let binary_id: BinaryId = region.into();

            let symbol = match sample.process.address_space().lookup_symbol( leaf_va.raw() ) {
                Some( s ) => s,
                None => return
            };

            let name = symbol.demangled_name
                .unwrap_or( symbol.raw_name )
                .into_owned();
            let rel_addr = RelativeAddr( symbol.relative_address );
            let range = Range {
                start: RelativeAddr( symbol.relative_range.start ),
                end:   RelativeAddr( symbol.relative_range.end )
            };

            // Anchor this binary to a (pid, load_offset) on first sight —
            // load_offset is `leaf.absolute - symbol.relative_address`.
            anchors.entry( binary_id.clone() ).or_insert( BinaryAnchor {
                pid: sample.process.pid(),
                load_offset: leaf_va.raw().wrapping_sub( symbol.relative_address )
            });

            let key: FuncKey = (FuncSourceTag::Native( binary_id ), name);
            let entry = funcs.entry( key ).or_insert_with( || FuncRecord::Native {
                range,
                counts: BTreeMap::new(),
                total: 0
            });
            if let FuncRecord::Native { counts, total, .. } = entry {
                *counts.entry( rel_addr ).or_insert( 0 ) += 1;
                *total += 1;
            }
            return;
        }

        // JIT? Look up by absolute VA in the jitdump_names range map.
        if let Some( idx ) = event.state.jitdump_names().get_index( leaf_va.raw() ) {
            let (range, name) = event.state.jitdump_names().get_by_index( idx ).unwrap();
            let key: FuncKey = (FuncSourceTag::Jit, name.clone());
            let abs_range = Range {
                start: AbsoluteAddr( range.start ),
                end:   AbsoluteAddr( range.end )
            };
            let entry = funcs.entry( key ).or_insert_with( || FuncRecord::Jit {
                range: abs_range,
                counts: BTreeMap::new(),
                total: 0
            });
            if let FuncRecord::Jit { counts, total, .. } = entry {
                *counts.entry( leaf_va ).or_insert( 0 ) += 1;
                *total += 1;
            }
        }
    })?;

    enum Arch { Amd64, Aarch64 }
    let arch = match state.architecture() {
        "amd64" => Arch::Amd64,
        "aarch64" => Arch::Aarch64,
        other => return Err( format!(
            "annotate: unsupported architecture '{}' (supports amd64, aarch64)", other
        ).into() ),
    };

    if funcs.is_empty() {
        eprintln!( "annotate: no samples landed in known functions" );
        return Ok(());
    }

    let mut chosen: Vec< (FuncKey, FuncRecord) > = if args.function.is_empty() {
        let mut v: Vec< _ > = funcs.into_iter().collect();
        v.sort_by( |a, b| b.1.total().cmp( &a.1.total() ) );
        v.truncate( args.top.max( 1 ) );
        v
    } else {
        funcs.into_iter()
            .filter( |(k, _)| args.function.iter().any( |needle| k.1.contains( needle ) ) )
            .collect()
    };
    chosen.sort_by( |a, b| b.1.total().cmp( &a.1.total() ) );

    if chosen.is_empty() {
        eprintln!( "annotate: no functions matched the --function filter" );
        return Ok(());
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let amd64_decoder = Amd64Decoder::default();
    let aarch64_decoder = Aarch64Decoder::default();
    let mut code_cache = CodeCache::new( state.architecture() );
    let mut source_cache = SourceCache::new(
        args.source_path.iter().map( PathBuf::from ).collect()
    );

    for ((tag, name), record) in chosen {
        match (tag, record) {
            (FuncSourceTag::Native( binary_id ), FuncRecord::Native { range, counts, total }) => {
                let binary = state.get_binary( &binary_id );
                let label = binary.basename();
                let code = code_cache.get( &binary_id, binary );
                let bytes = match code.and_then( |image| image.fetch( &range ) ) {
                    Some( b ) => b,
                    None => {
                        writeln!( out, "==== {} [{}]  rel 0x{:x}..0x{:x}  total={}  (no code bytes available) ====\n",
                                  name, label, range.start.raw(), range.end.raw(), total )?;
                        continue;
                    }
                };
                writeln!( out, "==== {} [{}]  rel 0x{:x}..0x{:x}  total={} samples ====",
                          name, label, range.start.raw(), range.end.raw(), total )?;
                writeln!( out, "      count   address       bytes                           asm" )?;
                let counts_u64: BTreeMap< u64, u64 > = counts.into_iter().map( |(k, v)| (k.raw(), v) ).collect();

                // For --source: build a closure that maps a relative address to (file,
                // line) by translating to absolute via the binary's anchor and then
                // asking the process's address_space.
                let resolver = if args.source {
                    anchors.get( &binary_id ).and_then( |anchor| {
                        let process = state.get_process( anchor.pid )?;
                        Some( (process.address_space(), anchor.load_offset) )
                    })
                } else {
                    None
                };

                if let Some( (address_space, load_offset) ) = resolver {
                    let resolve = |rel_addr: u64| -> Option< LineInfo > {
                        let abs = rel_addr.wrapping_add( load_offset );
                        let mut info: Option< LineInfo > = None;
                        address_space.decode_symbol_while( abs, &mut |frame| {
                            if info.is_none() {
                                if let (Some( file ), Some( line )) = (frame.file.as_ref(), frame.line) {
                                    info = Some( LineInfo {
                                        file: file.clone(),
                                        line,
                                        comp_dir: frame.comp_dir.clone()
                                    });
                                }
                            }
                            true
                        });
                        info
                    };
                    match arch {
                        Arch::Amd64 => disassemble_amd64( &amd64_decoder, bytes, range.start.raw(), &counts_u64,
                                       Some( resolve ), Some( &mut source_cache ), &mut out )?,
                        Arch::Aarch64 => disassemble_aarch64( &aarch64_decoder, bytes, range.start.raw(), &counts_u64,
                                       Some( resolve ), Some( &mut source_cache ), &mut out )?,
                    }
                } else {
                    let no_resolve: Option< fn(u64) -> Option< LineInfo > > = None;
                    match arch {
                        Arch::Amd64 => disassemble_amd64( &amd64_decoder, bytes, range.start.raw(), &counts_u64,
                                       no_resolve, None, &mut out )?,
                        Arch::Aarch64 => disassemble_aarch64( &aarch64_decoder, bytes, range.start.raw(), &counts_u64,
                                       no_resolve, None, &mut out )?,
                    }
                }
            }
            (FuncSourceTag::Jit, FuncRecord::Jit { range, counts, total }) => {
                let bytes = match state.jit_code().get( &range.start.raw() ) {
                    Some( b ) => b.as_slice(),
                    None => {
                        writeln!( out, "==== {} [JIT]  range 0x{:x}..0x{:x}  total={}  (no jitdump code bytes — recorder didn't embed a jit-*.dump FileBlob; pass --jitdump to override) ====\n",
                                  name, range.start.raw(), range.end.raw(), total )?;
                        continue;
                    }
                };
                writeln!( out, "==== {} [JIT]  range 0x{:x}..0x{:x}  total={} samples ====",
                          name, range.start.raw(), range.end.raw(), total )?;
                writeln!( out, "      count   address       bytes                           asm" )?;
                let counts_u64: BTreeMap< u64, u64 > = counts.into_iter().map( |(k, v)| (k.raw(), v) ).collect();
                let no_resolve: Option< fn(u64) -> Option< LineInfo > > = None;
                match arch {
                    Arch::Amd64 => disassemble_amd64( &amd64_decoder, bytes, range.start.raw(), &counts_u64,
                                       no_resolve, None, &mut out )?,
                    Arch::Aarch64 => disassemble_aarch64( &aarch64_decoder, bytes, range.start.raw(), &counts_u64,
                                       no_resolve, None, &mut out )?,
                }
            }
            // FuncSourceTag and FuncRecord are constructed in lockstep above —
            // the cross-variant cases can't occur.
            _ => unreachable!()
        }
        writeln!( out )?;
    }

    Ok(())
}
