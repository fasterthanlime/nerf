//! macOS implementation of `nperf record`. Drives nerf-mac-capture for
//! both `--pid <PID>` (attach to an existing process) and `--process
//! <NAME>` (spawn a fresh child via the preload-dylib bootstrap).

use std::borrow::Cow;
use std::error::Error;
use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::prelude::*;
use speedy::{Endianness, Writable};

use nerf_mac_capture::process_launcher::{
    drop_sudo_privileges, ReceivedStuff, TaskAccepter, TaskLauncher,
};
use nerf_mac_capture::{
    record_with_task, record_with_task_and_tick_hook, BinaryLoadedEvent, BinaryUnloadedEvent,
    JitdumpEvent, RecordOptions, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};
use nerf_mac_kperf::{record as kperf_record, RecordOptions as KperfRecordOptions};

use crate::archive::{
    BinaryFormat, Bitness, FramedPacket, Inode, MachOSymbolEntry, Packet, Platform, UserFrame,
    ARCHIVE_MAGIC, ARCHIVE_VERSION,
};
use crate::args::{self, TargetProcess};
use crate::live_sink::{
    BinaryLoadedEvent as LiveBinaryLoadedEvent, BinaryUnloadedEvent as LiveBinaryUnloadedEvent,
    LiveSink, LiveSymbol, SampleEvent as LiveSampleEvent, TargetAttached,
    ThreadName as LiveThreadName, WakeupEvent as LiveWakeupEvent,
};
use crate::utils::SigintHandler;

pub fn main(args: args::RecordArgs) -> Result<(), Box<dyn Error>> {
    main_with_live_sink(args, None)
}

pub fn main_with_live_sink(
    args: args::RecordArgs,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    if args.discard_all {
        return Err("--discard-all is not supported on macOS yet".into());
    }
    if args.profiler_args.offline {
        return Err(
            "--offline is not supported on macOS yet (raw-stack capture is M3b of the roadmap)"
                .into(),
        );
    }

    match TargetProcess::from(args.profiler_args.process_filter.clone()) {
        TargetProcess::ByPid(pid) => record_existing_pid(args, pid, live_sink),
        TargetProcess::ByName(name) => {
            let prog_args = args.program_args.clone();
            record_child_launch(args, name, prog_args, live_sink)
        }
        TargetProcess::ByNameWaiting(_, _) => {
            Err("--wait is not supported on macOS (the launched child is the one we wait for)".into())
        }
    }
}

fn record_existing_pid(
    args: args::RecordArgs,
    pid: u32,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    let exe_path = proc_pidpath(pid).unwrap_or_else(|err| {
        warn!("proc_pidpath({}) failed: {}", pid, err);
        String::new()
    });
    let output_path = resolve_output_path(&args, pid, &exe_path);

    info!("Recording PID {} -> {}", pid, output_path.display());

    let mut sink = open_sink(&output_path, pid, &exe_path, &args)?;
    sink.live_sink = live_sink;

    let sigint = SigintHandler::new();
    let start = std::time::Instant::now();
    let time_limit = args.profiler_args.time_limit.map(Duration::from_secs);
    let should_stop = || sigint_or_deadline(&sigint, &start, &time_limit);
    let opts = RecordOptions {
        pid,
        frequency_hz: args.frequency,
        duration: None,
        fold_recursive_prefix: false,
    };

    info!("Running... press Ctrl-C to stop.");
    match args.mac_backend.as_str() {
        "samply" => {
            // samply needs the task port up front: `record_with_task`
            // takes it and the live sink wants it for mach_vm_read of
            // JIT bytes. kperf reacquires internally; daemon doesn't
            // need it at all (yet — once the broker is wired, that's
            // where it'll come from).
            let task = task_for_pid_existing(pid)?;
            if let Some(live) = sink.live_sink.as_ref() {
                live.on_target_attached(&TargetAttached {
                    pid,
                    task_port: task as u64,
                });
            }
            if let Err(err) = record_with_task(task, opts, &mut sink, should_stop) {
                return Err(format!("nerf-mac-capture::record failed: {}", err).into());
            }
        }
        "kperf" => {
            // Time-based stop is enforced inside the kperf drain
            // loop (relative to kperf arming, after the initial
            // dyld scan), so should_stop here only watches for
            // SIGINT. Otherwise the slow first scan eats the
            // user-requested time budget before sampling begins.
            let kopts = KperfRecordOptions {
                pid,
                frequency_hz: args.frequency,
                duration: time_limit,
                ..Default::default()
            };
            let kperf_should_stop = || sigint.was_triggered();
            if let Err(err) = kperf_record(kopts, &mut sink, kperf_should_stop) {
                return Err(format!("nerf-mac-kperf::record failed: {}", err).into());
            }
        }
        "daemon" => {
            // Same time-budget shape as the kperf branch: the
            // daemon's drain loop continues until we close the
            // records channel, which the client driver does when
            // either the duration elapses or `should_stop` returns
            // true. SIGINT alone is enough to watch for here.
            let opts = nperfd_client::RemoteOptions {
                daemon_socket: args.daemon_socket.clone(),
                pid,
                frequency_hz: args.frequency,
                duration: time_limit,
                ..Default::default()
            };
            let daemon_should_stop = || sigint.was_triggered();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("daemon backend: tokio runtime build: {err}"))?;
            if let Err(err) = rt.block_on(nperfd_client::drive_session(
                opts,
                &mut sink,
                daemon_should_stop,
            )) {
                return Err(format!("nperfd-client failed: {err}").into());
            }
        }
        other => {
            return Err(format!("unknown --mac-backend value: {other}").into());
        }
    }

    sink.finish()?;
    Ok(())
}

