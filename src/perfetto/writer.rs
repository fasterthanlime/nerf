//! Perfetto trace writer for the small schema subset we use. See `mod.rs`.

use std::collections::HashMap;
use std::io::{self, Write};

use super::proto::*;

/// Field numbers from `protos/perfetto/trace/trace_packet.proto`.
mod tp {
    pub const TIMESTAMP: u32 = 8;
    pub const TRUSTED_PACKET_SEQUENCE_ID: u32 = 10;
    pub const INTERNED_DATA: u32 = 12;
    pub const SEQUENCE_FLAGS: u32 = 13;
    pub const PROCESS_DESCRIPTOR: u32 = 43;
    pub const THREAD_DESCRIPTOR: u32 = 44;
    pub const FIRST_PACKET_ON_SEQUENCE: u32 = 87;
    /// Field 54 in the current Perfetto trace_packet.proto. (Earlier I
    /// guessed 91 -- that turned out to be an Android-only field, which
    /// is why ui.perfetto.dev reported `energy_descriptor_invalid` /
    /// `entity_state_residency_lookup_failed` errors per sample.)
    pub const STREAMING_PROFILE_PACKET: u32 = 54;
}

/// `Trace.packet` field number.
const FIELD_TRACE_PACKET: u32 = 1;

/// `sequence_flags` bits (see `trace_packet.proto`).
const SEQ_FLAG_INCREMENTAL_STATE_CLEARED: u64 = 1;

/// One sample as we want to emit it.
pub struct Sample<'a> {
    pub timestamp_ns: u64,
    /// Frame strings, root-first. The encoder reverses to leaf-first if
    /// Perfetto wants leaf-first ordering for its flame view (it does).
    pub frames_root_first: &'a [String],
}

/// Per-thread state we accumulate, then drain into the output.
pub struct ThreadTrace {
    pub pid: u32,
    pub tid: u32,
    pub thread_name: String,
    /// Per-thread sample list; we own the strings to keep things simple.
    pub samples: Vec<OwnedSample>,
}

pub struct OwnedSample {
    pub timestamp_ns: u64,
    pub frames_root_first: Vec<String>,
}

pub fn write_trace<W: Write>(
    w: &mut W,
    process_pid: u32,
    process_name: &str,
    threads: &[ThreadTrace],
) -> io::Result<()> {
    // Sequence id 1 is reserved for the process descriptor; threads start
    // at 2 and go up.
    write_process_descriptor_packet(w, 1, process_pid, process_name)?;

    for (idx, t) in threads.iter().enumerate() {
        let sequence_id = 2 + idx as u32;
        write_thread_sequence(w, sequence_id, t)?;
    }
    Ok(())
}

fn write_process_descriptor_packet<W: Write>(
    w: &mut W,
    sequence_id: u32,
    pid: u32,
    process_name: &str,
) -> io::Result<()> {
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
        write_message(buf, tp::PROCESS_DESCRIPTOR, |pd| {
            // `int32 pid = 1`
            write_uint32(pd, 1, pid)?;
            // `string process_name = 6`
            write_string(pd, 6, process_name)?;
            Ok(())
        })?;
        Ok(())
    })
}

