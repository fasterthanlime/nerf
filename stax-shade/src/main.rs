//! `stax-shade` — per-attachment companion process.
//!
//! ## Why "shade"?
//!
//! In classical mythology a **shade** is a soul or ghost paired with
//! the living: it sees through the target, reaches across the
//! boundary, and stays attached for the duration. The name carries
//! both the mystical register (an unseen counterpart) and the
//! pair register (one shade, one body) simultaneously. One syllable.
//!
//! ## Why a separate process?
//!
//! `stax-shade` is the only process in the stax architecture that
//! holds Mach **task port rights** to a target — every operation
//! that requires `task_for_pid` (peek, poke, suspend, register
//! state, code patching for syping, breakpoint exception ports)
//! lives here. It attaches as the same uid to non-hardened local
//! workloads; privileged / hardened-runtime targets are out of
//! scope for this tool.
//!
//! Isolating that capability matters for two reasons:
//!
//! 1. **Failure containment.** A crash in the unwinder, a
//!    misaligned write, or a bad exception-port dance shouldn't
//!    take down the run registry / aggregator (`stax-server`) or
//!    the kperf owner (`staxd`). One target = one shade = one
//!    blast radius.
//! 2. **Surface reduction.** `stax` (CLI), `stax-server`, and
//!    `staxd` do not need task ports. `staxd` remains the
//!    privileged kperf/kdebug owner only.
//!
//! ## Lifecycle
//!
//! Spawned by `stax-server` when a run starts; not a LaunchAgent.
//! The shade lives the length of the *attachment*, not of any
//! single sampling pass — pausing sampling doesn't release the
//! task port, the shade stays alive, sampling resumes without
//! re-attaching.
//!
//! Two attachment modes:
//!
//! - `--attach <pid>` — `task_for_pid` against a running process.
//! - `--launch -- <argv…>` — `posix_spawn(POSIX_SPAWN_START_SUSPENDED)`
//!   so the child is paused before its first instruction; the
//!   shade acquires the task port from the freshly-spawned PID,
//!   registers with stax-server, sets up attachment-side helpers,
//!   then resumes the target. Never miss an event.
//!
//! ## What this binary does *today*
//!
//! Parses args, opens the Mach task port, registers with
//! stax-server when requested, then idles until SIGINT/SIGTERM.
//! The old uncorrelated periodic walker is gone; correlated
//! probe/framehop work belongs here next, not in staxd.

#![cfg(target_os = "macos")]

mod probe;

use std::os::fd::RawFd;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::WrapErr;
use facet::Facet;
use figue as args;
use stax_core::cmd_record_mac::LiveOnlySink;
use stax_live_proto::{TerminalBrokerClient, TerminalInput, TerminalOutput, TerminalSize};
use stax_shade_proto::{ShadeAck, ShadeCapabilities, ShadeCommand, ShadeInfo, ShadeRegistryClient};

#[derive(Facet, Debug)]
struct Cli {
    #[facet(flatten)]
    builtins: args::FigueBuiltins,

    /// Attach to a running process by PID.
    #[facet(args::named, default)]
    attach: Option<u32>,

    /// Local socket path of the spawning stax-server.
    #[facet(args::named, default)]
    server_socket: Option<String>,

    /// Run id (assigned by stax-server) this attachment belongs to.
    #[facet(args::named, default)]
    run_id: Option<u64>,

    /// Local socket path of the privileged staxd daemon.
    #[facet(args::named, default = "/var/run/staxd.sock")]
    daemon_socket: String,

    /// PET sampling frequency, in Hz.
    #[facet(args::named, default = 900)]
    frequency: u32,

    /// Stop sampling after this many seconds. Unlimited by default.
    #[facet(args::named, default)]
    time_limit: Option<u64>,

    /// Evaluation mode: for each parsed kperf sample, suspend that
    /// exact Mach thread and emit a paired probe result.
    #[facet(args::named, default)]
    race_kperf: bool,