fn record_child_launch(
    args: args::RecordArgs,
    program: String,
    program_args: Vec<String>,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    if args.mac_backend == "kperf" {
        return record_child_launch_kperf(args, program, program_args, live_sink);
    }
    if args.mac_backend == "daemon" {
        return record_child_launch_daemon(args, program, program_args, live_sink);
    }
    let mut accepter = TaskAccepter::new()
        .map_err(|err| format!("setting up Mach IPC accepter: {:?}", err))?;
    let server_name = accepter.server_name().to_owned();
    let launcher = TaskLauncher::new(
        OsString::from(&program),
        program_args.into_iter().map(OsString::from),
        &server_name,
    )
    .map_err(|err| format!("preparing TaskLauncher: {:?}", err))?;

    info!("Launching {}...", program);
    let child_guard = ChildGuard::new(launcher.launch_child());
    let child_for_stop = child_guard.share();

    info!("Waiting for child to bootstrap via preload dylib...");
    let accepted_task = wait_for_my_task(&mut accepter, Duration::from_secs(10))?;
    let pid = accepted_task.pid();
    info!("Child bootstrapped: PID {}", pid);

    let exe_path = proc_pidpath(pid).unwrap_or_else(|_| program.clone());
    let output_path = resolve_output_path(&args, pid, &exe_path);
    info!("Recording PID {} -> {}", pid, output_path.display());
    let mut sink = open_sink(&output_path, pid, &exe_path, &args)?;
    sink.live_sink = live_sink;

    // The child path already holds the task port from the bootstrap IPC;
    // hand it to the live sink before resuming the child.
    if let Some(live) = sink.live_sink.as_ref() {
        live.on_target_attached(&TargetAttached {
            pid,
            task_port: accepted_task.task() as u64,
        });
    }

    // Resume the child now that we have the task port and the headers
    // are written.
    accepted_task.start_execution();

    let sigint = SigintHandler::new();
    let start = std::time::Instant::now();
    let time_limit = args.profiler_args.time_limit.map(Duration::from_secs);
    let opts = RecordOptions {
        pid,
        frequency_hz: args.frequency,
        duration: None,
        fold_recursive_prefix: false,
    };

    let should_stop = || {
        if sigint_or_deadline(&sigint, &start, &time_limit) {
            return true;
        }
        // Also stop if the child has exited.
        match child_for_stop.lock() {
            Ok(mut c) => matches!(c.try_wait(), Ok(Some(_))),
            Err(_) => true,
        }
    };

    let drain_messages = |sink: &mut MacSink| {
        // Non-blocking-ish drain of any new IPC messages from the preload
        // dylib (jitdump-path notifications mostly).
        while let Ok(msg) = accepter.next_message(Duration::from_millis(0)) {
            match msg {
                ReceivedStuff::JitdumpPath(pid, path) => {
                    sink.on_jitdump(JitdumpEvent {
                        pid,
                        path: path.as_path(),
                    });
                }
                ReceivedStuff::AcceptedTask(_) => {
                    // We don't currently support multi-process recording; ignore
                    // additional task ports (descendants).
                }
                ReceivedStuff::Ignored => {}
            }
        }
    };

    info!("Running... press Ctrl-C to stop.");
    if let Err(err) = record_with_task_and_tick_hook(
        accepted_task.task(),
        opts,
        &mut sink,
        should_stop,
        drain_messages,
    ) {
        return Err(format!("nerf-mac-capture::record failed: {}", err).into());
    }

    // child_guard drops at end of scope, killing + reaping the child.

    sink.finish()?;
    Ok(())
}

