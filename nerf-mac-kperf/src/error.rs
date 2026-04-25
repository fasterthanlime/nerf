//! Errors raised by `nerf-mac-kperf`.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to dlopen private framework {path}: {msg}")]
    FrameworkLoad { path: String, msg: String },

    #[error("missing symbol {name}: {msg}")]
    SymbolMissing { name: String, msg: String },

    #[error("kperf needs root (run with sudo)")]
    NotRoot,

    #[error("{op} failed: {source}")]
    Sysctl {
        op: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("kperfdata: {op} returned {code}")]
    Kpep { op: &'static str, code: i32 },

    #[error("kperf: {op} returned {code}")]
    Kperf { op: &'static str, code: i32 },

    #[error("event {name:?} not found in PMU database")]
    UnknownEvent { name: String },

    #[error("too many events: {0} (cap is {1})")]
    TooManyEvents(usize, usize),
}
