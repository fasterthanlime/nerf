use gimli::{self, CfaRule, EvaluationResult, Format, Location, Piece, RegisterRule, Value};

use crate::address_space::{lookup_binary, MemoryReader};
use crate::arch::{Architecture, Registers, TryInto, UnwindFailure};
use crate::frame_descriptions::{ContextCache, UnwindInfo, UnwindInfoCache};
use crate::types::Bitness;

pub struct DwarfResult {
    pub initial_address: u64,
    pub cfa: u64,
    pub ra_address: Option<u64>,
    pub return_address_error: Option<UnwindFailure>,
}

fn dwarf_get_reg<A, M, R>(
    nth_frame: usize,
    register: u16,
    memory: &M,
    regs: &A::Regs,
    cfa_value: u64,
    rule: &RegisterRule<R>,
) -> Result<(u64, u64), UnwindFailure>
where
    A: Architecture,
    M: MemoryReader<A>,
    R: gimli::Reader,
    <R as gimli::Reader>::Offset: Default,
{
    let (value_address, value) = match *rule {
        RegisterRule::Offset(offset) => {
            let value_address = (cfa_value as i64 + offset) as u64;
            debug!(
                "Register {:?} at frame #{} is at 0x{:016X}",
                A::register_name(register),
                nth_frame,
                value_address
            );

            let value = match memory.get_pointer_at_address(value_address.try_into().unwrap()) {
                Some(value) => value,
                None => {
                    debug!( "Cannot grab register {:?} for frame #{}: failed to fetch it from 0x{:016X}", A::register_name( register ), nth_frame, value_address );
                    return Err(UnwindFailure::RegisterMemoryReadFailed);
                }
            };
            (value_address, value)
        }
        RegisterRule::Expression(ref expression) => {
            let value_address = match evaluate_dwarf_expression(memory, regs, expression.clone()) {
                Ok(value) => value,
                Err(_) => {
                    debug!( "Cannot grab register {:?} for frame #{}: failed to evaluate DWARF bytecode", A::register_name( register ), nth_frame );
                    return Err(UnwindFailure::RegisterExpressionFailed);
                }
            };

            let value = match memory.get_pointer_at_address(value_address.try_into().unwrap()) {
                Some(value) => value,
                None => {
                    debug!( "Cannot grab register {:?} for frame #{}: failed to fetch it from 0x{:016X}", A::register_name( register ), nth_frame, value_address );
                    return Err(UnwindFailure::RegisterMemoryReadFailed);
                }
            };

            (value_address, value)
        }
        ref rule => {
            error!(
                "Handling for this register rule is unimplemented: {:?}",
                rule
            );
            return Err(UnwindFailure::UnsupportedRegisterRule);
        }
    };

    debug!(
        "Register {:?} at frame #{} is equal to 0x{:016X}",
        A::register_name(register),
        nth_frame,
        value
    );
    Ok((value_address, value.into()))
}

