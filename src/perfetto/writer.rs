//! Perfetto trace writer for the small schema subset we use. Modeled on
//! the canonical `traced_perf` output in
//! `test/data/callstack_sampling.pftrace` (Perfetto's own test trace).
//!
//! Schema choices (validated against a known-good trace):
//!
//! * Single packet sequence (`trusted_packet_sequence_id = 1`). PerfSample
//!   carries `pid`/`tid` directly so we don't need per-thread sequences
//!   the way `StreamingProfilePacket` does.
//! * Bootstrap packet at start: ClockSnapshot + ProcessDescriptor +
//!   InternedData, flagged `SEQ_INCREMENTAL_STATE_CLEARED` and
//!   `first_packet_on_sequence = true`.
//! * Per-sample TracePackets: `SEQ_NEEDS_INCREMENTAL_STATE` flag, BOOTTIME
//!   timestamp (default), one `PerfSample { pid, tid, callstack_iid, … }`.
//! * No `Mapping`s yet; `Frame` carries only `function_name_id`. Perfetto's
//!   UI tolerates this but loses some detail in its drilldowns. We can
//!   add Mappings later when we wire in binary-load events from nerf-mac-capture.

use std::collections::HashMap;
use std::io::{self, Write};

use super::proto::*;

/// Field numbers from `protos/perfetto/trace/trace_packet.proto`. Verified
/// against the live proto in google/perfetto.
mod tp {
    pub const TIMESTAMP: u32 = 8;
    pub const TRUSTED_PACKET_SEQUENCE_ID: u32 = 10;
    pub const INTERNED_DATA: u32 = 12;
    pub const SEQUENCE_FLAGS: u32 = 13;
    pub const CLOCK_SNAPSHOT: u32 = 6;
    pub const PROCESS_DESCRIPTOR: u32 = 43;
    pub const FIRST_PACKET_ON_SEQUENCE: u32 = 87;
    /// `optional PerfSample perf_sample = 66;`
    pub const PERF_SAMPLE: u32 = 66;
}

/// `Trace.packet` field number.
const FIELD_TRACE_PACKET: u32 = 1;

/// Builtin clock ids (see `protos/perfetto/common/builtin_clock.proto`).
const BUILTIN_CLOCK_MONOTONIC: u64 = 3;
const BUILTIN_CLOCK_BOOTTIME: u64 = 6;

/// `sequence_flags` bits.
const SEQ_FLAG_INCREMENTAL_STATE_CLEARED: u64 = 1;
const SEQ_FLAG_NEEDS_INCREMENTAL_STATE: u64 = 2;

/// `Profiling.CpuMode` enum value `MODE_USER = 2`.
const CPU_MODE_USER: u64 = 2;

/// Single packet sequence covers everything; PerfSample carries tid/pid
/// inline so we don't need separate sequences per thread.
const SEQUENCE_ID: u32 = 1;

pub struct ThreadTrace {
    pub pid: u32,
    pub tid: u32,
    pub thread_name: String,
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
    // Build a single global interned table covering all samples across all
    // threads. Strings, frames, and callstacks all live in one numbering
    // space; PerfSample.callstack_iid references it.
    let mut interner = Interner::new();
    let mut sample_streams: Vec<(u32 /*tid*/, u32 /*pid*/, &OwnedSample, u64 /*cs iid*/)> =
        Vec::new();

    for t in threads {
        for s in &t.samples {
            let cs_iid = interner.intern_callstack(&s.frames_root_first);
            sample_streams.push((t.tid, t.pid, s, cs_iid));
        }
    }
    sample_streams.sort_by_key(|&(_, _, s, _)| s.timestamp_ns);

    // Anchor for the ClockSnapshot.
    let anchor_ns = sample_streams
        .first()
        .map(|&(_, _, s, _)| s.timestamp_ns)
        .unwrap_or(0);

    // Bootstrap packet: ClockSnapshot + ProcessDescriptor + InternedData,
    // cleared + first_on_sequence.
    write_bootstrap_packet(w, process_pid, process_name, anchor_ns, &interner)?;

