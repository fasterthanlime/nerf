//! Trait + event types that downstream crates implement and consume.
//!
//! This crate used to contain a samply-derived suspend-and-walk
//! sampling recorder; that recorder has been removed, and only the
//! `SampleSink` trait (plus `MachOSymbol`, `ThreadNameCache`) remains.
//! The daemon backend (`nperfd-client`) is the sole sampling path.

#![cfg(target_os = "macos")]

pub mod proc_maps;
pub mod recorder;
pub mod sample_sink;

pub use sample_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, MachOByteSource, SampleEvent, SampleSink,
    ThreadNameEvent, WakeupEvent,
};