    /// Working directory to use when launching a target.
    #[facet(args::named, default)]
    cwd: Option<String>,

    /// Initial PTY height for launched targets.
    #[facet(args::named, default)]
    terminal_rows: Option<u16>,

    /// Initial PTY width for launched targets.
    #[facet(args::named, default)]
    terminal_cols: Option<u16>,

    /// Launch a fresh process and attach to it before its first
    /// instruction (POSIX_SPAWN_START_SUSPENDED). Mutually
    /// exclusive with --attach. Trailing argv after `--`.
    #[facet(args::named, default)]
    launch: bool,

    /// Program + arguments for `--launch`.
    #[facet(args::positional, default)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    let telemetry = stax_telemetry::TelemetryRegistry::new("stax-shade");
    let _telemetry_registration =
        stax_vox_observe::register_global_telemetry("stax-shade", "process", telemetry.clone());
    let _vox_sigusr1_dump = stax_vox_observe::install_global_sigusr1_dump("stax-shade");

    let cli: Cli = args::Driver::new(
        args::builder::<Cli>()
            .expect("failed to build CLI")
            .cli(|c| c.args(std::env::args().skip(1)))
            .help(|h| {
                h.program_name(env!("CARGO_PKG_NAME"))
                    .version(env!("CARGO_PKG_VERSION"))
            })
            .build(),
    )
    .run()
    .unwrap();