    // Per-sample packets.
    for (tid, pid, sample, cs_iid) in &sample_streams {
        write_message(w, FIELD_TRACE_PACKET, |buf| {
            write_uint64(buf, tp::TIMESTAMP, sample.timestamp_ns)?;
            write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
            write_uint64(buf, tp::SEQUENCE_FLAGS, SEQ_FLAG_NEEDS_INCREMENTAL_STATE)?;
            write_message(buf, tp::PERF_SAMPLE, |ps| {
                // PerfSample fields: cpu=1, pid=2, tid=3, callstack_iid=4,
                // cpu_mode=5, timebase_count=6.
                write_uint32(ps, 1, 0)?; // cpu (we don't track it; 0 placeholder)
                write_uint32(ps, 2, *pid)?;
                write_uint32(ps, 3, *tid)?;
                write_uint64(ps, 4, *cs_iid)?;
                write_uint64(ps, 5, CPU_MODE_USER)?;
                Ok(())
            })?;
            Ok(())
        })?;
    }

    let _ = threads; // thread names are currently surfaced only via the
                     // sample-time tid; we could emit ThreadDescriptors
                     // for richer UI labels, but PerfSample's tid is enough
                     // for the timeline + flame views.

    Ok(())
}

fn write_bootstrap_packet<W: Write>(
    w: &mut W,
    process_pid: u32,
    process_name: &str,
    anchor_ns: u64,
    interner: &Interner,
) -> io::Result<()> {
    // First, the ClockSnapshot. Some Perfetto-internal parsers demand a
    // mapping for MONOTONIC; emitting one keeps `clock_sync_failure_*`
    // import warnings out of the way.
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
        write_message(buf, tp::CLOCK_SNAPSHOT, |snap| {
            write_message(snap, 1 /* clocks */, |c| {
                write_uint64(c, 1 /* clock_id */, BUILTIN_CLOCK_BOOTTIME)?;
                write_uint64(c, 2 /* timestamp */, anchor_ns)?;
                Ok(())
            })?;
            write_message(snap, 1 /* clocks */, |c| {
                write_uint64(c, 1, BUILTIN_CLOCK_MONOTONIC)?;
                write_uint64(c, 2, anchor_ns)?;
                Ok(())
            })?;
            write_uint64(snap, 2 /* primary_trace_clock */, BUILTIN_CLOCK_BOOTTIME)?;
            Ok(())
        })?;
        Ok(())
    })?;

    // ProcessDescriptor. Gives the timeline a friendly process name.
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
        write_message(buf, tp::PROCESS_DESCRIPTOR, |pd| {
            write_uint32(pd, 1 /* pid */, process_pid)?;
            write_string(pd, 6 /* process_name */, process_name)?;
            Ok(())
        })?;
        Ok(())
    })?;

    // InternedData payload. Bundle everything (function_names + frames +
    // callstacks) in one packet at trace-start; subsequent sample packets
    // reference these iids and inherit them via NEEDS_INCREMENTAL_STATE.
    let interned_payload = interner.encode_interned_data()?;
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint64(buf, tp::TIMESTAMP, anchor_ns)?;
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
        write_uint64(
            buf,
            tp::SEQUENCE_FLAGS,
            SEQ_FLAG_INCREMENTAL_STATE_CLEARED | SEQ_FLAG_NEEDS_INCREMENTAL_STATE,
        )?;
        write_uint64(buf, tp::FIRST_PACKET_ON_SEQUENCE, 1)?;
        write_bytes(buf, tp::INTERNED_DATA, &interned_payload)?;
        Ok(())
    })?;

    Ok(())
}

