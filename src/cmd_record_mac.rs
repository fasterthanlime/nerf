//! macOS `stax record`: drives the staxd daemon backend over a local
//! socket and tees its `SampleSink` events into the live (`--serve`)
//! WebSocket aggregator.

use std::error::Error;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stax_mac_capture::{
    BinaryLoadedEvent, BinaryUnloadedEvent, JitdumpEvent, SampleEvent, SampleSink,
    ThreadNameEvent, WakeupEvent,
};
use nwind::UserFrame;

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
    match args.target()? {
        TargetProcess::ByPid(pid) => record_existing_pid(args, pid, live_sink),
        TargetProcess::Launch { program, args: prog_args } => {
            record_child_launch(args, program, prog_args, live_sink)
        }
    }
}

fn record_existing_pid(
    args: args::RecordArgs,
    pid: u32,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    info!("Recording PID {pid}");
    let sink = LiveOnlySink { live_sink };
    notify_target_attached(&sink, pid);

    let sigint = SigintHandler::new();
    let time_limit = args.time_limit.map(Duration::from_secs);

    let opts = staxd_client::RemoteOptions {
        daemon_socket: args.daemon_socket.clone(),
        pid,
        frequency_hz: args.frequency,
        duration: time_limit,
        ..Default::default()
    };

    let stop_via_sink = sink.live_sink_stop_flag();
    let should_stop = || sigint.was_triggered() || stop_via_sink();

    // Multi-thread for keepalive pongs etc. Sync kdebug parsing
    // + libproc scanning runs on a dedicated OS thread inside
    // drive_session.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .map_err(|err| format!("daemon backend: tokio runtime build: {err}"))?;
    info!("Running... press Ctrl-C to stop.");
    if let Err(err) = rt.block_on(staxd_client::drive_session(opts, sink, should_stop)) {
        return Err(format!("staxd-client failed: {err}").into());
    }
    info!("Recording complete.");
    Ok(())
}

fn record_child_launch(
    args: args::RecordArgs,
    program: String,
    program_args: Vec<String>,
    live_sink: Option<Box<dyn LiveSink>>,
) -> Result<(), Box<dyn Error>> {
    info!("Launching {}...", program);
    let mut cmd = Command::new(&program);
    cmd.args(&program_args);
    let child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn {program}: {err}"))?;
    let pid = child.id();
    let child_guard = ChildGuard::new(child);
    let child_for_stop = child_guard.share();
    info!("Child started: PID {pid}");

    let sink = LiveOnlySink { live_sink };
    notify_target_attached(&sink, pid);

    let sigint = SigintHandler::new();
    let time_limit = args.time_limit.map(Duration::from_secs);

    let opts = staxd_client::RemoteOptions {
        daemon_socket: args.daemon_socket.clone(),
        pid,
        frequency_hz: args.frequency,
        duration: time_limit,
        ..Default::default()
    };

    let stop_via_sink = sink.live_sink_stop_flag();
    let should_stop = move || {
        if sigint.was_triggered() || stop_via_sink() {
            return true;
        }
        match child_for_stop.lock() {
            Ok(mut c) => matches!(c.try_wait(), Ok(Some(_))),
            Err(_) => true,
        }
    };

    // Multi-thread for keepalive pongs etc. The synchronous kdebug
    // parsing + libproc scanning has been hoisted onto a dedicated
    // OS thread inside drive_session, so the tokio workers stay
    // available for vox I/O regardless of how slow a Mach-O parse
    // happens to be.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .map_err(|err| format!("daemon backend: tokio runtime build: {err}"))?;
    info!("Running... press Ctrl-C to stop.");
    if let Err(err) = rt.block_on(staxd_client::drive_session(opts, sink, should_stop)) {
        return Err(format!("staxd-client failed: {err}").into());
    }

    // child_guard drops at end of scope, killing + reaping the child.
    info!("Recording complete.");
    Ok(())
}

/// `SampleSink` impl that forwards every event to a live sink (if any)
/// and drops it otherwise. There's no on-disk archive in the live-only
/// path.
struct LiveOnlySink {
    live_sink: Option<Box<dyn LiveSink>>,
}

/// The daemon path doesn't surface a `task_for_pid` handle on its
/// own (broker is a follow-up), but stax-server still wants a PID on
/// every `RunSummary` so `stax list` / `stax status` can show
/// something useful. Synthesise one with task_port=0 the moment we
/// know the pid.
fn notify_target_attached(sink: &LiveOnlySink, pid: u32) {
    if let Some(live) = sink.live_sink.as_ref() {
        futures::executor::block_on(
            live.on_target_attached(&TargetAttached { pid, task_port: 0 }),
        );
    }
}

impl LiveOnlySink {
    /// Build a `Fn() -> bool` that reflects the live sink's
    /// out-of-band stop signal (server closed ingest channel,
    /// etc.). Returns a closure that always returns `false` when
    /// the sink doesn't expose a stop flag — keeps the
    /// `should_stop` pattern in `record_*` symmetric.
    fn live_sink_stop_flag(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let flag = self.live_sink.as_ref().and_then(|s| s.stop_flag());
        move || {
            flag.as_ref()
                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
        }
    }
}

