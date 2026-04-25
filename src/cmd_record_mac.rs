//! macOS implementation of `nperf record`. Drives nerf-mac-capture against
//! an existing PID and writes packets directly into the nperf archive.
//!
//! Scope: existing-PID only (`--pid`). Child-launch (`--process`) requires
//! the DYLD_INSERT_LIBRARIES preload-dylib bundling pipeline, which is a
//! follow-up. See notes/mac-roadmap.md.

use std::borrow::Cow;
use std::error::Error;
use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::time::Duration;

use speedy::{Endianness, Writable};

use nerf_mac_capture::{
    record as mac_record, BinaryLoadedEvent, BinaryUnloadedEvent, RecordOptions, SampleEvent,
    SampleSink, ThreadNameEvent,
};

use crate::archive::{
    BinaryFormat, Bitness, FramedPacket, Inode, Packet, Platform, UserFrame, ARCHIVE_MAGIC,
    ARCHIVE_VERSION,
};
use crate::args::{self, TargetProcess};
use crate::utils::SigintHandler;

const DEFAULT_OUTPUT: &str = "perf.data";

pub fn main(args: args::RecordArgs) -> Result<(), Box<dyn Error>> {
    let pid = match TargetProcess::from(args.profiler_args.process_filter.clone()) {
        TargetProcess::ByPid(pid) => pid,
        TargetProcess::ByName(_) | TargetProcess::ByNameWaiting(_, _) => {
            return Err(
                "macOS record currently supports only --pid; --process child-launch is not yet wired up"
                    .into(),
            );
        }
    };

    if args.discard_all {
        return Err("--discard-all is not supported on macOS yet".into());
    }
    if args.profiler_args.offline {
        return Err(
            "--offline is not supported on macOS yet (raw-stack capture is M3 of the roadmap)"
                .into(),
        );
    }

    let output_path: PathBuf = args
        .profiler_args
        .output
        .clone()
        .unwrap_or_else(|| OsString::from(DEFAULT_OUTPUT))
        .into();

    let exe_path = match proc_pidpath(pid) {
        Ok(p) => p,
        Err(err) => {
            warn!("proc_pidpath({}) failed: {}", pid, err);
            String::new()
        }
    };

    info!("Recording PID {} -> {}", pid, output_path.display());

    let writer = BufWriter::new(File::create(&output_path)?);
    let mut sink = MacSink::new(writer, pid)?;

    sink.write_packet(Packet::Header {
        magic: ARCHIVE_MAGIC,
        version: ARCHIVE_VERSION,
    })?;
    sink.write_packet(Packet::MachineInfo {
        cpu_count: num_cpus::get() as u32,
        endianness: Endianness::NATIVE,
        bitness: Bitness::NATIVE,
        architecture: native_arch_name().into(),
        platform: Platform::MacOS,
    })?;
    sink.write_packet(Packet::ProcessInfo {
        pid,
        executable: Cow::Owned(exe_path.into_bytes()),
        binary_id: Inode::empty(),
    })?;
    sink.write_packet(Packet::ProfilingFrequency {
        frequency: args.frequency,
    })?;

    let sigint = SigintHandler::new();
    let start = std::time::Instant::now();
    let time_limit = args.profiler_args.time_limit.map(Duration::from_secs);
    let should_stop = || {
        if sigint.was_triggered() {
            return true;
        }
        if let Some(limit) = time_limit {
            if start.elapsed() >= limit {
                return true;
            }
        }
        false
    };

    let opts = RecordOptions {
        pid,
        frequency_hz: args.frequency,
        // We rely on `should_stop()` for the time limit so the elapsed time
        // is reported uniformly with the rest of the codebase. RecordOptions::duration
        // is a hard backstop only.
        duration: None,
        fold_recursive_prefix: false,
    };

    info!("Running... press Ctrl-C to stop.");
    if let Err(err) = mac_record(opts, &mut sink, should_stop) {
        return Err(format!("nerf-mac-capture::record failed: {}", err).into());
    }

    sink.finish()?;
    info!("Recording complete.");
    Ok(())
}