/// Per-trace interning state. Function-name strings, frames (one per
/// unique function name in this minimal encoding), and callstacks all
/// live in a single numbering space.
struct Interner {
    function_names: HashMap<String, u64>,
    next_string_iid: u64,
    /// frame iid -> function_name iid
    frame_to_function: Vec<u64>,
    /// function_name -> frame iid (1 frame per unique function name; we
    /// don't disambiguate inlined call sites for now)
    frame_iid_by_name: HashMap<String, u64>,
    next_frame_iid: u64,
    /// frame_id sequence -> callstack iid
    callstacks: HashMap<Vec<u64>, u64>,
    /// callstack iid -> frame_id sequence (in iid order, indexed iid-1)
    callstack_order: Vec<Vec<u64>>,
    next_callstack_iid: u64,
}

impl Interner {
    fn new() -> Self {
        Self {
            function_names: HashMap::new(),
            next_string_iid: 1,
            frame_to_function: Vec::new(),
            frame_iid_by_name: HashMap::new(),
            next_frame_iid: 1,
            callstacks: HashMap::new(),
            callstack_order: Vec::new(),
            next_callstack_iid: 1,
        }
    }

    /// Returns the callstack iid for the given root-first frame names.
    /// Perfetto's flame view wants leaf-first ordering in the frame_ids
    /// list, so we reverse internally.
    fn intern_callstack(&mut self, frames_root_first: &[String]) -> u64 {
        let mut frame_ids: Vec<u64> = Vec::with_capacity(frames_root_first.len());
        for name in frames_root_first.iter().rev() {
            let string_iid = match self.function_names.get(name) {
                Some(&iid) => iid,
                None => {
                    let iid = self.next_string_iid;
                    self.next_string_iid += 1;
                    self.function_names.insert(name.clone(), iid);
                    iid
                }
            };
            let frame_iid = match self.frame_iid_by_name.get(name) {
                Some(&iid) => iid,
                None => {
                    let iid = self.next_frame_iid;
                    self.next_frame_iid += 1;
                    self.frame_iid_by_name.insert(name.clone(), iid);
                    self.frame_to_function.push(string_iid);
                    iid
                }
            };
            frame_ids.push(frame_iid);
        }
        match self.callstacks.get(&frame_ids) {
            Some(&iid) => iid,
            None => {
                let iid = self.next_callstack_iid;
                self.next_callstack_iid += 1;
                self.callstack_order.push(frame_ids.clone());
                self.callstacks.insert(frame_ids, iid);
                iid
            }
        }
    }

    fn encode_interned_data(&self) -> io::Result<Vec<u8>> {
        // InternedData field numbers from interned_data.proto:
        //   function_names = 5
        //   frames         = 6
        //   callstacks     = 7
        // Frame fields:        iid=1, function_name_id=2 (mapping_id=3, rel_pc=4 omitted)
        // Callstack fields:    iid=1, frame_ids=2 (packed)
        // InternedString:      iid=1, str=2
        let mut out = Vec::new();

        // function_names (sorted by iid for deterministic output)
        let mut by_iid: Vec<(u64, &str)> = self
            .function_names
            .iter()
            .map(|(s, &iid)| (iid, s.as_str()))
            .collect();
        by_iid.sort_by_key(|&(iid, _)| iid);
        for (iid, name) in by_iid {
            write_message(&mut out, 5 /* function_names */, |s| {
                write_uint64(s, 1 /* iid */, iid)?;
                write_string(s, 2 /* str */, name)?;
                Ok(())
            })?;
        }

        // frames
        for (idx, &fn_iid) in self.frame_to_function.iter().enumerate() {
            let frame_iid = (idx as u64) + 1;
            write_message(&mut out, 6 /* frames */, |f| {
                write_uint64(f, 1 /* iid */, frame_iid)?;
                write_uint64(f, 2 /* function_name_id */, fn_iid)?;
                Ok(())
            })?;
        }

        // callstacks
        for (idx, frame_ids) in self.callstack_order.iter().enumerate() {
            let cs_iid = (idx as u64) + 1;
            write_message(&mut out, 7 /* callstacks */, |c| {
                write_uint64(c, 1 /* iid */, cs_iid)?;
                write_packed_uint64(c, 2 /* frame_ids */, frame_ids)?;
                Ok(())
            })?;
        }

        Ok(out)
    }
}