/// Bridge between the kperf-parse pipeline (sync `SampleSink`)
/// and the async `LiveSink`. Each callback drives the async fn to
/// completion via `futures::executor::block_on` — that's
/// independent of tokio so it's safe to invoke from inside an
/// already-async context.
///
/// The contract with `LiveSink` impls is that hot-path callbacks
/// (`on_sample`, `on_cpu_interval`) don't yield: their bodies push
/// onto a sync-friendly mpsc and return. Slow paths
/// (`on_binary_loaded` for parallel image walks) can yield
/// freely; block_on still works, the caller just blocks until the
/// future settles.
fn block_sink<F: std::future::Future<Output = ()>>(fut: F) {
    futures::executor::block_on(fut);
}

impl SampleSink for LiveOnlySink {
    fn on_sample(&mut self, ev: SampleEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        let user_backtrace: Vec<UserFrame> = ev
            .backtrace
            .iter()
            .map(|&address| UserFrame {
                address,
                initial_address: None,
            })
            .collect();
        block_sink(sink.on_sample(&LiveSampleEvent {
            timestamp: ev.timestamp_ns,
            pid: ev.pid,
            tid: ev.tid,
            cpu: u32::MAX,
            kernel_backtrace: ev.kernel_backtrace,
            user_backtrace: &user_backtrace,
            cycles: ev.cycles,
            instructions: ev.instructions,
            l1d_misses: ev.l1d_misses,
            branch_mispreds: ev.branch_mispreds,
        }));
    }

    fn on_cpu_interval(&mut self, ev: stax_mac_capture::sample_sink::CpuIntervalEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        match ev.kind {
            stax_mac_capture::sample_sink::CpuIntervalKind::OnCpu => {
                block_sink(sink.on_cpu_interval(&crate::live_sink::CpuIntervalEvent {
                    pid: ev.pid,
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    kind: crate::live_sink::CpuIntervalKind::OnCpu,
                }));
            }
            stax_mac_capture::sample_sink::CpuIntervalKind::OffCpu {
                stack,
                waker_tid,
                waker_user_stack,
            } => {
                let stack: Vec<UserFrame> = stack
                    .iter()
                    .map(|&address| UserFrame {
                        address,
                        initial_address: None,
                    })
                    .collect();
                block_sink(sink.on_cpu_interval(&crate::live_sink::CpuIntervalEvent {
                    pid: ev.pid,
                    tid: ev.tid,
                    start_ns: ev.start_ns,
                    end_ns: ev.end_ns,
                    kind: crate::live_sink::CpuIntervalKind::OffCpu {
                        stack: &stack,
                        waker_tid,
                        waker_user_stack,
                    },
                }));
            }
        }
    }

    fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        let live_symbols: Vec<LiveSymbol<'_>> = ev
            .symbols
            .iter()
            .map(|s| LiveSymbol {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: &s.name,
            })
            .collect();
        block_sink(sink.on_binary_loaded(&LiveBinaryLoadedEvent {
            path: ev.path,
            base_avma: ev.base_avma,
            vmsize: ev.vmsize,
            text_svma: ev.text_svma,
            arch: ev.arch,
            is_executable: ev.is_executable,
            symbols: &live_symbols,
            text_bytes: ev.text_bytes,
        }));
    }

    fn on_binary_unloaded(&mut self, ev: BinaryUnloadedEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_binary_unloaded(&LiveBinaryUnloadedEvent {
            path: ev.path,
            base_avma: ev.base_avma,
        }));
    }

    fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_thread_name(&LiveThreadName {
            pid: ev.pid,
            tid: ev.tid,
            name: ev.name,
        }));
    }

    fn on_jitdump(&mut self, _ev: JitdumpEvent<'_>) {}

    fn on_wakeup(&mut self, ev: WakeupEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_wakeup(&LiveWakeupEvent {
            timestamp: ev.timestamp_ns,
            pid: ev.pid,
            waker_tid: ev.waker_tid,
            wakee_tid: ev.wakee_tid,
            waker_user_stack: ev.waker_user_stack,
            waker_kernel_stack: ev.waker_kernel_stack,
        }));
    }

    fn on_probe_result(&mut self, ev: stax_mac_capture::ProbeResultEvent<'_>) {
        let Some(sink) = self.live_sink.as_ref() else {
            return;
        };
        block_sink(sink.on_probe_result(
            &crate::live_sink::ProbeResultEvent {
                tid: ev.tid,
                kperf_ts: ev.kperf_ts_ns,
                probe_done_ns: ev.probe_done_ns,
                mach_pc: ev.mach_pc,
                mach_lr: ev.mach_lr,
                mach_fp: ev.mach_fp,
                mach_sp: ev.mach_sp,
                mach_walked: ev.mach_walked,
                used_framehop: ev.used_framehop,
            },
        ));
    }

    fn on_macho_byte_source(
        &mut self,
        source: std::sync::Arc<dyn stax_mac_capture::MachOByteSource>,
    ) {
        if let Some(sink) = self.live_sink.as_ref() {
            block_sink(sink.on_macho_byte_source(source));
        }
    }
}

/// RAII guard for a launched child: kill + wait on drop. Shared with
/// `should_stop` so the recorder also stops when the child exits on its
/// own.
struct ChildGuard {
    child: Arc<Mutex<Child>>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child: Arc::new(Mutex::new(child)),
        }
    }

    fn share(&self) -> Arc<Mutex<Child>> {
        self.child.clone()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

