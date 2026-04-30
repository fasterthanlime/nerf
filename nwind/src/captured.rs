use std::sync::Arc;

use proc_maps::Region;

use crate::address_space::{AddressSpace, IAddressSpace, Primitive};
use crate::arch::{self, Architecture, Registers, UnwindFailure, UnwindMode};
use crate::binary::BinaryData;
use crate::dwarf_regs::DwarfRegs;
use crate::frame_descriptions::LoadHint;
use crate::types::UserFrame;

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

impl<A> Default for CapturedStackUnwinder<A>
where
    A: Architecture,
    A::RegTy: Primitive,
{
    fn default() -> Self {
        Self::new()
    }
}