    if let Err(e) = run(cli, telemetry).await {
        tracing::error!("stax-shade failed: {e:?}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(cli: Cli, telemetry: stax_telemetry::TelemetryRegistry) -> eyre::Result<()> {
    let mode = match (cli.attach, cli.launch, cli.command.first()) {
        (Some(pid), false, _) => AttachMode::Existing(pid),
        (None, true, Some(_)) => AttachMode::Launch(cli.command.clone()),
        (Some(_), true, _) => {
            eyre::bail!("--attach and --launch are mutually exclusive")
        }
        (None, true, None) => {
            eyre::bail!("--launch requires a program after `--`")
        }
        (None, false, _) => {
            eyre::bail!("specify --attach <pid> or --launch -- <argv…>")
        }
    };

    let attached = match mode {
        AttachMode::Existing(pid) => {
            let task = task_for_pid(pid)?;
            tracing::info!(pid, task_port = task, "attached to existing process");
            Attached {
                pid,
                task,
                pre_resume: None,
                terminal: None,
            }
        }
        AttachMode::Launch(argv) => {
            if let Some(cwd) = &cli.cwd {
                std::env::set_current_dir(cwd)
                    .map_err(|e| eyre::eyre!("set cwd to {cwd:?}: {e}"))?;
            }
            let terminal_size = match (cli.terminal_rows, cli.terminal_cols) {
                (Some(rows), Some(cols)) => Some(TerminalSize { rows, cols }),
                _ => None,
            };
            launch_suspended(
                argv,
                cli.server_socket
                    .is_some()
                    .then_some(terminal_size)
                    .flatten(),
            )?
        }
    };
    let pid = attached.pid;
    let task = attached.task;

    let launched_pid = attached.pre_resume.as_ref().map(|_| pid);
    let terminal = attached.terminal;
    let _ = task;
    let server_socket = cli.server_socket.clone();

    match (server_socket.as_deref(), cli.run_id) {
        (Some(socket), Some(run_id)) => {
            run_recording(
                cli,
                socket,
                stax_live_proto::RunId(run_id),
                pid,
                task,
                attached.pre_resume,
                terminal,
                launched_pid,
                telemetry.clone(),
            )
            .await?;
        }
        (Some(_), None) => {
            eyre::bail!("--server-socket requires --run-id; stax-server owns run allocation")
        }
        (None, _) => {
            tracing::warn!(
                "no --server-socket; running standalone attachment with no recording pipeline"
            );
            if let Some(pre_resume) = attached.pre_resume {
                pre_resume.resume()?;
            }
            park_until_signal().await;
        }
    }

    Ok(())
}

async fn run_recording(
    cli: Cli,
    server_socket: &str,
    run_id: stax_live_proto::RunId,
    pid: u32,
    task: mach2::port::mach_port_t,
    pre_resume: Option<PreResume>,
    terminal: Option<Pty>,
    launched_pid: Option<u32>,
    telemetry: stax_telemetry::TelemetryRegistry,
) -> eyre::Result<()> {
    let recording_start = Instant::now();
    let run_id_raw = run_id.0;
    tracing::info!(
        run_id = run_id_raw,
        pid,
        launched_pid,
        has_pre_resume = pre_resume.is_some(),
        has_terminal = terminal.is_some(),
        race_kperf = cli.race_kperf,
        frequency_hz = cli.frequency,
        daemon_socket = %cli.daemon_socket,
        "shade recording lifecycle starting"
    );

    let phase_start = Instant::now();
    let (_server_client, mut commands, _server_debug_registration) =
        register_with_server(server_socket, run_id.0, pid, telemetry.clone()).await?;
    tracing::info!(
        run_id = run_id.0,
        elapsed = ?phase_start.elapsed(),
        "shade registered with server"
    );
    let phase_start = Instant::now();
    let (ingest_sink, forwarder) = stax_core::ingest_sink::connect_to_existing_run_with_telemetry(
        server_socket,
        run_id,
        Some(telemetry.clone()),
    )
    .await?;
    tracing::info!(
        run_id = run_id.0,
        elapsed = ?phase_start.elapsed(),
        "shade connected ingest sink"
    );
    let phase_start = Instant::now();
    let terminal_pump = match terminal {
        Some(pty) => {
            Some(start_terminal_pump(server_socket, run_id, pty, telemetry.clone()).await?)
        }
        None => None,
    };
    tracing::info!(
        run_id = run_id.0,
        elapsed = ?phase_start.elapsed(),
        has_terminal_pump = terminal_pump.is_some(),
        "shade terminal pump configured"
    );

    let sink = LiveOnlySink::new(Some(Box::new(ingest_sink)));
    sink.notify_target_attached(pid);
    let stop_via_sink = sink.live_sink_stop_flag();
    let sink = if cli.race_kperf {
        tracing::info!("race-kperf probe enabled");
        probe::RaceKperfSink::enabled(task, sink)
    } else {
        probe::RaceKperfSink::disabled(sink)
    };
    let race_probe_trigger = sink.trigger();

    let opts = staxd_client::RemoteOptions {
        daemon_socket: cli.daemon_socket,
        pid,
        frequency_hz: cli.frequency,
        duration: cli.time_limit.map(Duration::from_secs),
        telemetry: Some(telemetry.clone()),
        ..Default::default()
    };
    let drive_pid = opts.pid;
    let drive_frequency = opts.frequency_hz;
    let drive_buf_records = opts.buf_records;

    let server_stop_requested = Arc::new(AtomicBool::new(false));
    let server_stop_for_task = server_stop_requested.clone();
    tokio::spawn(async move {
        match commands.recv().await {
            Ok(Some(command_sref)) => {
                let mut command = None;
                let _ = command_sref.map(|value| {
                    command = Some(value);
                });
                match command {
                    Some(ShadeCommand::Stop) => {
                        tracing::info!("stop requested by stax-server");
                        server_stop_for_task.store(true, Ordering::Relaxed);
                    }
                    None => {}
                }
            }
            Ok(None) => {
                tracing::info!("shade command channel closed");
                server_stop_for_task.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!("shade command channel failed: {e:?}");
                server_stop_for_task.store(true, Ordering::Relaxed);
            }
        }
    });

    tracing::info!(
        run_id = run_id.0,
        pid,
        "shade starting staxd recording pipeline"
    );
    let pre_resume = Arc::new(Mutex::new(pre_resume));
    let pre_resume_for_recording_start = pre_resume.clone();
    let child_exit = Arc::new(Mutex::new(None));
    let child_exit_for_stop = child_exit.clone();
    tracing::info!(
        run_id = run_id.0,
        pid = drive_pid,
        frequency_hz = drive_frequency,
        buf_records = drive_buf_records,
        elapsed = ?recording_start.elapsed(),
        "shade entering staxd-client drive_session"
    );
    let result = staxd_client::drive_session_with_hooks(
        opts,
        sink,
        move || {
            if server_stop_requested.load(Ordering::Relaxed) {
                return true;
            }
            if stop_via_sink() {
                return true;
            }
            if let Some(pid) = launched_pid
                && child_exit_for_stop
                    .lock()
                    .expect("child_exit poisoned")
                    .is_none()
                && let Some(exit) = launched_child_exited(pid)
            {
                *child_exit_for_stop.lock().expect("child_exit poisoned") = Some(exit);
                return true;
            }
            false
        },
        move || {
            tracing::info!(
                run_id = run_id_raw,
                "shade observed staxd ready/first batch; resuming launch-suspended target if present"
            );
            if let Some(pre_resume) = pre_resume_for_recording_start
                .lock()
                .expect("pre_resume poisoned")
                .take()
            {
                if let Err(err) = pre_resume.resume() {
                    tracing::warn!("failed to resume target after recording started: {err}");
                }
            }
        },
        move |tid, timing| {
            if let Some(trigger) = race_probe_trigger.as_ref() {
                trigger.enqueue(tid, timing);
            }
        },
    )
    .await;
    match &result {
        Ok(()) => tracing::info!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            "shade staxd-client drive_session completed"
        ),
        Err(e) => tracing::warn!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            error = %e,
            "shade staxd-client drive_session failed"
        ),
    }