/// Child-launch path for the kperf backend. Plain `Command::spawn` —
/// no preload dylib, no Mach IPC bootstrap. kperf samples in-kernel
/// by PID, so we don't need the task port before exec; we acquire
/// it inside `nerf-mac-kperf::record` via `task_for_pid` after fork.
///
/// Limitation: `task_for_pid` against a hardened-runtime / platform
/// binary returns `KERN_FAILURE` regardless of entitlements, so this
/// path won't work for those targets. Use the samply backend (which
/// has the preload-dylib `task_self`-trampoline trick) for those.
fn record_child_launch_kperf(
    args: args::RecordArgs,
    program: String,
    program_args: Vec<String>,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    use std::process::Command;

    info!("Launching {}...", program);
    let mut cmd = Command::new(&program);
    cmd.args(&program_args);
    drop_sudo_privileges(&mut cmd);
    let child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn {program}: {err}"))?;
    let pid = child.id();
    let child_guard = ChildGuard::new(child);
    let child_for_stop = child_guard.share();
    info!("Child started: PID {}", pid);

    let exe_path = proc_pidpath(pid).unwrap_or_else(|_| program.clone());
    let output_path = resolve_output_path(&args, pid, &exe_path);
    info!("Recording PID {} -> {}", pid, output_path.display());
    let mut sink = open_sink(&output_path, pid, &exe_path, &args)?;
    sink.live_sink = live_sink;

    let sigint = SigintHandler::new();
    let time_limit = args.profiler_args.time_limit.map(Duration::from_secs);

    let kopts = KperfRecordOptions {
        pid,
        frequency_hz: args.frequency,
        duration: time_limit,
        ..Default::default()
    };

    let should_stop = move || {
        if sigint.was_triggered() {
            return true;
        }
        match child_for_stop.lock() {
            Ok(mut c) => matches!(c.try_wait(), Ok(Some(_))),
            Err(_) => true,
        }
    };

    info!("Running... press Ctrl-C to stop.");
    if let Err(err) = kperf_record(kopts, &mut sink, should_stop) {
        return Err(format!("nerf-mac-kperf::record failed: {}", err).into());
    }

    // child_guard drops at end of scope, killing + reaping the child.
    sink.finish()?;
    Ok(())
}

