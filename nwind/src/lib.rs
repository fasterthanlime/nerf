#[cfg(feature = "log")]
extern crate log;

#[cfg(feature = "addr2line")]
extern crate addr2line;

pub extern crate proc_maps;

#[allow(unused_macros)]
#[cfg(any(not(feature = "log"), not(feature = "debug-logs")))]
macro_rules! trace { ($($token:expr),*) => {{ if false { $( let _ = &$token; )+ } }} }

#[cfg(any(not(feature = "log"), not(feature = "debug-logs")))]
macro_rules! debug { ($($token:expr),*) => {{ if false { $( let _ = &$token; )+ } }} }

#[allow(unused_macros)]
#[cfg(not(feature = "log"))]
macro_rules! info { ($($token:expr),*) => {{ if false { $( let _ = &$token; )+ } }} }

#[cfg(not(feature = "log"))]
macro_rules! warn { ($($token:expr),*) => {{ if false { $( let _ = &$token; )+ } }} }

#[cfg(not(feature = "log"))]
macro_rules! error { ($($token:expr),*) => {{ if false { $( let _ = &$token; )+ } }} }

#[cfg(any(not(feature = "log"), not(feature = "debug-logs")))]
macro_rules! debug_logs_enabled {
    () => {
        false
    };
}

#[allow(unused_macros)]
#[cfg(all(feature = "log", feature = "debug-logs"))]
macro_rules! trace { ($($token:expr),*) => { log::trace!( $($token),* ) } }

#[cfg(all(feature = "log", feature = "debug-logs"))]
macro_rules! debug { ($($token:expr),*) => { log::debug!( $($token),* ) } }

#[allow(unused_macros)]
#[cfg(feature = "log")]
macro_rules! info { ($($token:expr),*) => { log::info!( $($token),* ) } }

#[cfg(feature = "log")]
macro_rules! warn { ($($token:expr),*) => { log::warn!( $($token),* ) } }

#[cfg(feature = "log")]
macro_rules! error { ($($token:expr),*) => { log::error!( $($token),* ) } }

#[cfg(all(feature = "log", feature = "debug-logs"))]
macro_rules! debug_logs_enabled {
    () => {
        log::log_enabled!(log::Level::Debug)
    };
}

#[cfg(feature = "local-unwinding")]
#[macro_use]
extern crate thread_local_reentrant;

#[macro_use]
mod elf;

mod macho;

mod address_space;
pub mod arch;
mod arm_extab;
mod binary;
mod debug_info_index;
mod dwarf;
mod dwarf_regs;
mod frame_descriptions;
#[cfg(feature = "local-unwinding")]
mod local_unwinding;
mod range_map;
mod symbols;
mod types;
mod unwind_context;
pub mod utils;

pub use crate::address_space::{
    AddressSpace, BufferReader, Frame, IAddressSpace, Primitive, ResolvedSymbol,
};
pub use crate::binary::{BinaryData, BinaryDataReader, LoadHeader, SymbolTable};
pub use crate::dwarf_regs::DwarfRegs;
pub use crate::range_map::RangeMap;
pub use crate::symbols::Symbols;
pub use crate::types::{BinaryId, Bitness, Inode, UserFrame};

pub use crate::debug_info_index::DebugInfoIndex;
pub use crate::frame_descriptions::LoadHint;

#[cfg(feature = "local-unwinding")]
pub use crate::local_unwinding::{
    LocalAddressSpace, LocalAddressSpaceOptions, LocalUnwindContext, UnwindControl,
};