    if let Some(pre_resume) = pre_resume.lock().expect("pre_resume poisoned").take() {
        tracing::warn!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            "shade drive_session returned before staxd-ready resume hook fired; resuming target now"
        );
        pre_resume.resume()?;
    }

    if let Some(pid) = launched_pid {
        if child_exit.lock().expect("child_exit poisoned").is_none()
            && let Some(exit) = terminate_launched_child(pid)
        {
            *child_exit.lock().expect("child_exit poisoned") = Some(exit);
        }
    }

    if let Some(terminal_pump) = terminal_pump {
        if let Some(exit) = *child_exit.lock().expect("child_exit poisoned") {
            terminal_pump.report_exit(exit);
        }
        tracing::info!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            "shade finishing terminal pump"
        );
        terminal_pump.finish().await;
    }

    tracing::info!(
        run_id = run_id.0,
        elapsed = ?recording_start.elapsed(),
        "shade awaiting ingest forwarder"
    );
    if matches!(
        &result,
        Err(staxd_client::Error::WorkerShutdownTimedOut { .. })
    ) {
        tracing::warn!(
            run_id = run_id.0,
            elapsed = ?recording_start.elapsed(),
            "shade parser worker detached; waiting briefly for ingest forwarder to drain"
        );
        match tokio::time::timeout(Duration::from_secs(5), forwarder).await {
            Ok(Ok(())) => tracing::info!(
                run_id = run_id.0,
                elapsed = ?recording_start.elapsed(),
                "shade ingest forwarder drained after parser detach"
            ),
            Ok(Err(e)) => tracing::warn!("ingest forwarder task ended unexpectedly: {e}"),
            Err(_) => tracing::warn!(
                run_id = run_id.0,
                elapsed = ?recording_start.elapsed(),
                "shade ingest forwarder still blocked after parser detach drain timeout"
            ),
        }
    } else if let Err(e) = forwarder.await {
        tracing::warn!("ingest forwarder task ended unexpectedly: {e}");
    }

    result.map_err(|e| eyre::eyre!("staxd-client failed: {e}"))?;
    Ok(())
}

enum AttachMode {
    Existing(u32),
    Launch(Vec<String>),
}