/// Child-launch path for the daemon backend. Same shape as
/// `record_child_launch_kperf` — plain `Command::spawn`, no preload
/// dylib, no Mach IPC bootstrap — but the privileged kperf calls
/// happen in `nperfd` (which we connect to over a vox local socket)
/// instead of in-process. The CLI itself runs unprivileged.
fn record_child_launch_daemon(
    args: args::RecordArgs,
    program: String,
    program_args: Vec<String>,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    use std::process::Command;

    info!("Launching {}...", program);
    let mut cmd = Command::new(&program);
    cmd.args(&program_args);
    let child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn {program}: {err}"))?;
    let pid = child.id();
    let child_guard = ChildGuard::new(child);
    let child_for_stop = child_guard.share();
    info!("Child started: PID {}", pid);

    let exe_path = proc_pidpath(pid).unwrap_or_else(|_| program.clone());
    let output_path = resolve_output_path(&args, pid, &exe_path);
    info!("Recording PID {} -> {}", pid, output_path.display());
    let mut sink = open_sink(&output_path, pid, &exe_path, &args)?;
    sink.live_sink = live_sink;

    let sigint = SigintHandler::new();
    let time_limit = args.profiler_args.time_limit.map(Duration::from_secs);

    let opts = nperfd_client::RemoteOptions {
        daemon_socket: args.daemon_socket.clone(),
        pid,
        frequency_hz: args.frequency,
        duration: time_limit,
        ..Default::default()
    };

    let should_stop = move || {
        if sigint.was_triggered() {
            return true;
        }
        match child_for_stop.lock() {
            Ok(mut c) => matches!(c.try_wait(), Ok(Some(_))),
            Err(_) => true,
        }
    };

    info!("Running... press Ctrl-C to stop.");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("daemon backend: tokio runtime build: {err}"))?;
    if let Err(err) = rt.block_on(nperfd_client::drive_session(opts, &mut sink, should_stop)) {
        return Err(format!("nperfd-client failed: {err}").into());
    }

    // child_guard drops at end of scope, killing + reaping the child.
    sink.finish()?;
    Ok(())
}

fn wait_for_my_task(
    accepter: &mut TaskAccepter,
    timeout: Duration,
) -> Result<nerf_mac_capture::process_launcher::AcceptedTask, Box<dyn Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for child to bootstrap via preload dylib".into());
        }
        match accepter.next_message(remaining) {
            Ok(ReceivedStuff::AcceptedTask(task)) => return Ok(task),
            Ok(_) => continue, // jitdump-before-task or ignored kind; keep waiting
            Err(err) => return Err(format!("Mach IPC error: {:?}", err).into()),
        }
    }
}

fn open_sink(
    output_path: &std::path::Path,
    pid: u32,
    exe_path: &str,
    args: &args::RecordArgs,
) -> Result<MacSink, Box<dyn Error>> {
    // Bare File::create errors print as "Permission denied (os
    // error 13)" with no path, which is unactionable. The most common
    // case is leftover root-owned files from an earlier sudo'd run;
    // include both the path and the underlying io::Error so the user
    // can `ls -la` it without guessing.
    let writer = BufWriter::new(
        File::create(output_path).map_err(|err| {
            format!("opening output file {}: {err}", output_path.display())
        })?,
    );
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
        executable: Cow::Owned(exe_path.as_bytes().to_owned()),
        binary_id: Inode::empty(),
    })?;
    sink.write_packet(Packet::ProfilingFrequency {
        frequency: args.frequency,
    })?;

    Ok(sink)
}

/// Mirrors the Linux profiler.rs default-output convention: when `-o` isn't
/// given, fall back to `<YYYYMMDD>_<HHMMSS>_<pid>_<exe-basename>.nperf`.
fn resolve_output_path(args: &args::RecordArgs, pid: u32, exe_path: &str) -> PathBuf {
    if let Some(ref out) = args.profiler_args.output {
        return PathBuf::from(out);
    }

    let basename: String = {
        let raw = exe_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("nperf");
        raw.chars()
            .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
            .collect()
    };

    let now = Utc::now();
    PathBuf::from(format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}_{:05}_{}.nperf",
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        pid,
        basename
    ))
}

