//! Build a function-scoped control-flow graph from disassembled
//! instructions.
//!
//! Walks the same `ResolvedAddress` bytes as `disassemble.rs`, but
//! instead of producing a flat instruction list, classifies each
//! instruction's terminator behaviour, finds the block leaders,
//! and groups instructions into basic blocks with typed edges
//! between them.
//!
//! Edges that leave the function (calls to other symbols, indirect
//! branches, jumps to non-resolved addresses) are not modeled —
//! the CFG is strictly intra-procedural. Conditional branches
//! produce two edges (taken + fallthrough), unconditional branches
//! produce one, returns produce none.

use std::collections::BTreeSet;

use stax_live_proto::{AnnotatedLine, BasicBlock, CfgEdge, CfgEdgeKind, CfgUpdate};
use yaxpeax_arch::{Decoder, LengthedInstruction, U8Reader};

use crate::binaries::ResolvedAddress;
use crate::highlight::TokenHighlighter;

/// `self_lookup(addr)` returns `(self_on_cpu_ns, self_pet_samples)`,
/// same shape as `compute_annotated_view`.
pub fn compute_cfg_update(
    resolved: &ResolvedAddress,
    queried_address: u64,
    function_name: String,
    language: String,
    self_lookup: impl Fn(u64) -> (u64, u64),
) -> CfgUpdate {
    let arch = resolved.arch.as_deref().unwrap_or(host_arch());
    let mut hl = TokenHighlighter::new();
    let instrs: Vec<DecodedInstr> = match arch {
        "aarch64" | "arm64" | "arm64e" => decode_aarch64(resolved, &mut hl, &self_lookup),
        "amd64" | "x86_64" | "x86_64h" => decode_amd64(resolved, &mut hl, &self_lookup),
        _ => Vec::new(),
    };

    let base_address = resolved.base_address;
    let fn_hi = base_address + (resolved.bytes.len() as u64);
    let (blocks, edges) = build_blocks_and_edges(&instrs, base_address, fn_hi);

    CfgUpdate {
        function_name,
        language,
        base_address,
        queried_address,
        blocks,
        edges,
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

/// Per-instruction terminator classification. Drives both block
/// boundary detection and edge emission.
#[derive(Debug, Clone, Copy)]
enum Flow {
    /// Sequential — does not end a block on its own. Block only ends
    /// here if the next instruction is a jump target.
    Sequential,
    /// Unconditional return / trap. End of block, no successors.
    Return,
    /// Indirect branch (computed target, e.g. `BR Xn`, `JMP rax`).
    /// End of block, no successors we can attribute.
    Indirect,
    /// Unconditional branch to a known address.
    Branch { target: u64 },
    /// Conditional branch — taken arm goes to `target`, fall-through
    /// continues sequentially.
    ConditionalBranch { target: u64 },
    /// Direct call — control returns to the next address. We model
    /// it as a fallthrough (the called function is out of scope), but
    /// keep the kind distinct so the renderer can mark it.
    Call,
}

struct DecodedInstr {
    address: u64,
    /// Length in bytes; address of the next instruction is
    /// `address + length`.
    length: u64,
    line: AnnotatedLine,
    flow: Flow,
}

fn decode_aarch64(
    resolved: &ResolvedAddress,
    hl: &mut TokenHighlighter,
    self_lookup: &dyn Fn(u64) -> (u64, u64),
) -> Vec<DecodedInstr> {
    use yaxpeax_arm::armv8::a64::{InstDecoder, Opcode, Operand};

    let decoder = InstDecoder::default();
    let bytes = &resolved.bytes;
    let base = resolved.base_address;
    let mut out = Vec::with_capacity(bytes.len() / 4);
    let mut offset = 0;
    while offset + 4 <= bytes.len() {
        let address = base + offset as u64;
        let mut reader = U8Reader::new(&bytes[offset..offset + 4]);
        let (asm, flow) = match decoder.decode(&mut reader) {
            Ok(instr) => {
                let asm = format!("{}", instr);
                let flow = match instr.opcode {
                    Opcode::B => match instr.operands[0] {
                        Operand::PCOffset(off) => Flow::Branch {
                            target: address.wrapping_add(off as u64),
                        },
                        _ => Flow::Indirect,
                    },
                    Opcode::Bcc(_) => match instr.operands[0] {
                        Operand::PCOffset(off) => Flow::ConditionalBranch {
                            target: address.wrapping_add(off as u64),
                        },
                        _ => Flow::Indirect,
                    },
                    Opcode::CBZ | Opcode::CBNZ => match instr.operands[1] {
                        Operand::PCOffset(off) => Flow::ConditionalBranch {
                            target: address.wrapping_add(off as u64),
                        },
                        _ => Flow::Indirect,
                    },
                    Opcode::TBZ | Opcode::TBNZ => match instr.operands[2] {
                        Operand::PCOffset(off) => Flow::ConditionalBranch {
                            target: address.wrapping_add(off as u64),
                        },
                        _ => Flow::Indirect,
                    },
                    Opcode::BR | Opcode::BRAA | Opcode::BRAAZ | Opcode::BRAB | Opcode::BRABZ => {
                        Flow::Indirect
                    }
                    Opcode::BL => Flow::Call,
                    Opcode::BLR
                    | Opcode::BLRAA
                    | Opcode::BLRAAZ
                    | Opcode::BLRAB
                    | Opcode::BLRABZ => Flow::Call,
                    Opcode::RET | Opcode::RETAA | Opcode::RETAB => Flow::Return,
                    Opcode::ERET | Opcode::ERETAA | Opcode::ERETAB => Flow::Return,
                    _ => Flow::Sequential,
                };
                (asm, flow)
            }
            Err(err) => (format!("<decode error: {}>", err), Flow::Sequential),
        };
        let (on_cpu_ns, pet_samples) = self_lookup(address);
        let line = AnnotatedLine {
            address,
            tokens: hl.highlight_line(&asm),
            self_on_cpu_ns: on_cpu_ns,
            self_pet_samples: pet_samples,
            source_header: None,
        };
        out.push(DecodedInstr {
            address,
            length: 4,
            line,
            flow,
        });
        offset += 4;
    }
    out
}

fn decode_amd64(
    resolved: &ResolvedAddress,
    hl: &mut TokenHighlighter,
    self_lookup: &dyn Fn(u64) -> (u64, u64),
) -> Vec<DecodedInstr> {
    use yaxpeax_x86::amd64::InstDecoder;

    let decoder = InstDecoder::default();
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
                let next = address + len as u64;
                let flow = classify_x86(&instr, next);
                let (on_cpu_ns, pet_samples) = self_lookup(address);
                let line = AnnotatedLine {
                    address,
                    tokens: hl.highlight_line(&asm),
                    self_on_cpu_ns: on_cpu_ns,
                    self_pet_samples: pet_samples,
                    source_header: None,
                };
                out.push(DecodedInstr {
                    address,
                    length: len as u64,
                    line,
                    flow,
                });
                offset += len.max(1);
            }
            Err(err) => {
                let (on_cpu_ns, pet_samples) = self_lookup(address);
                let line = AnnotatedLine {
                    address,
                    tokens: hl.highlight_line(&format!("<decode error: {}>", err)),
                    self_on_cpu_ns: on_cpu_ns,
                    self_pet_samples: pet_samples,
                    source_header: None,
                };
                out.push(DecodedInstr {
                    address,
                    length: 1,
                    line,
                    flow: Flow::Sequential,
                });
                offset += 1;
            }
        }
    }
    out
}

