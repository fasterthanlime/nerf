#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate log;

#[macro_use]
extern crate serde_derive;

mod utils;

pub mod args;
mod raw_data;
mod perf_group;
mod perf_arch;
mod archive;
mod execution_queue;
mod kallsyms;
mod ps;
mod stack_reader;
mod metadata;
mod mount_info;
mod profiler;
mod interner;
mod data_reader;

pub use crate::archive::{ArchiveReader, Packet};
pub use crate::stack_reader::StackReader;
pub mod cmd_record;
#[cfg(target_os = "macos")]
pub mod cmd_record_mac;
#[cfg(target_os = "macos")]
pub mod cmd_setup_mac;
#[cfg(feature = "inferno")]
pub mod cmd_flamegraph;
pub mod cmd_csv;
pub mod cmd_collate;
pub mod cmd_annotate;
pub mod cmd_metadata;
pub mod cmd_trace_events;
mod jitdump;