struct Attached {
    pid: u32,
    task: mach2::port::mach_port_t,
    /// `Some` for `--launch`: target was started suspended via
    /// POSIX_SPAWN_START_SUSPENDED and is waiting for us to resume
    /// it. `None` for `--attach`: target was already running.
    pre_resume: Option<PreResume>,
    terminal: Option<Pty>,
}

struct PreResume {
    task: mach2::port::mach_port_t,
}

impl PreResume {
    fn resume(self) -> eyre::Result<()> {
        use mach2::kern_return::KERN_SUCCESS;
        use mach2::task::task_resume;
        // SAFETY: task is a valid Mach port acquired via task_for_pid
        // on the just-spawned child. task_resume is safe to call on
        // a suspended task port owned by us.
        let kr = unsafe { task_resume(self.task) };
        if kr != KERN_SUCCESS {
            eyre::bail!("task_resume failed: kr={kr}");
        }
        tracing::info!("target resumed");
        Ok(())
    }
}

struct Pty {
    master: RawFd,
    slave: RawFd,
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            if self.master >= 0 {
                libc::close(self.master);
            }
            if self.slave >= 0 {
                libc::close(self.slave);
            }
        }
    }
}

fn open_pty(size: TerminalSize) -> eyre::Result<Pty> {
    use std::ptr;

    let mut master = -1;
    let mut slave = -1;
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut winsize,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).wrap_err("openpty");
    }
    Ok(Pty { master, slave })
}

fn set_pty_size(fd: RawFd, size: TerminalSize) {
    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &winsize);
    }
}