fn classify_x86(instr: &yaxpeax_x86::amd64::Instruction, next_address: u64) -> Flow {
    use yaxpeax_x86::amd64::{Opcode, Operand};
    let target = |op: Operand| -> Option<u64> {
        match op {
            Operand::ImmediateI8 { imm } => Some(next_address.wrapping_add(imm as u64)),
            Operand::ImmediateI16 { imm } => Some(next_address.wrapping_add(imm as u64)),
            Operand::ImmediateI32 { imm } => Some(next_address.wrapping_add(imm as u64)),
            Operand::ImmediateI64 { imm } => Some(next_address.wrapping_add(imm as u64)),
            _ => None,
        }
    };
    match instr.opcode() {
        Opcode::JMP | Opcode::JMPF | Opcode::JMPE => match target(instr.operand(0)) {
            Some(t) => Flow::Branch { target: t },
            None => Flow::Indirect,
        },
        Opcode::JO
        | Opcode::JNO
        | Opcode::JB
        | Opcode::JNB
        | Opcode::JZ
        | Opcode::JNZ
        | Opcode::JA
        | Opcode::JNA
        | Opcode::JS
        | Opcode::JNS
        | Opcode::JP
        | Opcode::JNP
        | Opcode::JL
        | Opcode::JGE
        | Opcode::JLE
        | Opcode::JG
        | Opcode::JRCXZ
        | Opcode::LOOP
        | Opcode::LOOPZ
        | Opcode::LOOPNZ => match target(instr.operand(0)) {
            Some(t) => Flow::ConditionalBranch { target: t },
            None => Flow::Indirect,
        },
        Opcode::CALL | Opcode::CALLF => Flow::Call,
        Opcode::RETURN | Opcode::RETF | Opcode::IRET | Opcode::IRETD | Opcode::IRETQ => {
            Flow::Return
        }
        Opcode::HLT | Opcode::UD2 => Flow::Return,
        _ => Flow::Sequential,
    }
}

