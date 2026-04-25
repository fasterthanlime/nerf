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

    let anchor_ns = sample_streams
        .first()
        .map(|&(_, _, s, _)| s.timestamp_ns)
        .unwrap_or(0);

    // ClockSnapshot first (own packet, no incremental-state interaction).
    write_clock_snapshot(w, anchor_ns)?;

    // ProcessDescriptor (own packet, gives the timeline a labelled track).
    write_process_descriptor(w, process_pid, process_name)?;

    // Standalone CLEARED packet on the sample sequence. This matches the
    // working `callstack_sampling.pftrace` shape: it puts CLEARED on a
    // packet with no InternedData, then InternedData arrives on the
    // first NEEDS_INCREMENTAL_STATE packet. Putting CLEARED + InternedData
    // on the same packet doesn't reliably populate interned_data_ for
    // subsequent lookups in trace_processor v54.
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint64(buf, tp::TIMESTAMP, anchor_ns)?;
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
        write_uint64(buf, tp::SEQUENCE_FLAGS, SEQ_FLAG_INCREMENTAL_STATE_CLEARED)?;
        write_uint64(buf, tp::FIRST_PACKET_ON_SEQUENCE, 1)?;
        Ok(())
    })?;

    // Build the InternedData payload once; we attach it to the first sample's
    // TracePacket (with NEEDS_INCREMENTAL_STATE only) -- by that point CLEARED
    // has already established the incremental-state generation.
    let interned_payload = interner.encode_interned_data()?;

    for (idx, (tid, pid, sample, cs_iid)) in sample_streams.iter().enumerate() {
        let is_first = idx == 0;
        write_message(w, FIELD_TRACE_PACKET, |buf| {
            write_uint64(buf, tp::TIMESTAMP, sample.timestamp_ns)?;
            write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
            write_uint64(buf, tp::SEQUENCE_FLAGS, SEQ_FLAG_NEEDS_INCREMENTAL_STATE)?;
            // Attach the global InternedData to the first sample's
            // packet only -- subsequent samples inherit it via
            // NEEDS_INCREMENTAL_STATE on the same sequence.
            if is_first {
                write_bytes(buf, tp::INTERNED_DATA, &interned_payload)?;
            }
            write_message(buf, tp::PERF_SAMPLE, |ps| {
                write_uint32(ps, 1, 0)?;
                write_uint32(ps, 2, *pid)?;
                write_uint32(ps, 3, *tid)?;
                write_uint64(ps, 4, *cs_iid)?;
                write_uint64(ps, 5, CPU_MODE_USER)?;
                Ok(())
            })?;
            Ok(())
        })?;
    }

    let _ = threads;
    Ok(())
}

fn write_clock_snapshot<W: Write>(w: &mut W, anchor_ns: u64) -> io::Result<()> {
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
        write_message(buf, tp::CLOCK_SNAPSHOT, |snap| {
            write_message(snap, 1 /* clocks */, |c| {
                write_uint64(c, 1, BUILTIN_CLOCK_BOOTTIME)?;
                write_uint64(c, 2, anchor_ns)?;
                Ok(())
            })?;
            write_message(snap, 1 /* clocks */, |c| {
                write_uint64(c, 1, BUILTIN_CLOCK_MONOTONIC)?;
                write_uint64(c, 2, anchor_ns)?;
                Ok(())
            })?;
            write_uint64(snap, 2, BUILTIN_CLOCK_BOOTTIME)?; // primary_trace_clock
            Ok(())
        })?;
        Ok(())
    })
}