/// Spawn a fresh child via `posix_spawn` with
/// `POSIX_SPAWN_START_SUSPENDED`, acquire its task port, and
/// hand back the suspended-attachment record. The caller is
/// expected to do whatever pre-resume setup it needs (register
/// with stax-server, wait for kperf to be primed, install
/// breakpoints, …) and then call `PreResume::resume`.
///
/// Argv: `argv[0]` is the program path; the rest are passed to
/// the child as-is. `posix_spawnp` keeps CLI behavior aligned with
/// `std::process::Command`: bare program names resolve through PATH.
fn launch_suspended(
    argv: Vec<String>,
    terminal_size: Option<TerminalSize>,
) -> eyre::Result<Attached> {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::ptr;

    if argv.is_empty() {
        eyre::bail!("--launch requires at least one positional argument (the program path)");
    }

    let program = CString::new(argv[0].as_str())
        .map_err(|_| eyre::eyre!("program path contains an interior NUL"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| eyre::eyre!("argv contains an interior NUL"))?;
    let mut argv_p: Vec<*mut c_char> = argv_c.iter().map(|c| c.as_ptr() as *mut c_char).collect();
    argv_p.push(ptr::null_mut());

    let mut pty = match terminal_size {
        Some(size) => Some(open_pty(size)?),
        None => None,
    };

    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    // SAFETY: posix_spawnattr_init writes through the out-pointer.
    // We pair it with destroy below so the kernel side cleans up.
    let r = unsafe { libc::posix_spawnattr_init(&mut attr) };
    if r != 0 {
        eyre::bail!("posix_spawnattr_init: {r}");
    }
    // The whole point: child stays parked at its first instruction
    // until we task_resume. SETSIGDEF is recommended by Apple's
    // header so the child gets a clean signal mask regardless of
    // ours.
    let flags = libc::POSIX_SPAWN_START_SUSPENDED | libc::POSIX_SPAWN_SETSIGDEF;
    let r = unsafe { libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short) };
    if r != 0 {
        unsafe {
            libc::posix_spawnattr_destroy(&mut attr);
        }
        eyre::bail!("posix_spawnattr_setflags: {r}");
    }

    let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    let actions_ptr = if let Some(pty) = &pty {
        let r = unsafe { libc::posix_spawn_file_actions_init(&mut actions) };
        if r != 0 {
            unsafe {
                libc::posix_spawnattr_destroy(&mut attr);
            }
            eyre::bail!("posix_spawn_file_actions_init: {r}");
        }
        for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            let r = unsafe { libc::posix_spawn_file_actions_adddup2(&mut actions, pty.slave, fd) };
            if r != 0 {
                unsafe {
                    libc::posix_spawn_file_actions_destroy(&mut actions);
                    libc::posix_spawnattr_destroy(&mut attr);
                }
                eyre::bail!("posix_spawn_file_actions_adddup2({fd}): {r}");
            }
        }
        if pty.slave > libc::STDERR_FILENO {
            let r = unsafe { libc::posix_spawn_file_actions_addclose(&mut actions, pty.slave) };
            if r != 0 {
                unsafe {
                    libc::posix_spawn_file_actions_destroy(&mut actions);
                    libc::posix_spawnattr_destroy(&mut attr);
                }
                eyre::bail!("posix_spawn_file_actions_addclose(slave): {r}");
            }
        }
        let r = unsafe { libc::posix_spawn_file_actions_addclose(&mut actions, pty.master) };
        if r != 0 {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut actions);
                libc::posix_spawnattr_destroy(&mut attr);
            }
            eyre::bail!("posix_spawn_file_actions_addclose(master): {r}");
        }
        &actions as *const libc::posix_spawn_file_actions_t
    } else {
        ptr::null()
    };

    let mut pid: libc::pid_t = 0;
    let r = unsafe {
        libc::posix_spawnp(
            &mut pid,
            program.as_ptr(),
            actions_ptr,
            &attr,
            argv_p.as_ptr(),
            // Inherit our environment as-is — we want PATH /
            // DYLD_* / etc. flowing through to the child.
            extern_environ(),
        )
    };
    unsafe {
        if !actions.is_null() {
            libc::posix_spawn_file_actions_destroy(&mut actions);
        }
        libc::posix_spawnattr_destroy(&mut attr);
    }
    if r != 0 {
        eyre::bail!("posix_spawn({}): {r}", argv[0]);
    }
    let pid_u32 = pid as u32;
    tracing::info!(pid = pid_u32, program = %argv[0], "spawned target (suspended)");

    let task = task_for_pid(pid_u32).inspect_err(|_| {
        // Best-effort: the child is suspended and we own it; if
        // task_for_pid failed, there's no point leaving the
        // process around. SIGKILL it and reap.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    })?;

    Ok(Attached {
        pid: pid_u32,
        task,
        pre_resume: Some(PreResume { task }),
        terminal: pty.take(),
    })
}

#[derive(Clone, Copy)]
struct ChildExit {
    code: Option<i32>,
    signal: Option<i32>,
}

fn launched_child_exited(pid: u32) -> Option<ChildExit> {
    let mut status = 0;
    // SAFETY: waitpid is called for the direct child this shade
    // spawned. WNOHANG makes it a polling liveness check.
    let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if r == pid as libc::pid_t {
        Some(decode_wait_status(status))
    } else if r == -1 {
        Some(ChildExit {
            code: None,
            signal: None,
        })
    } else {
        None
    }
}

fn terminate_launched_child(pid: u32) -> Option<ChildExit> {
    let mut status = 0;
    // SAFETY: same direct child as above.
    let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if r == 0 {
        // Match the previous CLI ChildGuard semantics: when the
        // recording ends because of a time limit or user stop, the
        // launched target is not left running in the background.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(pid as libc::pid_t, &mut status, 0);
        }
        Some(decode_wait_status(status))
    } else if r == pid as libc::pid_t {
        Some(decode_wait_status(status))
    } else {
        None
    }
}