fn write_thread_sequence<W: Write>(
    w: &mut W,
    sequence_id: u32,
    thread: &ThreadTrace,
) -> io::Result<()> {
    // First, intern strings, frames, and callstacks for this thread.
    let mut function_names: HashMap<String, u64> = HashMap::new();
    let mut frames: Vec<u64> = Vec::new(); // frame iid -> function_name iid
    let mut frame_idx_by_name: HashMap<String, u64> = HashMap::new(); // we collapse 1 frame == 1 unique function name
    let mut callstack_iids: HashMap<Vec<u64>, u64> = HashMap::new(); // frame_id list -> callstack iid
    let mut callstack_order: Vec<Vec<u64>> = Vec::new();

    let mut next_string_iid: u64 = 1;
    let mut next_frame_iid: u64 = 1;
    let mut next_callstack_iid: u64 = 1;

    let mut sample_callstack_iids: Vec<u64> = Vec::with_capacity(thread.samples.len());

    for sample in &thread.samples {
        // Perfetto's flame graph view wants leaf-first order in the
        // callstack frame list.
        let leaf_first: Vec<&str> = sample
            .frames_root_first
            .iter()
            .rev()
            .map(String::as_str)
            .collect();

        let mut frame_iids: Vec<u64> = Vec::with_capacity(leaf_first.len());
        for name in &leaf_first {
            // intern the function name string
            let _string_iid = *function_names.entry((*name).to_owned()).or_insert_with(|| {
                let iid = next_string_iid;
                next_string_iid += 1;
                iid
            });
            // intern the frame (1:1 with the function name in this minimal
            // encoding; we don't try to disambiguate inlined vs. non-inlined
            // call sites yet)
            let frame_iid = *frame_idx_by_name.entry((*name).to_owned()).or_insert_with(|| {
                let iid = next_frame_iid;
                next_frame_iid += 1;
                frames.push(_string_iid);
                iid
            });
            frame_iids.push(frame_iid);
        }
        let cs_iid = *callstack_iids.entry(frame_iids.clone()).or_insert_with(|| {
            let iid = next_callstack_iid;
            next_callstack_iid += 1;
            callstack_order.push(frame_iids.clone());
            iid
        });
        sample_callstack_iids.push(cs_iid);
    }

    // Build the InternedData payload first; we'll emit it in the same
    // TracePacket as the ThreadDescriptor and the first sample.
    let interned_payload = build_interned_data(
        &function_names,
        &frames,
        &callstack_order,
    )?;

    // Emit the bootstrap TracePacket: thread_descriptor + interned_data,
    // marked `first_packet_on_sequence` and with `INCREMENTAL_STATE_CLEARED`.
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint64(buf, tp::TIMESTAMP, thread.samples.first().map(|s| s.timestamp_ns).unwrap_or(0))?;
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
        write_uint64(buf, tp::SEQUENCE_FLAGS, SEQ_FLAG_INCREMENTAL_STATE_CLEARED)?;
        write_uint64(buf, tp::FIRST_PACKET_ON_SEQUENCE, 1)?;
        write_message(buf, tp::THREAD_DESCRIPTOR, |td| {
            // `int32 pid = 1`
            write_uint32(td, 1, thread.pid)?;
            // `int32 tid = 2`
            write_uint32(td, 2, thread.tid)?;
            // `string thread_name = 5`
            write_string(td, 5, &thread.thread_name)?;
            Ok(())
        })?;
        // interned_data goes in the same packet
        write_bytes(buf, tp::INTERNED_DATA, &interned_payload)?;
        Ok(())
    })?;

    // Emit one TracePacket per sample with a single-element
    // StreamingProfilePacket. Perfetto can also pack many samples into one
    // packet via the repeated fields, but per-sample TracePackets keep
    // absolute timestamps trivial and the trace is still small.
    for (sample, &cs_iid) in thread.samples.iter().zip(sample_callstack_iids.iter()) {
        write_message(w, FIELD_TRACE_PACKET, |buf| {
            write_uint64(buf, tp::TIMESTAMP, sample.timestamp_ns)?;
            write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
            write_message(buf, tp::STREAMING_PROFILE_PACKET, |sp| {
                // `repeated uint64 callstack_iid = 1`
                write_packed_uint64(sp, 1, &[cs_iid])?;
                // `repeated int64 timestamp_delta_us = 2` (microseconds!)
                // 0 delta because each sample has its own absolute timestamp
                // on the TracePacket.
                write_packed_uint64(sp, 2, &[0])?;
                Ok(())
            })?;
            Ok(())
        })?;
    }

    Ok(())
}

/// `InternedData` field numbers from `protos/perfetto/trace/interned_data/interned_data.proto`.
mod id {
    pub const FUNCTION_NAMES: u32 = 5;
    pub const FRAMES: u32 = 6;
    pub const CALLSTACKS: u32 = 7;
}

/// `Frame` (`protos/perfetto/trace/profiling/profile_common.proto`).
mod frame {
    pub const IID: u32 = 1;
    pub const FUNCTION_NAME_ID: u32 = 2;
}

/// `Callstack`.
mod cs {
    pub const IID: u32 = 1;
    pub const FRAME_IDS: u32 = 2;
}

/// `InternedString`.
mod istr {
    pub const IID: u32 = 1;
    pub const STR: u32 = 2;
}

fn build_interned_data(
    function_names: &HashMap<String, u64>,
    frames: &[u64],
    callstacks: &[Vec<u64>],
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();

    // `repeated InternedString function_names = 5`
    // Re-emit in iid order so the wire output is deterministic + small.
    let mut by_iid: Vec<(u64, &str)> = function_names
        .iter()
        .map(|(name, &iid)| (iid, name.as_str()))
        .collect();
    by_iid.sort_by_key(|&(iid, _)| iid);
    for (iid, name) in &by_iid {
        write_message(&mut out, id::FUNCTION_NAMES, |s| {
            write_uint64(s, istr::IID, *iid)?;
            write_string(s, istr::STR, name)?;
            Ok(())
        })?;
    }

    // `repeated Frame frames = 6`
    for (idx, &fn_iid) in frames.iter().enumerate() {
        let frame_iid = (idx + 1) as u64;
        write_message(&mut out, id::FRAMES, |f| {
            write_uint64(f, frame::IID, frame_iid)?;
            write_uint64(f, frame::FUNCTION_NAME_ID, fn_iid)?;
            Ok(())
        })?;
    }

    // `repeated Callstack callstacks = 7`
    for (idx, frame_ids) in callstacks.iter().enumerate() {
        let cs_iid = (idx + 1) as u64;
        write_message(&mut out, id::CALLSTACKS, |c| {
            write_uint64(c, cs::IID, cs_iid)?;
            write_packed_uint64(c, cs::FRAME_IDS, frame_ids)?;
            Ok(())
        })?;
    }

    Ok(out)
}
