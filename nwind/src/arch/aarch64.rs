use crate::address_space::{lookup_binary, MemoryReader};
use crate::arch::{Architecture, Registers, UnwindFailure, UnwindMode, UnwindStatus};
use crate::dwarf::{dwarf_unwind, dwarf_unwind_with_info, DwarfResult};
use crate::frame_descriptions::{ContextCache, UnwindInfoCache};
use crate::types::{Bitness, Endianness};
use gimli::LittleEndian;
use macho_unwind_info::opcodes::OpcodeArm64;

// Source: DWARF for the ARM 64-bit, 3.1 DWARF register names
//         http://infocenter.arm.com/help/topic/com.arm.doc.ihi0057b/IHI0057B_aadwarf64.pdf
pub mod dwarf {
    pub const X0: u16 = 0;
    pub const X1: u16 = 1;
    pub const X2: u16 = 2;
    pub const X3: u16 = 3;
    pub const X4: u16 = 4;
    pub const X5: u16 = 5;
    pub const X6: u16 = 6;
    pub const X7: u16 = 7;
    pub const X8: u16 = 8;
    pub const X9: u16 = 9;
    pub const X10: u16 = 10;
    pub const X11: u16 = 11;
    pub const X12: u16 = 12;
    pub const X13: u16 = 13;
    pub const X14: u16 = 14;
    pub const X15: u16 = 15;
    pub const X16: u16 = 16;
    pub const X17: u16 = 17;
    pub const X18: u16 = 18;
    pub const X19: u16 = 19;
    pub const X20: u16 = 20;
    pub const X21: u16 = 21;
    pub const X22: u16 = 22;
    pub const X23: u16 = 23;
    pub const X24: u16 = 24;
    pub const X25: u16 = 25;
    pub const X26: u16 = 26;
    pub const X27: u16 = 27;
    pub const X28: u16 = 28;
    pub const X29: u16 = 29;
    pub const X30: u16 = 30;
    pub const X31: u16 = 31;

    pub const PC: u16 = 32;
}

static REGS: &'static [u16] = &[
    dwarf::X0,
    dwarf::X1,
    dwarf::X2,
    dwarf::X3,
    dwarf::X4,
    dwarf::X5,
    dwarf::X6,
    dwarf::X7,
    dwarf::X8,
    dwarf::X9,
    dwarf::X10,
    dwarf::X11,
    dwarf::X12,
    dwarf::X13,
    dwarf::X14,
    dwarf::X15,
    dwarf::X16,
    dwarf::X17,
    dwarf::X18,
    dwarf::X19,
    dwarf::X20,
    dwarf::X21,
    dwarf::X22,
    dwarf::X23,
    dwarf::X24,
    dwarf::X25,
    dwarf::X26,
    dwarf::X27,
    dwarf::X28,
    dwarf::X29,
    dwarf::X30,
    dwarf::X31,
    dwarf::PC,
];

#[repr(C)]
#[derive(Clone, Default)]
pub struct Regs {
    x0: u64,
    x1: u64,
    x2: u64,
    x3: u64,
    x4: u64,
    x5: u64,
    x6: u64,
    x7: u64,
    x8: u64,
    x9: u64,
    x10: u64,
    x11: u64,
    x12: u64,
    x13: u64,
    x14: u64,
    x15: u64,
    x16: u64,
    x17: u64,
    x18: u64,
    x19: u64,
    x20: u64,
    x21: u64,
    x22: u64,
    x23: u64,
    x24: u64,
    x25: u64,
    x26: u64,
    x27: u64,
    x28: u64,
    x29: u64,
    x30: u64,
    x31: u64,

    pc: u64,

    mask: u64,
}

unsafe_impl_registers!(Regs, REGS, u64);
impl_local_regs!(Regs, "aarch64", get_regs_aarch64);
impl_regs_debug!(Regs, REGS, Arch);

#[allow(dead_code)]
pub struct Arch {}