/// Acquire a Mach task port for an existing PID. Mirrors what
/// `nerf-mac-capture::record` does internally — but there we use it via
/// `record_with_task` so the same port can be handed to the live sink.
fn task_for_pid_existing(pid: u32) -> Result<mach2::port::mach_port_t, Box<dyn Error>> {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::port::{mach_port_t, MACH_PORT_NULL};
    use mach2::traps::{mach_task_self, task_for_pid};

    let mut task: mach_port_t = MACH_PORT_NULL;
    let kr = unsafe { task_for_pid(mach_task_self(), pid as i32, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(format!("task_for_pid({}) failed: kr={}", pid, kr).into());
    }
    Ok(task)
}

fn sigint_or_deadline(
    sigint: &SigintHandler,
    start: &std::time::Instant,
    time_limit: &Option<Duration>,
) -> bool {
    if sigint.was_triggered() {
        return true;
    }
    if let Some(limit) = time_limit {
        if start.elapsed() >= *limit {
            return true;
        }
    }
    false
}

fn native_arch_name() -> &'static str {
    // Match the architecture-name strings the nwind crate uses, since
    // data_reader dispatches on these to pick a per-arch AddressSpace.
    if cfg!(target_arch = "aarch64") {
        "aarch64"
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
    /// Tracks the address range each loaded image occupies so we can emit
    /// the exact `MemoryRegionUnmap` range when the image is unloaded.
    loaded_ranges: std::collections::HashMap<u64, u64>,
    /// Jitdump paths the preload dylib reported during recording.
    jitdump_paths: Vec<PathBuf>,
    /// Optional live sink fed from `on_sample` so `--serve` can stream
    /// aggregations alongside the on-disk archive.
    live_sink: Option<Box<dyn LiveSink>>,
}

impl MacSink {
    fn new(writer: BufWriter<File>, _pid: u32) -> io::Result<Self> {
        Ok(Self {
            writer,
            loaded_ranges: std::collections::HashMap::new(),
            jitdump_paths: Vec::new(),
            live_sink: None,
        })
    }

    fn write_packet(&mut self, packet: Packet<'_>) -> io::Result<()> {
        FramedPacket::Known(packet)
            .write_to_stream(&mut self.writer)
            .map_err(io::Error::from)
    }

    fn finish(mut self) -> io::Result<()> {
        use std::io::Write;

        // Embed each discovered jitdump file's bytes into the archive as a
        // FileBlob. data_reader's pre-scan picks these up and seeds the
        // jitdump_events queue so `nperf collate <archive>` resolves JIT
        // names without needing --jitdump.
        for path in self.jitdump_paths.clone() {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    info!(
                        "Embedding jitdump {} ({} bytes) into archive.",
                        path.display(),
                        bytes.len()
                    );
                    let path_bytes = path
                        .to_string_lossy()
                        .as_bytes()
                        .to_owned();
                    let _ = self.write_packet(Packet::FileBlob {
                        path: Cow::Owned(path_bytes),
                        data: Cow::Owned(bytes),
                    });
                }
                Err(err) => {
                    warn!(
                        "Could not read jitdump {} for embedding: {}",
                        path.display(),
                        err
                    );
                }
            }
        }

        self.writer.flush()?;
        info!("Recording complete.");
        Ok(())
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
        let kernel_backtrace: Vec<u64> = ev.kernel_backtrace.to_vec();
        if let Some(sink) = self.live_sink.as_ref() {
            sink.on_sample(&LiveSampleEvent {
                timestamp: ev.timestamp_ns,
                pid: ev.pid,
                tid: ev.tid,
                cpu: u32::MAX,
                kernel_backtrace: &kernel_backtrace,
                user_backtrace: &user_backtrace,
                cycles: ev.cycles,
                instructions: ev.instructions,
                l1d_misses: ev.l1d_misses,
                branch_mispreds: ev.branch_mispreds,
            });
        }
        let packet = Packet::Sample {
            timestamp: ev.timestamp_ns,
            pid: ev.pid,
            tid: ev.tid,
            cpu: u32::MAX, // unknown / not tracked on mac yet
            kernel_backtrace: Cow::Owned(kernel_backtrace),
            user_backtrace: Cow::Owned(user_backtrace),
        };
        if let Err(err) = self.write_packet(packet) {
            warn!("on_sample write failed: {}", err);
        }
    }

    fn on_cpu_interval(&mut self, ev: nerf_mac_capture::sample_sink::CpuIntervalEvent<'_>) {
        // Forward to the live sink only -- the offline archive
        // format doesn't carry CPU intervals (yet); the live
        // aggregator is the only consumer that needs them right now.
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        match ev.kind {
            nerf_mac_capture::sample_sink::CpuIntervalKind::OnCpu => {
                sink.on_cpu_interval(&crate::live_sink::CpuIntervalEvent {
                    pid: ev.pid,
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    kind: crate::live_sink::CpuIntervalKind::OnCpu,
                });
            }
            nerf_mac_capture::sample_sink::CpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                // Materialise the cached stack as `UserFrame`s for
                // the live-sink shape. The local Vec lives for the
                // duration of the sink call; the sink copies into
                // an owned `LiveEvent` so it can drop the borrow.
                let stack: Vec<UserFrame> = stack
                    .iter()
                    .map(|&address| UserFrame {
                        address,
                        initial_address: None,
                    })
                    .collect();
                sink.on_cpu_interval(&crate::live_sink::CpuIntervalEvent {
                    pid: ev.pid,
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    kind: crate::live_sink::CpuIntervalKind::OffCpu {
                        stack: &stack,
                        waker_tid,
                        waker_user_stack,
                    },
                });
            }
        }
    }

    fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>) {
        // Tee to the live sink (if any) so the live aggregator can resolve
        // addresses against this image even though we still write the same
        // archive packets below.
        if let Some(sink) = self.live_sink.as_ref() {
            let live_symbols: Vec<LiveSymbol<'_>> = ev
                .symbols
                .iter()
                .map(|s| LiveSymbol {
                    start_svma: s.start_svma,
                    end_svma: s.end_svma,
                    name: &s.name,
                })
                .collect();
            sink.on_binary_loaded(&LiveBinaryLoadedEvent {
                path: ev.path,
                base_avma: ev.base_avma,
                vmsize: ev.vmsize,
                text_svma: ev.text_svma,
                arch: ev.arch,
                is_executable: ev.is_executable,
                symbols: &live_symbols,
                text_bytes: ev.text_bytes,
            });
        }

        // We key Mach-O binaries by path (BinaryId::ByName) since macOS
        // doesn't surface a stable per-image inode the way Linux does.
        // Inode::empty() trips the `is_invalid()` check on the analysis
        // side so it falls back to the name-keyed path consistently.
        let inode = Inode::empty();
        let path_bytes: Vec<u8> = ev.path.as_bytes().to_owned();

        // Synthesize a single LoadHeader covering the __TEXT segment so
        // nwind's address_space.reload can compute the slide
        // (base_avma - text_svma) for symbol lookups.
        let load_headers = vec![nwind::LoadHeader {
            address: ev.text_svma,
            file_offset: 0,
            file_size: ev.vmsize,
            memory_size: ev.vmsize,
            // 16K pages on aarch64-apple-darwin, 4K on x86_64. Use 16K
            // unconditionally; nwind only uses this for alignment of file
            // offsets, not for any kernel-level mmap.
            alignment: 0x4000,
            is_readable: true,
            is_writable: false,
            is_executable: true,
        }];

        if let Err(err) = self.write_packet(Packet::BinaryInfo {
            inode,
            symbol_table_count: 0,
            path: Cow::Owned(path_bytes.clone()),
            load_headers: Cow::Owned(load_headers),
            format: BinaryFormat::MachO,
        }) {
            warn!("on_binary_loaded BinaryInfo write failed: {}", err);
            return;
        }

        if let Some(uuid) = ev.uuid {
            let _ = self.write_packet(Packet::BuildId {
                inode,
                build_id: uuid.to_vec(),
                path: Cow::Owned(path_bytes.clone()),
            });
        }

        if !ev.symbols.is_empty() {
            let entries: Vec<MachOSymbolEntry> = ev
                .symbols
                .iter()
                .map(|s| MachOSymbolEntry {
                    start_svma: s.start_svma,
                    end_svma: s.end_svma,
                    name: s.name.clone(),
                })
                .collect();
            let _ = self.write_packet(Packet::MachOSymbolTable {
                inode,
                path: Cow::Owned(path_bytes.clone()),
                text_svma: ev.text_svma,
                entries,
            });
        }

        // The runtime memory region. The analysis side keys binary lookups
        // off region.name (since major/minor are 0), so the name here must
        // match the BinaryInfo path verbatim. We set `inode: 1` so the
        // address-space reload code doesn't drop the region as an anonymous
        // mapping (its filter is `inode == 0 && name != "[vdso]"`).
        self.loaded_ranges.insert(ev.base_avma, ev.vmsize);
        let _ = self.write_packet(Packet::MemoryRegionMap {
            pid: ev.pid,
            range: ev.base_avma..ev.base_avma + ev.vmsize,
            is_read: true,
            is_write: false,
            is_executable: true,
            is_shared: false,
            file_offset: 0,
            inode: 1,
            major: 0,
            minor: 0,
            name: Cow::Owned(path_bytes.clone()),
        });

        let _ = self.write_packet(Packet::BinaryLoaded {
            pid: ev.pid,
            inode: Some(inode),
            name: Cow::Owned(path_bytes),
        });
    }

    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>) {
        if let Some(sink) = self.live_sink.as_ref() {
            sink.on_binary_unloaded(&LiveBinaryUnloadedEvent {
                path: ev.path,
                base_avma: ev.base_avma,
            });
        }
        if let Some(vmsize) = self.loaded_ranges.remove(&ev.base_avma) {
            let _ = self.write_packet(Packet::MemoryRegionUnmap {
                pid: ev.pid,
                range: ev.base_avma..ev.base_avma + vmsize,
            });
        }
        let _ = self.write_packet(Packet::BinaryUnloaded {
            pid: ev.pid,
            inode: Some(Inode::empty()),
            name: Cow::Owned(ev.path.as_bytes().to_owned()),
        });
    }

    fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>) {
        if let Some(sink) = self.live_sink.as_ref() {
            sink.on_thread_name(&LiveThreadName {
                pid: ev.pid,
                tid: ev.tid,
                name: ev.name,
            });
        }
        let _ = self.write_packet(Packet::ThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: Cow::Owned(ev.name.as_bytes().to_owned()),
        });
    }

    fn on_jitdump(&mut self, ev: JitdumpEvent<'_>) {
        let path = ev.path.to_path_buf();
        if !self.jitdump_paths.iter().any(|p| p == &path) {
            info!(
                "Detected JIT runtime: {} (PID {})",
                path.display(),
                ev.pid
            );
            self.jitdump_paths.push(path);
        }
    }

    fn on_kallsyms(&mut self, data: &[u8]) {
        info!("Embedding /proc/kallsyms ({} bytes) into archive.", data.len());
        let _ = self.write_packet(Packet::FileBlob {
            path: Cow::Borrowed(b"/proc/kallsyms"),
            data: Cow::Borrowed(data),
        });
    }

    fn on_wakeup(&mut self, ev: WakeupEvent<'_>) {
        // Forward to the live sink only -- the offline analysis
        // pipeline doesn't yet have a place for wakeup edges.
        if let Some(sink) = self.live_sink.as_ref() {
            sink.on_wakeup(&LiveWakeupEvent {
                timestamp: ev.timestamp_ns,
                pid: ev.pid,
                waker_tid: ev.waker_tid,
                wakee_tid: ev.wakee_tid,
                waker_user_stack: ev.waker_user_stack,
                waker_kernel_stack: ev.waker_kernel_stack,
            });
        }
    }

    fn on_macho_byte_source(
        &mut self,
        source: std::sync::Arc<dyn nerf_mac_capture::MachOByteSource>,
    ) {
        // Forward to the live sink so the binary registry can use
        // it for disassembly fallback; offline archive ignores.
        if let Some(sink) = self.live_sink.as_ref() {
            sink.on_macho_byte_source(source);
        }
    }
}

/// RAII guard for a child process we launched: kill + wait on drop.
///
/// `std::process::Child` deliberately doesn't kill on drop (the
/// stdlib leaves the policy to the caller), but the contract for
/// `nperf record --process` is "if nperf detached, the child should
/// detach too." We share the inner `Child` between the guard (for
/// kill+wait on the way out) and the `should_stop` closure (which
/// polls `try_wait()` so the recorder stops when the child exits on
/// its own).
struct ChildGuard {
    child: Arc<Mutex<Child>>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Arc::new(Mutex::new(child)) }
    }

    fn share(&self) -> Arc<Mutex<Child>> {
        self.child.clone()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            // Both calls are best-effort: child may have already
            // exited (kill returns EPERM or NotFound) and wait may
            // race with a try_wait that already reaped the zombie.
            let _ = c.kill();
            let _ = c.wait();
        }
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
