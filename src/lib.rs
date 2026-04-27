#[macro_use]
extern crate log;

mod utils;

pub mod args;
pub mod ingest_sink;
pub mod live_sink;

#[cfg(target_os = "macos")]
pub mod cmd_record_mac;
#[cfg(target_os = "macos")]
pub mod cmd_setup_mac;