fn build_blocks_and_edges(
    instrs: &[DecodedInstr],
    fn_lo: u64,
    fn_hi: u64,
) -> (Vec<BasicBlock>, Vec<CfgEdge>) {
    if instrs.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Step 1: collect block leaders.
    //
    //   - the first instruction
    //   - every in-function branch target
    //   - every instruction immediately after a branch / return /
    //     indirect, even if no jump targets it (avoids merging the
    //     dead-fallthrough into the previous block)
    let mut leaders: BTreeSet<u64> = BTreeSet::new();
    leaders.insert(instrs[0].address);
    let in_function = |target: u64| -> bool { target >= fn_lo && target < fn_hi };
    for instr in instrs {
        match instr.flow {
            Flow::Branch { target } | Flow::ConditionalBranch { target } => {
                if in_function(target) {
                    leaders.insert(target);
                }
            }
            _ => {}
        }
        let next = instr.address + instr.length;
        if next >= fn_hi {
            continue;
        }
        match instr.flow {
            Flow::Branch { .. }
            | Flow::ConditionalBranch { .. }
            | Flow::Return
            | Flow::Indirect => {
                leaders.insert(next);
            }
            _ => {}
        }
    }

    // Step 2: group instructions into blocks. A block runs from one
    // leader up to (but not including) the next leader. Instructions
    // that aren't preceded by a leader fall into the current block.
    let leader_vec: Vec<u64> = leaders.iter().copied().collect();
    let mut blocks: Vec<BasicBlock> = leader_vec
        .iter()
        .enumerate()
        .map(|(i, &addr)| BasicBlock {
            id: i as u32,
            start_address: addr,
            lines: Vec::new(),
        })
        .collect();

    let block_id_for = |addr: u64| -> Option<u32> {
        // Last leader <= addr.
        match leader_vec.binary_search(&addr) {
            Ok(i) => Some(i as u32),
            Err(0) => None,
            Err(i) => Some((i - 1) as u32),
        }
    };

    for instr in instrs {
        if let Some(id) = block_id_for(instr.address) {
            blocks[id as usize].lines.push(instr.line.clone());
        }
    }

    // Step 3: emit edges off the last instruction of each block.
    let mut edges: Vec<CfgEdge> = Vec::new();
    for block in &blocks {
        let Some(last_addr) = block.lines.last().map(|l| l.address) else {
            continue;
        };
        let Some(decoded) = instrs.iter().find(|i| i.address == last_addr) else {
            continue;
        };
        let next = decoded.address + decoded.length;
        let next_block = block_id_for(next).filter(|_| next < fn_hi);
        match decoded.flow {
            Flow::Sequential | Flow::Call => {
                if let Some(to) = next_block {
                    edges.push(CfgEdge {
                        from_id: block.id,
                        to_id: to,
                        kind: if matches!(decoded.flow, Flow::Call) {
                            CfgEdgeKind::Call
                        } else {
                            CfgEdgeKind::Fallthrough
                        },
                    });
                }
            }
            Flow::Branch { target } => {
                if let Some(to) = block_id_for(target).filter(|_| in_function(target)) {
                    edges.push(CfgEdge {
                        from_id: block.id,
                        to_id: to,
                        kind: CfgEdgeKind::Branch,
                    });
                }
            }
            Flow::ConditionalBranch { target } => {
                if let Some(to) = block_id_for(target).filter(|_| in_function(target)) {
                    edges.push(CfgEdge {
                        from_id: block.id,
                        to_id: to,
                        kind: CfgEdgeKind::ConditionalBranch,
                    });
                }
                if let Some(to) = next_block {
                    edges.push(CfgEdge {
                        from_id: block.id,
                        to_id: to,
                        kind: CfgEdgeKind::Fallthrough,
                    });
                }
            }
            Flow::Return | Flow::Indirect => {}
        }
    }

    (blocks, edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binaries::ResolvedAddress;

    fn synth(arch: &str, base: u64, bytes: Vec<u8>) -> ResolvedAddress {
        let end = base + bytes.len() as u64;
        ResolvedAddress {
            base_address: base,
            end_address: end,
            fn_start_svma: base,
            bytes,
            function_name: "test".into(),
            binary_path: String::new(),
            arch: Some(arch.into()),
            language: stax_demangle::Language::Unknown,
            image: None,
        }
    }

    #[test]
    fn aarch64_simple_branch() {
        // Two instructions: B +4 (skip the next), then a NOP.
        // Encoded little-endian.
        // B +4: 0x14000001 → bytes 01 00 00 14
        // NOP:  0xd503201f → bytes 1f 20 03 d5
        let bytes = vec![0x01, 0x00, 0x00, 0x14, 0x1f, 0x20, 0x03, 0xd5];
        let resolved = synth("aarch64", 0x1000, bytes);
        let cfg = compute_cfg_update(&resolved, 0x1000, "test".into(), "rust".into(), |_| (0, 0));
        // B is a branch terminator → the B instr is its own block,
        // its target is 0x1004 → also a leader; so two blocks.
        assert_eq!(cfg.blocks.len(), 2);
        assert_eq!(cfg.blocks[0].start_address, 0x1000);
        assert_eq!(cfg.blocks[1].start_address, 0x1004);
        assert_eq!(cfg.edges.len(), 1);
        assert_eq!(cfg.edges[0].kind, CfgEdgeKind::Branch);
        assert_eq!(cfg.edges[0].from_id, 0);
        assert_eq!(cfg.edges[0].to_id, 1);
    }
}
