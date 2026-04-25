use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::ops::Range;

use nwind::{BinaryId, Symbols};
use yaxpeax_arch::LengthedInstruction;
use yaxpeax_x86::amd64::InstDecoder;

use crate::args::AnnotateArgs;
use crate::data_reader::{
    Binary, EventKind, read_data, repack_cli_args,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum FunctionSource {
    Native( BinaryId ),
    Jit
}

type FuncKey = (FunctionSource, String);

struct FunctionData {
    range: Range< u64 >,
    counts: BTreeMap< u64, u64 >,
    total: u64
}

struct SymbolCache {
    by_binary: HashMap< BinaryId, Vec< (Range< u64 >, String) > >
}

impl SymbolCache {
    fn new() -> Self {
        SymbolCache { by_binary: HashMap::new() }
    }

    fn ensure_loaded( &mut self, binary_id: &BinaryId, binary: &Binary ) {
        if self.by_binary.contains_key( binary_id ) {
            return;
        }
        let mut v: Vec< (Range< u64 >, String) > = Vec::new();
        if let Some( data ) = binary.data() {
            Symbols::each_from_binary_data( data, |range, name| {
                v.push( (range, rustc_demangle::demangle( name ).to_string()) );
            });
        }
        v.sort_by( |a, b| a.0.start.cmp( &b.0.start ) );
        self.by_binary.insert( binary_id.clone(), v );
    }

    fn lookup< 'a >( &'a mut self, binary_id: &BinaryId, binary: &Binary, addr: u64 ) -> Option< (Range< u64 >, &'a str) > {
        self.ensure_loaded( binary_id, binary );
        let syms = self.by_binary.get( binary_id ).unwrap();
        let idx = syms.partition_point( |(range, _)| range.start <= addr );
        if idx == 0 {
            return None;
        }
        let (range, name) = &syms[ idx - 1 ];
        if range.contains( &addr ) {
            Some( (range.clone(), name.as_str()) )
        } else {
            None
        }
    }
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

fn fetch_native_bytes< 'a >( binary: &'a Binary, range: &Range< u64 > ) -> Option< &'a [u8] > {
    let data = binary.data()?;
    let len = (range.end - range.start) as usize;
    for header in data.load_headers() {
        if !header.is_executable {
            continue;
        }
        let segment_end = header.address + header.memory_size;
        if header.address <= range.start && range.end <= segment_end {
            let in_segment = range.start - header.address;
            // The function's bytes might lie partially outside file_size (e.g. in
            // a BSS-style tail), but for executable text that's vanishingly rare.
            // Be defensive anyway.
            if in_segment + (len as u64) > header.file_size {
                return None;
            }
            let file_off = (header.file_offset + in_segment) as usize;
            let bytes = data.as_bytes();
            if file_off.checked_add( len )? > bytes.len() {
                return None;
            }
            return Some( &bytes[ file_off..file_off + len ] );
        }
    }
    None
}

fn disassemble_amd64< W: Write >(
    decoder: &InstDecoder,
    bytes: &[u8],
    base: u64,
    counts: &BTreeMap< u64, u64 >,
    out: &mut W
) -> io::Result< () > {
    let mut offset: usize = 0;
    while offset < bytes.len() {
        let addr = base + offset as u64;
        match decoder.decode_slice( &bytes[ offset.. ] ) {
            Ok( instr ) => {
                let len = instr.len().to_const() as usize;
                let end = (offset + len).min( bytes.len() );
                let count = counts.get( &addr ).copied().unwrap_or( 0 );
                let mark = if count > 0 { ">" } else { " " };
                let hex = format_hex_bytes( &bytes[ offset..end ] );
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  {}",
                          mark, count, addr, hex, instr )?;
                offset = end;
            }
            Err( err ) => {
                let count = counts.get( &addr ).copied().unwrap_or( 0 );
                let mark = if count > 0 { ">" } else { " " };
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  <decode error: {}>",
                          mark, count, addr, format!( "{:02x}", bytes[ offset ] ), err )?;
                offset += 1;
            }
        }
    }
    Ok(())
}

