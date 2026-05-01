use std::convert::TryInto;
use std::sync::Arc;

use nwind::proc_maps::Region;

use nwind::arch::{self, Architecture, Registers};
use nwind::{
    AddressSpace, BinaryData, DwarfRegs, IAddressSpace, LoadHint, Primitive, UnwindFailure,
    UnwindMode, UserFrame,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedImageMapping {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub is_read: bool,
    pub is_write: bool,
    pub is_executable: bool,
    pub path: String,
}

impl CapturedImageMapping {
    pub fn executable_text(
        path: impl Into<String>,
        start: u64,
        size: u64,
        file_offset: u64,
    ) -> Self {
        Self {
            start,
            end: start.saturating_add(size),
            file_offset,
            is_read: true,
            is_write: false,
            is_executable: true,
            path: path.into(),
        }
    }

    fn to_region(&self) -> Region {
        Region {
            start: self.start,
            end: self.end,
            is_read: self.is_read,
            is_write: self.is_write,
            is_executable: self.is_executable,
            is_shared: false,
            file_offset: self.file_offset,
            major: 0,
            minor: 0,
            // AddressSpace::reload intentionally skips inode 0 mappings.
            // major/minor 0 still keys by path, so this is only an
            // "eligible for reload" marker for synthetic captured maps.
            inode: 1,
            name: self.path.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedLoadFailure {
    pub path: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedReload {
    pub mapped_regions: usize,
    pub loaded_binaries: usize,
    pub load_failures: Vec<CapturedLoadFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedUnwindError {
    NoMappings,
    NoMappedRegions,
    MissingStackPointer,
    MissingInstructionPointer,
    EmptyStack,
    OnlyLeafFrame { reason: Option<UnwindFailure> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedThreadState {
    pub pc: u64,
    pub lr: u64,
    pub fp: u64,
    pub sp: u64,
}

impl CapturedThreadState {
    pub fn new(pc: u64, lr: u64, fp: u64, sp: u64) -> Self {
        Self {
            pc: strip_code_pointer(pc),
            lr: strip_code_pointer(lr),
            fp: strip_data_pointer(fp),
            sp: strip_data_pointer(sp),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedStack<'a> {
    pub base: u64,
    pub bytes: &'a [u8],
}

impl<'a> CapturedStack<'a> {
    pub fn new(base: u64, bytes: &'a [u8]) -> Self {
        Self {
            base: strip_data_pointer(base),
            bytes,
        }
    }

    fn read_u64(&self, address: u64) -> Option<u64> {
        let offset = address.checked_sub(self.base)? as usize;
        let end = offset.checked_add(8)?;
        let bytes = self.bytes.get(offset..end)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedBridgePolicy {
    Never,
    AnyOnlyLeaf,
    OnlyNoBinaryOrNoUnwindInfo,
}

impl CapturedBridgePolicy {
    fn should_bridge(self, error: &CapturedUnwindError) -> bool {
        match self {
            Self::Never => false,
            Self::AnyOnlyLeaf => matches!(error, CapturedUnwindError::OnlyLeafFrame { .. }),
            Self::OnlyNoBinaryOrNoUnwindInfo => matches!(
                error,
                CapturedUnwindError::OnlyLeafFrame {
                    reason: Some(UnwindFailure::NoBinary | UnwindFailure::NoUnwindInfo)
                }
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedUnwindOptions {
    pub mode: UnwindMode,
    pub bridge: CapturedBridgePolicy,
    pub max_frames: usize,
}

impl CapturedUnwindOptions {
    pub const DEFAULT_MAX_FRAMES: usize = 64;

    pub fn metadata(mode: UnwindMode) -> Self {
        Self {
            mode,
            bridge: CapturedBridgePolicy::AnyOnlyLeaf,
            max_frames: Self::DEFAULT_MAX_FRAMES,
        }
    }

    pub fn dwarf_with_fp_bridge() -> Self {
        Self {
            mode: UnwindMode::Default,
            bridge: CapturedBridgePolicy::OnlyNoBinaryOrNoUnwindInfo,
            max_frames: Self::DEFAULT_MAX_FRAMES,
        }
    }
}

impl Default for CapturedUnwindOptions {
    fn default() -> Self {
        Self {
            mode: UnwindMode::Default,
            bridge: CapturedBridgePolicy::Never,
            max_frames: Self::DEFAULT_MAX_FRAMES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedUnwindOutcome {
    pub callers: Vec<u64>,
    pub bridge_attempted: bool,
    pub bridge_steps: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedUnwindFailure {
    pub error: CapturedUnwindError,
    pub bridge_attempted: bool,
    pub bridge_steps: usize,
}

pub struct CapturedStackUnwinder<A: Architecture = arch::native::Arch> {
    address_space: AddressSpace<A>,
    mappings: Vec<CapturedImageMapping>,
    dirty: bool,
    last_reload: CapturedReload,
}

impl<A> CapturedStackUnwinder<A>
where
    A: Architecture,
    A::RegTy: Primitive,
{
    pub fn new() -> Self {
        Self {
            address_space: AddressSpace::new(),
            mappings: Vec::new(),
            dirty: false,
            last_reload: CapturedReload::default(),
        }
    }

    pub fn set_mappings(&mut self, mappings: impl IntoIterator<Item = CapturedImageMapping>) {
        self.mappings = mappings.into_iter().collect();
        self.dirty = true;
    }

    pub fn add_mapping(&mut self, mapping: CapturedImageMapping) {
        self.mappings
            .retain(|existing| existing.start != mapping.start || existing.path != mapping.path);
        self.mappings.push(mapping);
        self.dirty = true;
    }

    pub fn remove_mapping_by_start(&mut self, start: u64) {
        let old_len = self.mappings.len();
        self.mappings.retain(|mapping| mapping.start != start);
        if self.mappings.len() != old_len {
            self.dirty = true;
        }
    }

    pub fn last_reload(&self) -> &CapturedReload {
        &self.last_reload
    }

    pub fn reload_if_dirty(&mut self) -> &CapturedReload {
        if !self.dirty {
            return &self.last_reload;
        }

        let regions: Vec<_> = self
            .mappings
            .iter()
            .filter(|mapping| mapping.start < mapping.end && !mapping.path.is_empty())
            .map(CapturedImageMapping::to_region)
            .collect();
        let mut loaded_binaries = 0;
        let mut load_failures = Vec::new();
        let reloaded =
            self.address_space.reload(
                regions,
                &mut |region, handle| match BinaryData::load_from_fs(&region.name) {
                    Ok(binary) => {
                        loaded_binaries += 1;
                        handle.set_binary(Arc::new(binary));
                        handle.should_load_symbols(false);
                        handle.should_load_frame_descriptions(true);
                        handle.should_use_eh_frame_hdr(true);
                        handle.should_load_eh_frame(LoadHint::WhenNecessary);
                        handle.should_load_debug_frame(true);
                    }
                    Err(error) => load_failures.push(CapturedLoadFailure {
                        path: region.name.clone(),
                        error: error.to_string(),
                    }),
                },
            );

        self.last_reload = CapturedReload {
            mapped_regions: reloaded.regions_mapped.len(),
            loaded_binaries,
            load_failures,
        };
        self.dirty = false;
        &self.last_reload
    }

    pub fn unwind_into(
        &mut self,
        regs: &mut DwarfRegs,
        stack: &[u8],
        output: &mut Vec<UserFrame>,
    ) -> Result<(), CapturedUnwindError> {
        self.unwind_into_with_mode(regs, stack, output, UnwindMode::Default)
    }

    pub fn unwind_into_at(
        &mut self,
        regs: &mut DwarfRegs,
        stack_base: u64,
        stack: &[u8],
        output: &mut Vec<UserFrame>,
    ) -> Result<(), CapturedUnwindError> {
        self.unwind_into_with_mode_at(regs, stack_base, stack, output, UnwindMode::Default)
    }

    pub fn unwind_into_with_mode(
        &mut self,
        regs: &mut DwarfRegs,
        stack: &[u8],
        output: &mut Vec<UserFrame>,
        mode: UnwindMode,
    ) -> Result<(), CapturedUnwindError> {
        let stack_base = match regs.get(A::STACK_POINTER_REG) {
            Some(address) => address,
            None => {
                output.clear();
                return Err(CapturedUnwindError::MissingStackPointer);
            }
        };
        self.unwind_into_with_mode_at(regs, stack_base, stack, output, mode)
    }

    pub fn unwind_into_with_mode_at(
        &mut self,
        regs: &mut DwarfRegs,
        stack_base: u64,
        stack: &[u8],
        output: &mut Vec<UserFrame>,
        mode: UnwindMode,
    ) -> Result<(), CapturedUnwindError> {
        if self.mappings.is_empty() {
            output.clear();
            return Err(CapturedUnwindError::NoMappings);
        }
        if stack.is_empty() {
            output.clear();
            return Err(CapturedUnwindError::EmptyStack);
        }
        if !regs.contains(A::STACK_POINTER_REG) {
            output.clear();
            return Err(CapturedUnwindError::MissingStackPointer);
        }
        if !regs.contains(A::INSTRUCTION_POINTER_REG) {
            output.clear();
            return Err(CapturedUnwindError::MissingInstructionPointer);
        }

        let reload = self.reload_if_dirty();
        if reload.mapped_regions == 0 {
            output.clear();
            return Err(CapturedUnwindError::NoMappedRegions);
        }

        self.address_space
            .unwind_with_mode_and_stack_base(regs, stack_base, &stack, output, mode);
        if output.len() <= 1 {
            return Err(CapturedUnwindError::OnlyLeafFrame {
                reason: self.address_space.last_unwind_failure(),
            });
        }
        Ok(())
    }
}

impl CapturedStackUnwinder<arch::native::Arch> {
    pub fn unwind_callers(
        &mut self,
        state: CapturedThreadState,
        stack: CapturedStack<'_>,
        scratch: &mut Vec<UserFrame>,
        options: CapturedUnwindOptions,
    ) -> Result<CapturedUnwindOutcome, CapturedUnwindFailure> {
        match self.unwind_callers_once(state, stack, scratch, options.mode, options.max_frames) {
            Ok(callers) => Ok(CapturedUnwindOutcome {
                callers,
                bridge_attempted: false,
                bridge_steps: 0,
            }),
            Err(error) if !options.bridge.should_bridge(&error) => Err(CapturedUnwindFailure {
                error,
                bridge_attempted: false,
                bridge_steps: 0,
            }),
            Err(error) => {
                let mut last_error = error;
                let mut bridge_steps = 0usize;
                let mut bridge_prefix = Vec::with_capacity(options.max_frames);
                let mut fp = strip_data_pointer(state.fp);
                let mut sp = strip_data_pointer(state.sp);

                while bridge_steps < options.max_frames {
                    let Some(next_state) = fp_bridge_step(stack, fp, sp) else {
                        return Err(CapturedUnwindFailure {
                            error: last_error,
                            bridge_attempted: true,
                            bridge_steps,
                        });
                    };

                    bridge_steps += 1;
                    bridge_prefix.push(next_state.pc);

                    match self.unwind_callers_once(
                        next_state,
                        stack,
                        scratch,
                        options.mode,
                        options.max_frames,
                    ) {
                        Ok(mut callers) => {
                            bridge_prefix.append(&mut callers);
                            return Ok(CapturedUnwindOutcome {
                                callers: bridge_prefix,
                                bridge_attempted: true,
                                bridge_steps,
                            });
                        }
                        Err(error) if options.bridge.should_bridge(&error) => {
                            last_error = error;
                            fp = next_state.fp;
                            sp = next_state.sp;
                        }
                        Err(error) => {
                            return Err(CapturedUnwindFailure {
                                error,
                                bridge_attempted: true,
                                bridge_steps,
                            });
                        }
                    }
                }

                Err(CapturedUnwindFailure {
                    error: last_error,
                    bridge_attempted: true,
                    bridge_steps,
                })
            }
        }
    }

    fn unwind_callers_once(
        &mut self,
        state: CapturedThreadState,
        stack: CapturedStack<'_>,
        scratch: &mut Vec<UserFrame>,
        mode: UnwindMode,
        max_frames: usize,
    ) -> Result<Vec<u64>, CapturedUnwindError> {
        let mut regs = dwarf_regs_from_state(state);
        self.unwind_into_with_mode_at(&mut regs, stack.base, stack.bytes, scratch, mode)?;
        let mut callers = Vec::with_capacity(scratch.len().saturating_sub(1).min(max_frames));
        for frame in scratch.iter().skip(1).take(max_frames) {
            let pc = strip_code_pointer(frame.address);
            if pc != 0 {
                callers.push(pc);
            }
        }
        if callers.is_empty() {
            return Err(CapturedUnwindError::OnlyLeafFrame { reason: None });
        }
        Ok(callers)
    }
}

impl<A> Default for CapturedStackUnwinder<A>
where
    A: Architecture,
    A::RegTy: Primitive,
{
    fn default() -> Self {
        Self::new()
    }
}

pub fn captured_frame_pointer_walk(
    state: CapturedThreadState,
    stack: CapturedStack<'_>,
    max_frames: usize,
) -> Vec<u64> {
    let mut walked = Vec::with_capacity(max_frames);
    let mut fp = strip_data_pointer(state.fp);
    for _ in 0..max_frames {
        let Some(next_fp) = stack.read_u64(fp).map(strip_data_pointer) else {
            break;
        };
        let Some(saved_lr) = stack.read_u64(fp.saturating_add(8)).map(strip_code_pointer) else {
            break;
        };
        if saved_lr != 0 {
            walked.push(saved_lr);
        }
        if next_fp <= fp {
            break;
        }
        fp = next_fp;
    }
    walked
}

fn fp_bridge_step(stack: CapturedStack<'_>, fp: u64, sp: u64) -> Option<CapturedThreadState> {
    let fp = strip_data_pointer(fp);
    if fp == 0 || fp < strip_data_pointer(sp) {
        return None;
    }

    let next_fp = strip_data_pointer(stack.read_u64(fp)?);
    let pc = strip_code_pointer(stack.read_u64(fp.checked_add(8)?)?);
    if next_fp == 0 || next_fp <= fp || pc == 0 {
        return None;
    }

    let caller_sp = fp.checked_add(16)?;
    let lr = stack
        .read_u64(next_fp.checked_add(8)?)
        .map(strip_code_pointer)
        .unwrap_or(0);

    Some(CapturedThreadState {
        pc,
        lr,
        fp: next_fp,
        sp: caller_sp,
    })
}

fn dwarf_regs_from_state(state: CapturedThreadState) -> DwarfRegs {
    let mut regs = DwarfRegs::new();
    append_native_dwarf_regs(&mut regs, state);
    regs
}

#[cfg(target_arch = "aarch64")]
fn append_native_dwarf_regs(regs: &mut DwarfRegs, state: CapturedThreadState) {
    use nwind::arch::aarch64::dwarf;

    regs.append(dwarf::PC, strip_code_pointer(state.pc));
    regs.append(dwarf::X30, strip_code_pointer(state.lr));
    regs.append(dwarf::X29, strip_data_pointer(state.fp));
    regs.append(dwarf::X31, strip_data_pointer(state.sp));
}

#[cfg(target_arch = "x86_64")]
fn append_native_dwarf_regs(regs: &mut DwarfRegs, state: CapturedThreadState) {
    use nwind::arch::amd64::dwarf;

    regs.append(dwarf::RETURN_ADDRESS, strip_code_pointer(state.pc));
    regs.append(dwarf::RBP, strip_data_pointer(state.fp));
    regs.append(dwarf::RSP, strip_data_pointer(state.sp));
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
pub fn strip_code_pointer(mut ptr: u64) -> u64 {
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
pub fn strip_code_pointer(ptr: u64) -> u64 {
    ptr
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
pub fn strip_data_pointer(mut ptr: u64) -> u64 {
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
pub fn strip_data_pointer(ptr: u64) -> u64 {
    ptr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_fp_walk_reads_saved_lr_chain() {
        let mut stack = vec![0u8; 64];
        write_u64(&mut stack, 0, 0x1010);
        write_u64(&mut stack, 8, 0xaaaa);
        write_u64(&mut stack, 16, 0);
        write_u64(&mut stack, 24, 0xbbbb);

        let state = CapturedThreadState::new(0, 0, 0x1000, 0x1000);
        let stack = CapturedStack::new(0x1000, &stack);

        assert_eq!(
            captured_frame_pointer_walk(state, stack, 64),
            vec![0xaaaa, 0xbbbb]
        );
    }

    #[test]
    fn captured_fp_walk_preserves_recursive_return_addresses() {
        let mut stack = vec![0u8; 64];
        write_u64(&mut stack, 0, 0x1010);
        write_u64(&mut stack, 8, 0xaaaa);
        write_u64(&mut stack, 16, 0x1020);
        write_u64(&mut stack, 24, 0xaaaa);
        write_u64(&mut stack, 32, 0);
        write_u64(&mut stack, 40, 0xbbbb);

        let state = CapturedThreadState::new(0, 0, 0x1000, 0x1000);
        let stack = CapturedStack::new(0x1000, &stack);

        assert_eq!(
            captured_frame_pointer_walk(state, stack, 64),
            vec![0xaaaa, 0xaaaa, 0xbbbb]
        );
    }

    fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}