fn native_arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        "unknown"
    }
}

/// Wraps an output BufWriter and emits archive packets in response to
/// SampleSink events.
struct MacSink {
    writer: BufWriter<File>,
}

impl MacSink {
    fn new(writer: BufWriter<File>, _pid: u32) -> io::Result<Self> {
        Ok(Self { writer })
    }

    fn write_packet(&mut self, packet: Packet<'_>) -> io::Result<()> {
        FramedPacket::Known(packet)
            .write_to_stream(&mut self.writer)
            .map_err(io::Error::from)
    }

    fn finish(mut self) -> io::Result<()> {
        use std::io::Write;
        self.writer.flush()
    }
}

impl SampleSink for MacSink {
    fn on_sample(&mut self, ev: SampleEvent<'_>) {
        let user_backtrace: Vec<UserFrame> = ev
            .backtrace
            .iter()
            .map(|&address| UserFrame {
                address,
                initial_address: None,
            })
            .collect();
        let packet = Packet::Sample {
            timestamp: ev.timestamp_ns,
            pid: ev.pid,
            tid: ev.tid,
            cpu: u32::MAX, // unknown / not tracked on mac yet
            kernel_backtrace: Cow::Owned(Vec::new()),
            user_backtrace: Cow::Owned(user_backtrace),
        };
        if let Err(err) = self.write_packet(packet) {
            warn!("on_sample write failed: {}", err);
        }
    }

    fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>) {
        // Emit BinaryInfo (Mach-O variant) so the analysis side knows about
        // the image. We use a synthetic Inode keyed off the base address.
        let inode = avma_pseudo_inode(ev.base_avma);
        if let Err(err) = self.write_packet(Packet::BinaryInfo {
            inode,
            symbol_table_count: 0,
            path: Cow::Owned(ev.path.as_bytes().to_owned()),
            load_headers: Cow::Owned(Vec::new()),
            format: BinaryFormat::MachO,
        }) {
            warn!("on_binary_loaded BinaryInfo write failed: {}", err);
            return;
        }

        if let Some(uuid) = ev.uuid {
            let _ = self.write_packet(Packet::BuildId {
                inode,
                build_id: uuid.to_vec(),
                path: Cow::Owned(ev.path.as_bytes().to_owned()),
            });
        }

        let _ = self.write_packet(Packet::BinaryLoaded {
            pid: ev.pid,
            inode: Some(inode),
            name: Cow::Owned(ev.path.as_bytes().to_owned()),
        });
    }

    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>) {
        let inode = avma_pseudo_inode(ev.base_avma);
        let _ = self.write_packet(Packet::BinaryUnloaded {
            pid: ev.pid,
            inode: Some(inode),
            name: Cow::Owned(ev.path.as_bytes().to_owned()),
        });
    }

    fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>) {
        let _ = self.write_packet(Packet::ThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: Cow::Owned(ev.name.as_bytes().to_owned()),
        });
    }
}

/// nperf's archive uses an `Inode` to identify a binary across packets.
/// macOS doesn't have a perf-style inode-on-disk identity per loaded image
/// (the same dyld_shared_cache slice is shared by many processes), so we
/// fabricate one by hashing the base AVMA into the `inode` field.
fn avma_pseudo_inode(base_avma: u64) -> Inode {
    Inode {
        inode: base_avma,
        dev_major: 0,
        dev_minor: 0,
    }
}

/// Look up the executable path for `pid` via `proc_pidpath(3)`.
fn proc_pidpath(pid: u32) -> io::Result<String> {
    extern "C" {
        fn proc_pidpath(pid: libc::c_int, buf: *mut libc::c_void, buflen: u32) -> libc::c_int;
    }
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * libc::PATH_MAX as usize;
    let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    let ret = unsafe {
        proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if ret <= 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(ret as usize);
    let s = unsafe { CStr::from_ptr(buf.as_ptr() as *const libc::c_char) }
        .to_string_lossy()
        .into_owned();
    Ok(s)
}