pub fn main( args: AnnotateArgs ) -> Result< (), Box< dyn Error > > {
    // Parse the jitdump up front (if any), capturing the actual code bytes
    // alongside the per-record VA. State::jitdump_names already remembers the
    // VA->name mapping, but throws the bytes away — we need them here.
    let jit_code: HashMap< u64, Vec< u8 > > = if let Some( path ) = args.collation_args.jitdump.as_ref() {
        let dump = crate::jitdump::JitDump::load( std::path::Path::new( path ) )
            .map_err( |err| format!( "failed to open jitdump {:?}: {}", path, err ) )?;
        let mut map = HashMap::new();
        for record in dump.records {
            if let crate::jitdump::Record::CodeLoad { virtual_address, code, .. } = record {
                map.insert( virtual_address, code.into_owned() );
            }
        }
        map
    } else {
        HashMap::new()
    };

    let (_, read_data_args) = repack_cli_args( &args.collation_args );

    let mut sym_cache = SymbolCache::new();
    let mut funcs: HashMap< FuncKey, FunctionData > = HashMap::new();

    let state = read_data( read_data_args, |event| {
        let sample = match event.kind {
            EventKind::Sample( s ) => s,
            _ => return
        };
        let leaf = match sample.user_backtrace.first() {
            Some( f ) => f,
            None => return
        };
        // For the leaf frame we want the actual sampled IP (not IP - 1, which
        // is what callers further up the stack get). `user_frame.address` is
        // already the IP at sample time.
        let addr = leaf.address;

        // Native?
        if let Some( region ) = sample.process.memory_regions().get_value( addr ) {
            let binary_id: BinaryId = region.into();
            let binary = event.state.get_binary( &binary_id );
            let (range, name) = match sym_cache.lookup( &binary_id, binary, addr ) {
                Some( (r, n) ) => (r, n.to_string()),
                None => return
            };
            let key: FuncKey = (FunctionSource::Native( binary_id ), name);
            let entry = funcs.entry( key ).or_insert_with( || FunctionData {
                range: range.clone(),
                counts: BTreeMap::new(),
                total: 0
            });
            *entry.counts.entry( addr ).or_insert( 0 ) += 1;
            entry.total += 1;
            return;
        }

        // JIT?
        if let Some( idx ) = event.state.jitdump_names().get_index( addr ) {
            let (range, name) = event.state.jitdump_names().get_by_index( idx ).unwrap();
            let key: FuncKey = (FunctionSource::Jit, name.clone());
            let entry = funcs.entry( key ).or_insert_with( || FunctionData {
                range: range.clone(),
                counts: BTreeMap::new(),
                total: 0
            });
            *entry.counts.entry( addr ).or_insert( 0 ) += 1;
            entry.total += 1;
        }
    })?;

    // Architecture gate. yaxpeax has decoders for arm/aarch64/mips that we can
    // wire up later; for now we error out cleanly on anything that isn't
    // x86_64. The architecture string is whatever the recorder captured in
    // MachineInfo (e.g. "amd64", "aarch64", "arm", "mips64").
    let arch = state.architecture();
    if arch != "amd64" {
        return Err( format!(
            "annotate: only x86_64 (amd64) is supported in this version (got '{}')",
            arch
        ).into() );
    }

    if funcs.is_empty() {
        eprintln!( "annotate: no samples landed in known functions" );
        return Ok(());
    }

    let mut chosen: Vec< (FuncKey, FunctionData) > = if args.function.is_empty() {
        let mut v: Vec< _ > = funcs.into_iter().collect();
        v.sort_by( |a, b| b.1.total.cmp( &a.1.total ) );
        v.truncate( args.top.max( 1 ) );
        v
    } else {
        funcs.into_iter()
            .filter( |(k, _)| args.function.iter().any( |needle| k.1.contains( needle ) ) )
            .collect()
    };
    chosen.sort_by( |a, b| b.1.total.cmp( &a.1.total ) );

    if chosen.is_empty() {
        eprintln!( "annotate: no functions matched the --function filter" );
        return Ok(());
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let decoder = InstDecoder::default();

    for ((source, name), data) in chosen {
        match &source {
            FunctionSource::Native( binary_id ) => {
                let binary = state.get_binary( binary_id );
                let label = binary.basename();
                let bytes = match fetch_native_bytes( binary, &data.range ) {
                    Some( b ) => b,
                    None => {
                        writeln!( out, "==== {} [{}]  range 0x{:x}..0x{:x}  total={}  (could not locate code bytes; skipping) ====\n",
                                  name, label, data.range.start, data.range.end, data.total )?;
                        continue;
                    }
                };
                writeln!( out, "==== {} [{}]  range 0x{:x}..0x{:x}  total={} samples ====",
                          name, label, data.range.start, data.range.end, data.total )?;
                writeln!( out, "      count   address       bytes                           asm" )?;
                disassemble_amd64( &decoder, bytes, data.range.start, &data.counts, &mut out )?;
            }
            FunctionSource::Jit => {
                let bytes = match jit_code.get( &data.range.start ) {
                    Some( b ) => b.as_slice(),
                    None => {
                        writeln!( out, "==== {} [JIT]  range 0x{:x}..0x{:x}  total={}  (no jitdump code bytes; pass --jitdump?) ====\n",
                                  name, data.range.start, data.range.end, data.total )?;
                        continue;
                    }
                };
                writeln!( out, "==== {} [JIT]  range 0x{:x}..0x{:x}  total={} samples ====",
                          name, data.range.start, data.range.end, data.total )?;
                writeln!( out, "      count   address       bytes                           asm" )?;
                disassemble_amd64( &decoder, bytes, data.range.start, &data.counts, &mut out )?;
            }
        }
        writeln!( out )?;
    }

    Ok(())
}
