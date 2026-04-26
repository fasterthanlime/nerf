//! Disassemble a resolved function and produce live-annotation lines.
//!
//! The arch comes from the binary itself (Mach-O CPU type / ELF
//! e_machine, captured by the sampler). Falls back to the host arch
//! when the field is missing.

use nperf_live_proto::AnnotatedLine;

use yaxpeax_arch::{Decoder, LengthedInstruction, U8Reader};
use yaxpeax_arm::armv8::a64::InstDecoder as Aarch64Decoder;
use yaxpeax_x86::amd64::InstDecoder as Amd64Decoder;

use crate::binaries::ResolvedAddress;
use crate::highlight::AsmHighlighter;

pub fn disassemble(
    resolved: &ResolvedAddress,
    hl: &mut AsmHighlighter,
    mut self_count: impl FnMut(u64) -> u64,
) -> Vec<AnnotatedLine> {
    let arch = resolved
        .arch
        .as_deref()
        .unwrap_or(host_arch());
    match arch {
        "aarch64" | "arm64" | "arm64e" => {
            disassemble_aarch64(resolved, hl, &mut self_count)
        }
        "amd64" | "x86_64" | "x86_64h" => disassemble_amd64(resolved, hl, &mut self_count),
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
    self_count: &mut dyn FnMut(u64) -> u64,
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
        out.push(AnnotatedLine {
            address,
            html: hl.highlight_line(&asm),
            self_count: self_count(address),
            source_header: None,
        });
        offset += 4;
    }
    out
}

fn disassemble_amd64(
    resolved: &ResolvedAddress,
    hl: &mut AsmHighlighter,
    self_count: &mut dyn FnMut(u64) -> u64,
) -> Vec<AnnotatedLine> {
    let decoder = Amd64Decoder::default();
    let bytes = &resolved.bytes;
    let base = resolved.base_address;
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let address = base + offset as u64;
        match decoder.decode_slice(&bytes[offset..]) {
            Ok(instr) => {
                let len = instr.len().to_const() as usize;
                let asm = format!("{}", instr);
                out.push(AnnotatedLine {
                    address,
                    html: hl.highlight_line(&asm),
                    self_count: self_count(address),
                    source_header: None,
                });
                offset += len.max(1);
            }
            Err(err) => {
                out.push(AnnotatedLine {
                    address,
                    html: hl.highlight_line(&format!("<decode error: {}>", err)),
                    self_count: self_count(address),
                    source_header: None,
                });
                offset += 1;
            }
        }
    }
    out
}