fn evaluate_dwarf_expression<A, M, R>(
    memory: &M,
    regs: &A::Regs,
    expr: gimli::read::Expression<R>,
) -> Result<u64, UnwindFailure>
where
    A: Architecture,
    M: MemoryReader<A>,
    R: gimli::Reader,
    <R as gimli::Reader>::Offset: Default,
{
    let address_size = match A::BITNESS {
        Bitness::B32 => 4,
        Bitness::B64 => 8,
    };
    let encoding = gimli::Encoding {
        // TODO: use CIE format?
        format: Format::Dwarf32,
        // This doesn't currently matter for expressions.
        version: 0,
        address_size,
    };

    if debug_logs_enabled!() {
        debug!("Evaluating DWARF expression:");
        let mut iter = expr.clone().operations(encoding);
        while let Ok(Some(op)) = iter.next() {
            debug!("  {:?}", op);
        }
    }

    let mut evaluation = expr.evaluation(encoding);
    let mut result = evaluation.evaluate();
    let value;
    loop {
        match result {
            Ok(EvaluationResult::Complete) => {
                let mut pieces = evaluation.result();
                if pieces.len() == 1 {
                    match pieces.pop().unwrap() {
                        Piece {
                            size_in_bits: None,
                            bit_offset: None,
                            location: Location::Address { address },
                            ..
                        } => {
                            value = address;
                            break;
                        }
                        piece => {
                            error!("Unhandled DWARF evaluation result: {:?}", piece);
                            return Err(UnwindFailure::CfaExpressionFailed);
                        }
                    }
                } else {
                    error!("Unhandled DWARF evaluation result: {:?}", pieces);
                    return Err(UnwindFailure::CfaExpressionFailed);
                }
            }
            Ok(EvaluationResult::RequiresRegister {
                register,
                base_type,
            }) => {
                if base_type != gimli::UnitOffset(Default::default()) {
                    error!( "Failed to evaluate DWARF expression: unsupported base type in RequiresRegister rule: {:?}", base_type );
                    return Err(UnwindFailure::CfaExpressionFailed);
                }

                let reg_value = match regs.get(register.0) {
                    Some(reg_value) => reg_value.into(),
                    None => {
                        error!( "Failed to evaluate DWARF expression due to a missing value of register {:?}", A::register_name( register.0 ) );
                        return Err(UnwindFailure::CfaExpressionFailed);
                    }
                };

                debug!(
                    "Fetched register {:?}: 0x{:016X}",
                    A::register_name(register.0),
                    reg_value
                );
                result = evaluation.resume_with_register(Value::Generic(reg_value));
            }
            Ok(EvaluationResult::RequiresMemory {
                address,
                size,
                space: None,
                base_type,
            }) if size as usize == std::mem::size_of::<A::RegTy>() => {
                if base_type != gimli::UnitOffset(Default::default()) {
                    error!( "Failed to evaluate DWARF expression: unsupported base type in RequiresMemory rule: {:?}", base_type );
                    return Err(UnwindFailure::CfaExpressionFailed);
                }

                let address = match crate::arch::TryFrom::try_from(address) {
                    Some(address) => address,
                    None => {
                        error!( "Failed to evaluate DWARF expression: out of range address in a RequiresMemory rule: 0x{:016X}", address );
                        return Err(UnwindFailure::CfaExpressionFailed);
                    }
                };
                let raw_value = match memory.get_pointer_at_address(address) {
                    Some(raw_value) => raw_value.into(),
                    None => {
                        error!( "Failed to evaluate DWARF expression: couldn't fetch {} bytes from 0x{:016X}", size, address );
                        return Err(UnwindFailure::CfaExpressionFailed);
                    }
                };

                debug!("Fetched memory from 0x{:016X}: 0x{:X}", address, raw_value);

                let value;
                if std::mem::size_of::<A::RegTy>() == 4 {
                    value = gimli::Value::U32(raw_value as u32);
                } else {
                    value = gimli::Value::U64(raw_value);
                }

                result = evaluation.resume_with_memory(value);
            }
            Ok(result) => {
                error!(
                    "Failed to evaluate DWARF expression due to unhandled requirement: {:?}",
                    result
                );
                return Err(UnwindFailure::CfaExpressionFailed);
            }
            Err(error) => {
                error!("Failed to evaluate DWARF expression: {:?}", error);
                return Err(UnwindFailure::CfaExpressionFailed);
            }
        }
    }

    Ok(value)
}

fn dwarf_unwind_impl<A: Architecture, M: MemoryReader<A>>(
    nth_frame: usize,
    memory: &M,
    regs: &A::Regs,
    unwind_info: &UnwindInfo<A::Endianity>,
    next_regs: &mut Vec<(u16, u64)>,
    ra_address: &mut Option<u64>,
) -> Result<(u64, bool, Option<UnwindFailure>), UnwindFailure> {
    debug!(
        "Initial address for frame #{}: 0x{:016X}",
        nth_frame,
        unwind_info.initial_absolute_address()
    );

    let cfa = unwind_info.cfa();
    debug!("Grabbing CFA for frame #{}: {:?}", nth_frame, cfa);

    let cfa_value = match cfa {
        CfaRule::RegisterAndOffset {
            register: cfa_register,
            offset: cfa_offset,
        } => {
            let cfa_register_value = match regs.get(cfa_register.0) {
                Some(cfa_register_value) => cfa_register_value.into(),
                None => {
                    debug!(
                        "Failed to fetch CFA for frame #{}: failed to fetch register {:?}",
                        nth_frame,
                        A::register_name(cfa_register.0)
                    );
                    return Err(UnwindFailure::MissingCfaRegister);
                }
            };

            let value: u64 = (cfa_register_value as i64 + cfa_offset) as u64;
            debug!(
                "Got CFA for frame #{}: {:?} (0x{:016X}) + {} = 0x{:016X}",
                nth_frame,
                A::register_name(cfa_register.0),
                cfa_register_value,
                cfa_offset,
                value
            );
            value
        }
        CfaRule::Expression(expr) => {
            let value = evaluate_dwarf_expression(memory, regs, expr)
                .map_err(|_| UnwindFailure::CfaExpressionFailed)?;
            debug!("Evaluated CFA for frame #{}: 0x{:016X}", nth_frame, value);
            value
        }
    };

    let mut cacheable = true;
    let mut return_address_error = None;
    unwind_info.each_register(|(register, rule)| {
        debug!("  Register {:?}: {:?}", A::register_name(register.0), rule);

        match dwarf_get_reg(nth_frame + 1, register.0, memory, regs, cfa_value, rule) {
            Ok((value_address, value)) => {
                if register.0 == A::RETURN_ADDRESS_REG {
                    *ra_address = Some(value_address);
                }

                next_regs.push((register.0, value));
            }
            Err(error) => {
                if register.0 == A::RETURN_ADDRESS_REG {
                    return_address_error = Some(error);
                }
                cacheable = false;
            }
        }
    });

    Ok((cfa_value, cacheable, return_address_error))
}