fn decode_wait_status(status: libc::c_int) -> ChildExit {
    if libc::WIFEXITED(status) {
        ChildExit {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        ChildExit {
            code: None,
            signal: Some(libc::WTERMSIG(status)),
        }
    } else {
        ChildExit {
            code: None,
            signal: None,
        }
    }
}

struct TerminalPump {
    events: tokio::sync::mpsc::UnboundedSender<TerminalOutput>,
    output_task: tokio::task::JoinHandle<()>,
    _debug_registration: stax_vox_observe::VoxDebugRegistration,
}

impl TerminalPump {
    fn report_exit(&self, exit: ChildExit) {
        let _ = self.events.send(TerminalOutput::ExitStatus {
            code: exit.code,
            signal: exit.signal,
        });
    }

    async fn finish(self) {
        drop(self.events);
        let _ = self.output_task.await;
    }
}

async fn start_terminal_pump(
    socket: &str,
    run_id: stax_live_proto::RunId,
    mut pty: Pty,
    telemetry: stax_telemetry::TelemetryRegistry,
) -> eyre::Result<TerminalPump> {
    let url = format!("local://{socket}");
    let client: TerminalBrokerClient = vox::connect(&url)
        .observer(
            stax_vox_observe::VoxObserverLogger::new("stax-shade", "terminal")
                .with_telemetry(telemetry),
        )
        .await?;
    let debug_registration = stax_vox_observe::register_global_caller(
        "stax-shade",
        "terminal",
        "TerminalBroker",
        &client.caller,
    );
    let (input_to_shade, mut input_from_server) = vox::channel::<TerminalInput>();
    let (output_to_server, output_from_shade) = vox::channel::<TerminalOutput>();

    client
        .attach_terminal(run_id, input_to_shade, output_from_shade)
        .await
        .map_err(|e| eyre::eyre!("attach_terminal failed: {e:?}"))?;

    let read_fd = pty.master;
    let write_fd = unsafe { libc::dup(read_fd) };
    if write_fd < 0 {
        return Err(std::io::Error::last_os_error()).wrap_err("dup pty master");
    }
    unsafe {
        libc::close(pty.slave);
    }
    pty.master = -1;
    pty.slave = -1;

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<TerminalOutput>();
    let events_from_reader = events_tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let data = buf[..n as usize].to_vec();
                if events_from_reader
                    .send(TerminalOutput::Bytes { data })
                    .is_err()
                {
                    break;
                }
                continue;
            }
            if n == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                // macOS reports EIO when the slave side of a PTY closes.
                Some(libc::EIO) => break,
                _ => {
                    let _ = events_from_reader.send(TerminalOutput::Error {
                        message: format!("pty read failed: {err}"),
                    });
                    break;
                }
            }
        }
        unsafe {
            libc::close(read_fd);
        }
    });

    let (input_tx, input_rx) = std::sync::mpsc::channel::<TerminalInput>();
    std::thread::spawn(move || {
        for input in input_rx {
            match input {
                TerminalInput::Bytes { data } => {
                    let mut offset = 0;
                    while offset < data.len() {
                        let n = unsafe {
                            libc::write(
                                write_fd,
                                data[offset..].as_ptr().cast(),
                                data.len() - offset,
                            )
                        };
                        if n > 0 {
                            offset += n as usize;
                        } else if std::io::Error::last_os_error().raw_os_error()
                            != Some(libc::EINTR)
                        {
                            break;
                        }
                    }
                }
                TerminalInput::Resize { size } => set_pty_size(write_fd, size),
                TerminalInput::Close => break,
            }
        }
        unsafe {
            libc::close(write_fd);
        }
    });

    tokio::spawn(async move {
        loop {
            match input_from_server.recv().await {
                Ok(Some(input_sref)) => {
                    let mut input = None;
                    let _ = input_sref.map(|value| {
                        input = Some(value);
                    });
                    if input_tx.send(input.expect("input set")).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = input_tx.send(TerminalInput::Close);
                    break;
                }
                Err(e) => {
                    tracing::warn!("terminal input recv failed: {e:?}");
                    let _ = input_tx.send(TerminalInput::Close);
                    break;
                }
            }
        }
    });

    let output_task = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            if output_to_server.send(event).await.is_err() {
                break;
            }
        }
        let _ = output_to_server.close(Default::default()).await;
    });

    Ok(TerminalPump {
        events: events_tx,
        output_task,
        _debug_registration: debug_registration,
    })
}

unsafe extern "C" {
    static environ: *mut *mut std::os::raw::c_char;
}

