//! Incremental jitdump reader.
//!
//! Cranelift / wasmtime / V8 / perf-map-agent all write a stream of
//! records to `/tmp/jit-<pid>.dump`. The file grows during the
//! target's lifetime; the existing offline path embeds the whole
//! thing at end-of-recording so `nperf collate` can resolve JIT
//! addresses post-hoc, but the live UI saw only `(no binary)` for
//! every JIT'd function.
//!
//! This tailer opens the file once it appears, parses the global
//! header, then on each `tick()` reads everything appended since
//! the last call and returns the new `CodeLoad` records. The
//! recorder can then emit synthetic `BinaryLoadedEvent`s into the
//! live sink so JIT'd functions show up in the flame graph and
//! top table by name as soon as the runtime emits them.
//!
//! Format reference: linux/tools/perf/Documentation/jitdump-specification.txt

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Magic bytes the jitdump producer writes verbatim. Big-endian
/// 0x4A695444 ("JiTD"); we read it as LE because most current
/// runtimes emit little-endian on x86_64/aarch64.
const JITDUMP_MAGIC_LE: u32 = 0x4474_694A;
const JITDUMP_MAGIC_BE: u32 = 0x4A69_5444;

/// Record id 0: code load. Other ids (close, debug-info, unwind-info)
/// exist but we don't yet surface them in the live UI.
const JIT_CODE_LOAD: u32 = 0;

#[derive(Debug, Clone)]
pub struct JitCodeLoad {
    /// Runtime virtual address (AVMA) where the function was loaded.
    pub avma: u64,
    /// Function size in bytes.
    pub code_size: u64,
    /// Function name. Free-form bytes from the runtime; usually
    /// UTF-8 demangled-ish (cranelift uses `function_<n>` or its
    /// IR symbol name; V8 uses the JS source name).
    pub name: String,
    /// Raw machine-code bytes the runtime emitted for this
    /// function. Same length as `code_size`. We carry these so the
    /// live UI can disassemble JIT'd code without needing
    /// `task_for_pid` / `mach_vm_read` against the target.
    pub code: Vec<u8>,
}

pub struct JitdumpTailer {
    file: File,
    /// Bytes consumed from the file so far (header + N records).
    offset: u64,
    /// `false` until we successfully validate the global header; we
    /// don't try to interpret records before that.
    header_parsed: bool,
}

impl JitdumpTailer {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            file,
            offset: 0,
            header_parsed: false,
        })
    }

    /// Read whatever the runtime has appended since the previous
    /// call and return any new `CodeLoad` records. Other record
    /// types are skipped silently. Partial records at the tail (the
    /// file is being actively written) are NOT consumed -- we'll
    /// pick them up on the next tick.
    pub fn tick(&mut self) -> io::Result<Vec<JitCodeLoad>> {
        let len = self.file.metadata()?.len();
        if len <= self.offset {
            return Ok(Vec::new());
        }
        let to_read = (len - self.offset) as usize;
        let prev_offset = self.offset;
        let mut buf = vec![0u8; to_read];
        self.file.seek(SeekFrom::Start(self.offset))?;
        self.file.read_exact(&mut buf)?;

        let mut out = Vec::new();
        let mut cursor = 0usize;

        if !self.header_parsed {
            // Global header is 40 bytes. Need that much before we
            // can validate the magic.
            if buf.len() < 40 {
                return Ok(Vec::new());
            }
            let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
            if magic != JITDUMP_MAGIC_LE && magic != JITDUMP_MAGIC_BE {
                log::warn!(
                    "jitdump_tail: unrecognised magic {:#x}; treating file as malformed",
                    magic
                );
                self.offset = len; // don't keep retrying
                return Ok(Vec::new());
            }
            cursor = 40;
            self.header_parsed = true;
        }

        // Each record starts with `id u32 + total_size u32 + timestamp u64`.
        while cursor + 16 <= buf.len() {
            let id = u32::from_le_bytes(buf[cursor..cursor + 4].try_into().unwrap());
            let total_size =
                u32::from_le_bytes(buf[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
            if total_size < 16 || cursor + total_size > buf.len() {
                // Either malformed or the runtime hasn't finished
                // writing this record yet. Stop and rewind so the
                // next tick re-reads from this point.
                break;
            }
            if id == JIT_CODE_LOAD {
                if let Some(rec) = parse_code_load(&buf[cursor + 16..cursor + total_size]) {
                    log::warn!(
                        "jitdump_tail: CodeLoad name={:?} avma={:#x} size={:#x} (range {:#x}..{:#x})",
                        rec.name,
                        rec.avma,
                        rec.code_size,
                        rec.avma,
                        rec.avma + rec.code_size,
                    );
                    out.push(rec);
                }
            } else {
                log::warn!(
                    "jitdump_tail: skipping record id={id} size={total_size}"
                );
            }
            cursor += total_size;
        }

        self.offset += cursor as u64;
        log::warn!(
            "jitdump_tail tick: read {to_read}B (offset {prev_offset} -> {} of {len}), \
             produced {} CodeLoad records",
            self.offset,
            out.len(),
        );
        Ok(out)
    }
}

fn parse_code_load(payload: &[u8]) -> Option<JitCodeLoad> {
    // pid u32, tid u32, vma u64, code_addr u64, code_size u64, code_index u64
    // then null-terminated name, then code_size bytes of code.
    if payload.len() < 40 {
        return None;
    }
    let avma = u64::from_le_bytes(payload[8..16].try_into().ok()?);
    let code_size = u64::from_le_bytes(payload[24..32].try_into().ok()?);
    let name_bytes = &payload[40..];
    let nul = name_bytes.iter().position(|&b| b == 0)?;
    let name = String::from_utf8_lossy(&name_bytes[..nul]).into_owned();
    let code_start = 40 + nul + 1;
    let code_end = code_start + code_size as usize;
    let code = if code_end <= payload.len() {
        payload[code_start..code_end].to_vec()
    } else {
        Vec::new()
    };
    Some(JitCodeLoad {
        avma,
        code_size,
        name,
        code,
    })
}