#[doc(hidden)]
pub struct State {
    ctx_cache: ContextCache<LittleEndian>,
    unwind_cache: UnwindInfoCache,
    new_regs: Vec<(u16, u64)>,
}

fn checked_add(a: u64, b: u64) -> Result<u64, UnwindFailure> {
    a.checked_add(b)
        .ok_or(UnwindFailure::RegisterMemoryReadFailed)
}

fn read_u64<M: MemoryReader<Arch>>(memory: &M, address: u64) -> Result<u64, UnwindFailure> {
    memory
        .get_pointer_at_address(address)
        .ok_or(UnwindFailure::RegisterMemoryReadFailed)
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
fn strip_code_ptr(mut ptr: u64) -> u64 {
    unsafe {
        std::arch::asm!(
            "xpaci {ptr}",
            ptr = inout(reg) ptr,
            options(nomem, nostack, preserves_flags)
        );
    }
    ptr
}

#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
fn strip_code_ptr(ptr: u64) -> u64 {
    ptr
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
fn strip_data_ptr(mut ptr: u64) -> u64 {
    unsafe {
        std::arch::asm!(
            "xpacd {ptr}",
            ptr = inout(reg) ptr,
            options(nomem, nostack, preserves_flags)
        );
    }
    ptr
}

#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
fn strip_data_ptr(ptr: u64) -> u64 {
    ptr
}

fn append_saved_pair<M: MemoryReader<Arch>>(
    memory: &M,
    new_regs: &mut Vec<(u16, u64)>,
    cfa: u64,
    offset: &mut u64,
    saved: bool,
    regs: Option<(u16, u16)>,
) {
    if !saved {
        return;
    }
    if let Some((first, second)) = regs {
        if let Some(address) = cfa.checked_sub(*offset) {
            if let Ok(value) = read_u64(memory, address) {
                new_regs.push((first, value));
            }
        }
        if let Some(address) = cfa.checked_sub(offset.saturating_add(8)) {
            if let Ok(value) = read_u64(memory, address) {
                new_regs.push((second, value));
            }
        }
    }
    *offset = offset.saturating_add(16);
}

fn compact_unwind<M: MemoryReader<Arch>>(
    nth_frame: usize,
    memory: &M,
    ctx_cache: &mut ContextCache<LittleEndian>,
    regs: &Regs,
    new_regs: &mut Vec<(u16, u64)>,
    allow_dwarf_fde: bool,
) -> Result<DwarfResult, UnwindFailure> {
    new_regs.clear();

    let pc = regs
        .get(dwarf::PC)
        .ok_or(UnwindFailure::MissingInstructionPointer)?;
    if pc == 0 {
        return Err(UnwindFailure::NullInstructionPointer);
    }

    let lookup_address = if nth_frame == 0 { pc } else { pc - 1 };
    let binary = lookup_binary(nth_frame, memory, regs).ok_or(UnwindFailure::NoBinary)?;
    let entry = binary
        .lookup_macho_compact_unwind(lookup_address)
        .ok_or(UnwindFailure::NoUnwindInfo)?;

    match OpcodeArm64::parse(entry.opcode) {
        OpcodeArm64::Frameless {
            stack_size_in_bytes,
        } => {
            let sp = regs
                .get(dwarf::X31)
                .ok_or(UnwindFailure::MissingCfaRegister)?;
            let lr = strip_code_ptr(
                regs.get(dwarf::X30)
                    .ok_or(UnwindFailure::MissingReturnAddress)?,
            );
            new_regs.push((dwarf::X30, lr));
            Ok(DwarfResult {
                initial_address: entry.initial_address,
                cfa: checked_add(sp, stack_size_in_bytes as u64)?,
                ra_address: None,
                return_address_error: None,
            })
        }
        OpcodeArm64::FrameBased {
            d14_and_d15_saved,
            d12_and_d13_saved,
            d10_and_d11_saved,
            d8_and_d9_saved,
            x27_and_x28_saved,
            x25_and_x26_saved,
            x23_and_x24_saved,
            x21_and_x22_saved,
            x19_and_x20_saved,
            ..
        } => {
            let fp = strip_data_ptr(
                regs.get(dwarf::X29)
                    .ok_or(UnwindFailure::MissingCfaRegister)?,
            );
            if fp == 0 {
                return Err(UnwindFailure::MissingCfaRegister);
            }

            let cfa = checked_add(fp, 16)?;
            let next_fp = strip_data_ptr(read_u64(memory, fp)?);
            let ra_address = checked_add(fp, 8)?;
            let next_lr = strip_code_ptr(read_u64(memory, ra_address)?);
            new_regs.push((dwarf::X29, next_fp));
            new_regs.push((dwarf::X30, next_lr));

            let mut offset = 32;
            append_saved_pair(memory, new_regs, cfa, &mut offset, d14_and_d15_saved, None);
            append_saved_pair(memory, new_regs, cfa, &mut offset, d12_and_d13_saved, None);
            append_saved_pair(memory, new_regs, cfa, &mut offset, d10_and_d11_saved, None);
            append_saved_pair(memory, new_regs, cfa, &mut offset, d8_and_d9_saved, None);
            append_saved_pair(
                memory,
                new_regs,
                cfa,
                &mut offset,
                x27_and_x28_saved,
                Some((dwarf::X27, dwarf::X28)),
            );
            append_saved_pair(
                memory,
                new_regs,
                cfa,
                &mut offset,
                x25_and_x26_saved,
                Some((dwarf::X25, dwarf::X26)),
            );
            append_saved_pair(
                memory,
                new_regs,
                cfa,
                &mut offset,
                x23_and_x24_saved,
                Some((dwarf::X23, dwarf::X24)),
            );
            append_saved_pair(
                memory,
                new_regs,
                cfa,
                &mut offset,
                x21_and_x22_saved,
                Some((dwarf::X21, dwarf::X22)),
            );
            append_saved_pair(
                memory,
                new_regs,
                cfa,
                &mut offset,
                x19_and_x20_saved,
                Some((dwarf::X19, dwarf::X20)),
            );

            Ok(DwarfResult {
                initial_address: entry.initial_address,
                cfa,
                ra_address: Some(ra_address),
                return_address_error: None,
            })
        }
        OpcodeArm64::Dwarf { eh_frame_fde } => {
            if !allow_dwarf_fde {
                return Err(UnwindFailure::NoUnwindInfo);
            }
            let unwind_info = binary
                .lookup_eh_unwind_row_by_fde_offset(ctx_cache, lookup_address, eh_frame_fde)
                .ok_or(UnwindFailure::NoUnwindInfo)?;
            dwarf_unwind_with_info(nth_frame, memory, regs, &unwind_info, new_regs)
        }
        OpcodeArm64::Null | OpcodeArm64::UnrecognizedKind(_) => Err(UnwindFailure::NoUnwindInfo),
    }
}

impl Architecture for Arch {
    const NAME: &'static str = "aarch64";
    const ENDIANNESS: Endianness = Endianness::LittleEndian;
    const BITNESS: Bitness = Bitness::B64;
    const STACK_POINTER_REG: u16 = dwarf::X31;
    const INSTRUCTION_POINTER_REG: u16 = dwarf::PC;
    const RETURN_ADDRESS_REG: u16 = dwarf::X30;

    type Endianity = LittleEndian;
    type State = State;
    type Regs = Regs;
    type RegTy = u64;

    fn register_name_str(register: u16) -> Option<&'static str> {
        use self::dwarf::*;

        let name = match register {
            X0 => "X0",
            X1 => "X1",
            X2 => "X2",
            X3 => "X3",
            X4 => "X4",
            X5 => "X5",
            X6 => "X6",
            X7 => "X7",
            X8 => "X8",
            X9 => "X9",
            X10 => "X10",
            X11 => "X11",
            X12 => "X12",
            X13 => "X13",
            X14 => "X14",
            X15 => "X15",
            X16 => "X16",
            X17 => "X17",
            X18 => "X18",
            X19 => "X19",
            X20 => "X20",
            X21 => "X21",
            X22 => "X22",
            X23 => "X23",
            X24 => "X24",
            X25 => "X25",
            X26 => "X26",
            X27 => "X27",
            X28 => "X28",
            X29 => "X29",
            X30 => "LR",
            X31 => "SP",
            _ => return None,
        };

        Some(name)
    }

    #[inline]
    fn initial_state() -> Self::State {
        State {
            ctx_cache: ContextCache::new(),
            unwind_cache: UnwindInfoCache::new(),
            new_regs: Vec::with_capacity(32),
        }
    }

    fn clear_cache(state: &mut Self::State) {
        state.unwind_cache.clear();
    }

    #[inline]
    fn unwind<M: MemoryReader<Self>>(
        nth_frame: usize,
        memory: &M,
        state: &mut Self::State,
        regs: &mut Self::Regs,
        initial_address: &mut Option<u64>,
        ra_address: &mut Option<u64>,
    ) -> Result<UnwindStatus, UnwindFailure> {
        Self::unwind_with_mode(
            nth_frame,
            memory,
            state,
            regs,
            initial_address,
            ra_address,
            UnwindMode::Default,
        )
    }

    fn unwind_with_mode<M: MemoryReader<Self>>(
        nth_frame: usize,
        memory: &M,
        state: &mut Self::State,
        regs: &mut Self::Regs,
        initial_address: &mut Option<u64>,
        ra_address: &mut Option<u64>,
        mode: UnwindMode,
    ) -> Result<UnwindStatus, UnwindFailure> {
        let result = match mode {
            UnwindMode::Default => match dwarf_unwind(
                nth_frame,
                memory,
                &mut state.ctx_cache,
                &mut state.unwind_cache,
                regs,
                &mut state.new_regs,
            ) {
                Ok(result) => result,
                Err(UnwindFailure::NoUnwindInfo) => compact_unwind(
                    nth_frame,
                    memory,
                    &mut state.ctx_cache,
                    regs,
                    &mut state.new_regs,
                    true,
                )?,
                Err(error) => return Err(error),
            },
            UnwindMode::DwarfOnly => dwarf_unwind(
                nth_frame,
                memory,
                &mut state.ctx_cache,
                &mut state.unwind_cache,
                regs,
                &mut state.new_regs,
            )?,
            UnwindMode::CompactOnly => compact_unwind(
                nth_frame,
                memory,
                &mut state.ctx_cache,
                regs,
                &mut state.new_regs,
                false,
            )?,
            UnwindMode::CompactWithDwarfRefs => compact_unwind(
                nth_frame,
                memory,
                &mut state.ctx_cache,
                regs,
                &mut state.new_regs,
                true,
            )?,
        };
        *initial_address = Some(result.initial_address);
        *ra_address = result.ra_address;
        let cfa = result.cfa;

        let mut recovered_return_address = false;
        for &(register, value) in &state.new_regs {
            regs.append(register, value);

            recovered_return_address = recovered_return_address || register == dwarf::X30;
        }

        regs.append(dwarf::X31, cfa);

        debug!(
            "Register {:?} at frame #{} is equal to 0x{:016X}",
            Self::register_name(dwarf::X31),
            nth_frame + 1,
            cfa
        );

        if recovered_return_address || nth_frame == 0 {
            regs.pc = regs.x30;
            Ok(UnwindStatus::InProgress)
        } else {
            debug!(
                "Previous frame not found: failed to determine the return address of frame #{}",
                nth_frame + 1
            );
            Err(result
                .return_address_error
                .unwrap_or(UnwindFailure::MissingReturnAddress))
        }
    }
}