fn extern_environ() -> *const *mut std::os::raw::c_char {
    // SAFETY: read of process-wide global. macOS exposes
    // `environ` as the canonical envp; posix_spawn accepts a
    // const pointer to it.
    unsafe { environ as *const _ }
}

async fn register_with_server(
    socket: &str,
    run_id: u64,
    target_pid: u32,
    telemetry: stax_telemetry::TelemetryRegistry,
) -> eyre::Result<(
    ShadeRegistryClient,
    vox::Rx<ShadeCommand>,
    stax_vox_observe::VoxDebugRegistration,
)> {
    let url = format!("local://{socket}");
    let client: ShadeRegistryClient = vox::connect(&url)
        .observer(
            stax_vox_observe::VoxObserverLogger::new("stax-shade", "shade-registry")
                .with_telemetry(telemetry),
        )
        .await?;
    let debug_registration = stax_vox_observe::register_global_caller(
        "stax-shade",
        "shade-registry",
        "ShadeRegistry",
        &client.caller,
    );
    let (commands_to_shade, commands_from_server) = vox::channel::<ShadeCommand>();
    let info = ShadeInfo {
        run_id,
        target_pid,
        shade_pid: std::process::id(),
        capabilities: ShadeCapabilities {
            peek: false,
            poke: false,
            // The old periodic walker was intentionally removed.
            // Flip this when shade owns the correlated probe/walk.
            framehop_walker: false,
            breakpoint_step: false,
        },
    };
    match client.register_shade(info, commands_to_shade).await {
        Ok(ShadeAck { accepted: true, .. }) => {
            tracing::info!(run_id, "registered with stax-server");
            Ok((client, commands_from_server, debug_registration))
        }
        Ok(ShadeAck {
            accepted: false,
            reason,
        }) => {
            eyre::bail!(
                "stax-server rejected registration: {}",
                reason.unwrap_or_else(|| "(no reason)".to_owned())
            )
        }
        Err(vox::VoxError::User(err)) => eyre::bail!("server returned error: {err:?}"),
        Err(e) => eyre::bail!("vox register_shade failed: {e:?}"),
    }
}

fn task_for_pid(pid: u32) -> eyre::Result<mach2::port::mach_port_t> {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::port::{MACH_PORT_NULL, mach_port_t};
    use mach2::traps::{mach_task_self, task_for_pid};

    let mut task: mach_port_t = MACH_PORT_NULL;
    // SAFETY: out-pointer is valid for the duration; pid is a plain
    // integer; mach_task_self is always-safe.
    let kr = unsafe { task_for_pid(mach_task_self(), pid as i32, &mut task) };
    if kr != KERN_SUCCESS {
        eyre::bail!(
            "task_for_pid({pid}) failed: kr={kr} \
             (expected same-uid, non-hardened target; privileged and \
             hardened-runtime targets are out of scope)"
        );
    }
    Ok(task)
}

/// Idle until SIGINT or SIGTERM. Stage C will replace this with
/// awaiting on the vox session's `closed()` future once the server
/// can actually call into `Shade` and drive a real teardown.
///
/// Earlier versions also raced a `spawn_blocking(read stdin)` so
/// closing the parent's pipe would terminate the shade. That made
/// ctrl-c hang: the blocking-pool thread was stuck in a `read()`
/// syscall forever, and tokio's runtime drop waits for the
/// blocking pool. Signals alone are enough — stax-server kills the
/// shade with SIGTERM at run-end.
async fn park_until_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("install SIGINT handler failed: {e}");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("install SIGTERM handler failed: {e}");
            return;
        }
    };
    tokio::select! {
        _ = sigint.recv() => tracing::info!("received SIGINT, shutting down"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax_shade=info,vox::client=debug"));

    let oslog = tracing_oslog::OsLogger::new("eu.bearcove.stax-shade", "default");

    tracing_subscriber::registry()
        .with(filter)
        .with(oslog)
        .init();
}