pub(crate) fn dwarf_unwind_with_info<A: Architecture, M: MemoryReader<A>>(
    nth_frame: usize,
    memory: &M,
    regs: &A::Regs,
    unwind_info: &UnwindInfo<A::Endianity>,
    next_regs: &mut Vec<(u16, u64)>,
) -> Result<DwarfResult, UnwindFailure> {
    next_regs.clear();

    if unwind_info.is_signal_frame() {
        debug!("Frame #{} is a signal frame!", nth_frame);
    }

    let mut ra_address = None;
    let result = dwarf_unwind_impl(
        nth_frame,
        memory,
        regs,
        unwind_info,
        next_regs,
        &mut ra_address,
    );

    let initial_address = unwind_info.initial_absolute_address();
    let (cfa, return_address_error) = match result {
        Ok((cfa, _, return_address_error)) => (cfa, return_address_error),
        Err(error) => return Err(error),
    };

    Ok(DwarfResult {
        initial_address,
        cfa,
        ra_address,
        return_address_error,
    })
}

pub fn dwarf_unwind<A: Architecture, M: MemoryReader<A>>(
    nth_frame: usize,
    memory: &M,
    ctx_cache: &mut ContextCache<A::Endianity>,
    unwind_cache: &mut UnwindInfoCache,
    regs: &A::Regs,
    next_regs: &mut Vec<(u16, u64)>,
) -> Result<DwarfResult, UnwindFailure> {
    next_regs.clear();

    let address: u64 = regs
        .get(A::INSTRUCTION_POINTER_REG)
        .ok_or(UnwindFailure::MissingInstructionPointer)?
        .into();
    if address == 0 {
        debug!("Instruction pointer is NULL; cannot continue unwinding");
        return Err(UnwindFailure::NullInstructionPointer);
    }

    let address = if nth_frame == 0 { address } else { address - 1 };
    let cached_unwind_info = unwind_cache.lookup(address);
    let mut uncached_unwind_info = None;

    if cached_unwind_info.is_none() {
        if let Some(binary) = lookup_binary(nth_frame, memory, regs) {
            uncached_unwind_info = binary.lookup_unwind_row(ctx_cache, address);
        } else if let Some(registry) = memory.dynamic_fde_registry() {
            uncached_unwind_info = registry.lookup_unwind_row(ctx_cache, address);
        } else {
            return Err(UnwindFailure::NoBinary);
        }
    }

    let unwind_info = match cached_unwind_info
        .as_ref()
        .or(uncached_unwind_info.as_ref())
    {
        Some(unwind_info) => unwind_info,
        None => {
            debug!("No unwind info for address 0x{:016X}", address);
            return Err(UnwindFailure::NoUnwindInfo);
        }
    };

    let mut ra_address = None;
    let result = dwarf_unwind_impl(
        nth_frame,
        memory,
        regs,
        unwind_info,
        next_regs,
        &mut ra_address,
    );

    let initial_address = unwind_info.initial_absolute_address();
    let (cfa, return_address_error) = match result {
        Ok((cfa, cacheable, return_address_error)) => {
            if cacheable {
                if let Some(uncached_unwind_info) = uncached_unwind_info {
                    uncached_unwind_info.cache_into(unwind_cache);
                }
            }
            (cfa, return_address_error)
        }
        Err(error) => return Err(error),
    };

    Ok(DwarfResult {
        initial_address,
        cfa,
        ra_address,
        return_address_error,
    })
}
