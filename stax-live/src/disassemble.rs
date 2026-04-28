//! Disassemble a resolved function and produce live-annotation lines.
//!
//! The arch comes from the binary itself (Mach-O CPU type / ELF
//! e_machine, captured by the sampler). Falls back to the host arch
//! when the field is missing.

use stax_live_proto::AnnotatedLine;

use yaxpeax_arch::{Decoder, LengthedInstruction, U8Reader};
use yaxpeax_arm::armv8::a64::InstDecoder as Aarch64Decoder;
use yaxpeax_x86::amd64::InstDecoder as Amd64Decoder;

use crate::binaries::ResolvedAddress;
use crate::highlight::AsmHighlighter;

/// Disassemble `resolved`'s bytes and look up per-instruction stats
/// via `self_lookup`, which returns `(self_on_cpu_ns, self_pet_samples)`
/// for the address at each line.
pub fn disassemble(
    resolved: &ResolvedAddress,
    hl: &mut AsmHighlighter,
    mut self_lookup: impl FnMut(u64) -> (u64, u64),
) -> Vec<AnnotatedLine> {
    let arch = resolved.arch.as_deref().unwrap_or(host_arch());
    match arch {
        "aarch64" | "arm64" | "arm64e" => disassemble_aarch64(resolved, hl, &mut self_lookup),
        "amd64" | "x86_64" | "x86_64h" => disassemble_amd64(resolved, hl, &mut self_lookup),
        _ => Vec::new(),
    }
}

fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        "unknown"
    }
}

fn disassemble_aarch64(
    resolved: &ResolvedAddress,
    hl: &mut AsmHighlighter,
    self_lookup: &mut dyn FnMut(u64) -> (u64, u64),
) -> Vec<AnnotatedLine> {
    let decoder = Aarch64Decoder::default();
    let bytes = &resolved.bytes;
    let base = resolved.base_address;
    let mut out = Vec::with_capacity(bytes.len() / 4);
    let mut offset = 0;
    while offset + 4 <= bytes.len() {
        let inst_bytes = &bytes[offset..offset + 4];
        let mut reader = U8Reader::new(inst_bytes);
        let asm = match decoder.decode(&mut reader) {
            Ok(instr) => format!("{}", instr),
            Err(err) => format!("<decode error: {}>", err),
        };
        let address = base + offset as u64;
        let (on_cpu_ns, pet_samples) = self_lookup(address);
        out.push(AnnotatedLine {
            address,
            html: hl.highlight_line(&asm),
            self_on_cpu_ns: on_cpu_ns,
            self_pet_samples: pet_samples,
            source_header: None,
        });
        offset += 4;
    }
    out
}

fn disassemble_amd64(
    resolved: &ResolvedAddress,
    hl: &mut AsmHighlighter,
    self_lookup: &mut dyn FnMut(u64) -> (u64, u64),
) -> Vec<AnnotatedLine> {
    let decoder = Amd64Decoder::default();
    let bytes = &resolved.bytes;
    let base = resolved.base_address;
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let address = base + offset as u64;
        let (on_cpu_ns, pet_samples) = self_lookup(address);
        match decoder.decode_slice(&bytes[offset..]) {
            Ok(instr) => {
                let len = instr.len().to_const() as usize;
                let asm = format!("{}", instr);
                out.push(AnnotatedLine {
                    address,
                    html: hl.highlight_line(&asm),
                    self_on_cpu_ns: on_cpu_ns,
                    self_pet_samples: pet_samples,
                    source_header: None,
                });
                offset += len.max(1);
            }
            Err(err) => {
                out.push(AnnotatedLine {
                    address,
                    html: hl.highlight_line(&format!("<decode error: {}>", err)),
                    self_on_cpu_ns: on_cpu_ns,
                    self_pet_samples: pet_samples,
                    source_header: None,
                });
                offset += 1;
            }
        }
    }
    out
}