fn write_process_descriptor<W: Write>(
    w: &mut W,
    pid: u32,
    name: &str,
) -> io::Result<()> {
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, SEQUENCE_ID)?;
        write_message(buf, tp::PROCESS_DESCRIPTOR, |pd| {
            write_uint32(pd, 1 /* pid */, pid)?;
            write_string(pd, 6 /* process_name */, name)?;
            Ok(())
        })?;
        Ok(())
    })
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
        //   build_ids       = 16   (InternedString)
        //   mapping_paths   = 17   (InternedString)
        //   function_names  = 5    (InternedString)
        //   mappings        = 19
        //   frames          = 6
        //   callstacks      = 7
        //
        // Frame fields:     iid=1, function_name_id=2, mapping_id=3, rel_pc=4
        // Mapping fields:   iid=1, build_id=2, exact_offset=8, start_offset=3,
        //                   start=4, end=5, load_bias=6, path_string_ids=7 (repeated)
        // Callstack fields: iid=1, frame_ids=2 (packed)
        // InternedString:   iid=1, str=2
        //
        // We emit a single dummy Mapping at iid 1 covering the entire 64-bit
        // address space and have every Frame reference it with rel_pc=0. The
        // proto comment says mapping_id=0 means "fully symbolized, no mapping",
        // but Perfetto's UI parser rejects such frames as invalid in practice.
        // We don't have per-binary load addresses plumbed through here yet;
        // when binary-load events are wired in we can emit one Mapping per
        // image and recover real address attribution.
        let mut out = Vec::new();

        const DUMMY_BUILD_ID_IID: u64 = 1;
        const DUMMY_PATH_IID: u64 = 1;
        const DUMMY_MAPPING_IID: u64 = 1;

        // build_ids[1] = "" (we don't have a real build id)
        write_message(&mut out, 16 /* build_ids */, |s| {
            write_uint64(s, 1, DUMMY_BUILD_ID_IID)?;
            write_string(s, 2, "")?;
            Ok(())
        })?;

        // mapping_paths[1] = "nperf"
        write_message(&mut out, 17 /* mapping_paths */, |s| {
            write_uint64(s, 1, DUMMY_PATH_IID)?;
            write_string(s, 2, "nperf")?;
            Ok(())
        })?;

        // mappings[1] = covers 0..u64::MAX
        write_message(&mut out, 19 /* mappings */, |m| {
            write_uint64(m, 1 /* iid */, DUMMY_MAPPING_IID)?;
            write_uint64(m, 2 /* build_id */, DUMMY_BUILD_ID_IID)?;
            write_uint64(m, 3 /* start_offset */, 0)?;
            write_uint64(m, 4 /* start */, 0)?;
            write_uint64(m, 5 /* end */, u64::MAX)?;
            write_uint64(m, 6 /* load_bias */, 0)?;
            // path_string_ids is repeated; emit one entry pointing at the dummy path.
            write_uint64(m, 7 /* path_string_ids */, DUMMY_PATH_IID)?;
            write_uint64(m, 8 /* exact_offset */, 0)?;
            Ok(())
        })?;

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

        // frames -- every frame references the single dummy mapping with rel_pc=0
        for (idx, &fn_iid) in self.frame_to_function.iter().enumerate() {
            let frame_iid = (idx as u64) + 1;
            write_message(&mut out, 6 /* frames */, |f| {
                write_uint64(f, 1 /* iid */, frame_iid)?;
                write_uint64(f, 2 /* function_name_id */, fn_iid)?;
                write_uint64(f, 3 /* mapping_id */, DUMMY_MAPPING_IID)?;
                write_uint64(f, 4 /* rel_pc */, 0)?;
                Ok(())
            })?;
        }

        // callstacks. `frame_ids` is `repeated uint64` in a proto2 schema,
        // which defaults to NON-PACKED -- emit each id as its own field-2
        // entry. Perfetto's parser doesn't accept packed for this field;
        // sending packed silently broke iid lookup with no schema-level
        // error reported.
        for (idx, frame_ids) in self.callstack_order.iter().enumerate() {
            let cs_iid = (idx as u64) + 1;
            write_message(&mut out, 7 /* callstacks */, |c| {
                write_uint64(c, 1 /* iid */, cs_iid)?;
                for &fid in frame_ids {
                    write_uint64(c, 2 /* frame_ids */, fid)?;
                }
                Ok(())
            })?;
        }

        Ok(out)
    }
}
