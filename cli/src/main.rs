use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::exit;

use figue as args;
use stax_core::{
    args::{
        AnnotateArgs, Cli, Command, FlameArgs, ProbeDiffArgs, RecordArgs, ThreadsArgs, TopArgs,
        WaitArgs,
    },
    cmd_setup_mac,
};
use stax_live_proto::{
    DiagnosticsSnapshot, FlameNode, FlamegraphUpdate, HistogramSnapshot, LaunchEnvVar,
    LaunchRequest, LiveFilter, OffCpuBreakdown, ProbeDiffEntry, ProbeDiffUpdate, ProfilerClient,
    RunControlClient, RunSummary, ServerStatus, StopReason, TelemetrySnapshot, TerminalInput,
    TerminalOutput, TerminalSize, ThreadsUpdate, TopSort, ViewParams, WaitCondition, WaitOutcome,
};

fn main_impl() -> Result<(), Box<dyn Error>> {
    if env::var("RUST_LOG").is_err() {
        // cranelift_jit/cranelift_codegen log every JIT'd function at info,
        // which floods the terminal once we start the live RPC server.
        unsafe {
            env::set_var("RUST_LOG", "info,cranelift_jit=warn,cranelift_codegen=warn");
        }
    }

    env_logger::init();
    init_tracing();
    let _vox_sigusr1_dump = stax_vox_observe::install_global_sigusr1_dump("stax");

    let cli: Cli = args::Driver::new(
        args::builder::<Cli>()
            .expect("failed to build CLI")
            .cli(|c| c.args(env::args().skip(1)))
            .help(|h| {
                h.program_name(env!("CARGO_PKG_NAME"))
                    .version(env!("CARGO_PKG_VERSION"))
            })
            .build(),
    )
    .run()
    .unwrap();

    match cli.command {
        Command::Record(args) => run_record(args)?,
        Command::Setup(args) => cmd_setup_mac::main(args)?,
        Command::Status => block_on_async(async { run_status().await })?,
        Command::List => block_on_async(async { run_list().await })?,
        Command::Diagnose => block_on_async(async { run_diagnose().await })?,
        Command::Dump => run_dump()?,
        Command::Wait(args) => block_on_async(async { run_wait(args).await })?,
        Command::Stop => block_on_async(async { run_stop().await })?,
        Command::Top(args) => block_on_async(async { run_top(args).await })?,
        Command::Annotate(args) => block_on_async(async { run_annotate(args).await })?,
        Command::Flame(args) => block_on_async(async { run_flame(args).await })?,
        Command::Threads(args) => block_on_async(async { run_threads(args).await })?,
        Command::ProbeDiff(args) => block_on_async(async { run_probe_diff(args).await })?,
    }
    Ok(())
}

fn main() {
    if let Err(error) = main_impl() {
        eprintln!("error: {error}");
        exit(1);
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax=info,stax_vox_observe=info"));
    let oslog = tracing_oslog::OsLogger::new("eu.bearcove.stax", "default");
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(oslog)
        .try_init();
}

fn block_on_async<F: std::future::Future<Output = Result<(), Box<dyn Error>>>>(
    fut: F,
) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(fut)
}

fn run_record(args: RecordArgs) -> Result<(), Box<dyn Error>> {
    block_on_async(async { run_record_async(args).await })
}

fn stax_server_socket() -> Option<PathBuf> {
    if let Ok(p) = env::var("STAX_SERVER_SOCKET") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    if let Ok(rt) = env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt).join("stax-server.sock");
        if p.exists() {
            return Some(p);
        }
    }
    let uid = unsafe { libc::getuid() };
    let p = PathBuf::from(format!("/tmp/stax-server-{uid}.sock"));
    p.exists().then_some(p)
}

// --- agent-facing subcommands ------------------------------------------

fn run_dump() -> Result<(), Box<dyn Error>> {
    let self_pid = std::process::id();
    let mut targets = Vec::new();
    for name in ["staxd", "stax-server", "stax-shade", "stax"] {
        for pid in pids_by_exact_process_name(name)? {
            if pid != self_pid {
                targets.push(DumpTarget {
                    name: name.to_owned(),
                    pid,
                });
            }
        }
    }
    targets.sort_by(|a, b| (a.pid, &a.name).cmp(&(b.pid, &b.name)));
    targets.dedup_by_key(|target| target.pid);

    if targets.is_empty() {
        println!("no stax processes found");
        return Ok(());
    }

    let mut failed = false;
    for target in targets {
        let rc = unsafe { libc::kill(target.pid as libc::pid_t, libc::SIGUSR1) };
        if rc == 0 {
            println!("signaled {} pid {}", target.name, target.pid);
        } else {
            failed = true;
            eprintln!(
                "failed to signal {} pid {}: {}",
                target.name,
                target.pid,
                std::io::Error::last_os_error()
            );
        }
    }

    if failed {
        Err("one or more dump signals failed".into())
    } else {
        Ok(())
    }
}

struct DumpTarget {
    name: String,
    pid: u32,
}

fn pids_by_exact_process_name(name: &str) -> Result<Vec<u32>, Box<dyn Error>> {
    let output = std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()?;
    if output.status.success() {
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                line.parse::<u32>()
                    .map_err(|e| format!("pgrep returned invalid pid {line:?}: {e}").into())
            })
            .collect();
    }
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    Err(format!(
        "pgrep -x {name} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

async fn run_record_async(args: RecordArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("record", &client);
    let target = args.target()?;
    let label = args
        .command
        .first()
        .cloned()
        .unwrap_or_else(|| "(attached)".to_owned());
    let config = stax_live_proto::RunConfig {
        label,
        frequency_hz: args.frequency,
        correlate_frequency_hz: args.correlate_frequency.unwrap_or(args.frequency),
        correlate_kperf: args.correlate_kperf,
    };

    let mut terminal_relay = None;
    let run_id = match target {
        stax_core::args::TargetProcess::ByPid(pid) => client
            .start_attach(pid, config, args.daemon_socket.clone(), args.time_limit)
            .await
            .map_err(|e| format!("{e:?}"))?,
        stax_core::args::TargetProcess::Launch {
            program,
            args: rest,
        } => {
            let mut command = Vec::with_capacity(1 + rest.len());
            command.push(program);
            command.extend(rest);
            let (terminal_input_tx, terminal_input_rx) = vox::channel::<TerminalInput>();
            let (terminal_output_tx, terminal_output_rx) = vox::channel::<TerminalOutput>();
            let terminal_size = current_terminal_size_or_default();
            let request = LaunchRequest {
                command,
                cwd: env::current_dir()?.to_string_lossy().into_owned(),
                env: env::vars_os()
                    .filter_map(|(key, value)| {
                        Some(LaunchEnvVar {
                            key: key.into_string().ok()?,
                            value: value.into_string().ok()?,
                        })
                    })
                    .collect(),
                config,
                daemon_socket: args.daemon_socket.clone(),
                time_limit_secs: args.time_limit,
                terminal_size: Some(terminal_size),
            };
            let run_id = client
                .start_launch(request, terminal_input_rx, terminal_output_tx)
                .await
                .map_err(|e| format!("{e:?}"))?;
            terminal_relay = Some(TerminalRelay::start(
                terminal_input_tx,
                terminal_output_rx,
                Some(terminal_size),
            ));
            run_id
        }
    };
    eprintln!("stax: started run {}", run_id.0);

    let wait_client = client.clone();
    tokio::select! {
        outcome = wait_client.wait_active(WaitCondition::UntilStopped, None) => {
            match outcome.map_err(|e| format!("{e:?}"))? {
                WaitOutcome::Stopped { summary } => {
                    drop(terminal_relay.take());
                    println!("stopped:");
                    print_run_one_line(&summary);
                    fail_on_recorder_error(&summary)?;
                }
                WaitOutcome::NoActiveRun => {
                    drop(terminal_relay.take());
                    print_finished_run_or_message(&client, run_id).await?;
                }
                other => {
                    eprintln!("stax: unexpected wait outcome: {other:?}");
                }
            }
        }
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|e| format!("waiting for Ctrl-C: {e}"))?;
            let summary = client.stop_active().await.map_err(|e| format!("{e:?}"))?;
            drop(terminal_relay.take());
            println!("stopped:");
            print_run_one_line(&summary);
        }
    }
    drop(terminal_relay);
    Ok(())
}

async fn print_finished_run_or_message(
    client: &RunControlClient,
    run_id: stax_live_proto::RunId,
) -> Result<(), Box<dyn Error>> {
    let runs = client.list_runs().await.map_err(|e| format!("{e:?}"))?;
    let Some(summary) = runs.into_iter().find(|run| run.id == run_id) else {
        eprintln!("stax: run ended before wait attached");
        return Ok(());
    };
    println!("stopped:");
    print_run_one_line(&summary);
    fail_on_recorder_error(&summary)?;
    Ok(())
}

fn fail_on_recorder_error(summary: &RunSummary) -> Result<(), Box<dyn Error>> {
    if let Some(StopReason::RecorderError { message }) = &summary.stop_reason {
        return Err(format!("recorder failed: {message}").into());
    }
    Ok(())
}

struct TerminalRelay {
    _raw_mode: Option<RawMode>,
}

impl TerminalRelay {
    fn start(
        terminal_input: vox::Tx<TerminalInput>,
        mut terminal_output: vox::Rx<TerminalOutput>,
        initial_size: Option<TerminalSize>,
    ) -> Self {
        let raw_mode = RawMode::enable().ok().flatten();
        let (input_events_tx, mut input_events_rx) =
            tokio::sync::mpsc::unbounded_channel::<TerminalInput>();

        if let Some(size) = initial_size {
            let _ = input_events_tx.send(TerminalInput::Resize { size });
        }

        let stdin_events = input_events_tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => {
                        let _ = stdin_events.send(TerminalInput::Close);
                        break;
                    }
                    Ok(n) => {
                        if stdin_events
                            .send(TerminalInput::Bytes {
                                data: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        let _ = stdin_events.send(TerminalInput::Close);
                        break;
                    }
                }
            }
        });

        #[cfg(unix)]
        {
            let resize_events = input_events_tx.clone();
            tokio::spawn(async move {
                if let Ok(mut sigwinch) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                {
                    while sigwinch.recv().await.is_some() {
                        if let Some(size) = current_terminal_size() {
                            let _ = resize_events.send(TerminalInput::Resize { size });
                        }
                    }
                }
            });
        }

        tokio::spawn(async move {
            while let Some(event) = input_events_rx.recv().await {
                if terminal_input.send(event).await.is_err() {
                    break;
                }
            }
            let _ = terminal_input.close(Default::default()).await;
        });

        tokio::spawn(async move {
            let mut stdout = std::io::stdout();
            loop {
                match terminal_output.recv().await {
                    Ok(Some(output_sref)) => {
                        let mut output = None;
                        let _ = output_sref.map(|value| {
                            output = Some(value);
                        });
                        match output.expect("output set") {
                            TerminalOutput::Bytes { data } => {
                                let _ = stdout.write_all(&data);
                                let _ = stdout.flush();
                            }
                            TerminalOutput::ExitStatus { .. } => {}
                            TerminalOutput::Error { message } => {
                                eprintln!("stax terminal: {message}");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("stax terminal recv failed: {e:?}");
                        break;
                    }
                }
            }
        });

        Self {
            _raw_mode: raw_mode,
        }
    }
}

struct RawMode {
    fd: libc::c_int,
    original: libc::termios,
}

impl RawMode {
    fn enable() -> std::io::Result<Option<Self>> {
        let fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(fd) } == 0 {
            return Ok(None);
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
            // Keep Ctrl-C/Ctrl-\ signal generation enabled so the
            // CLI can still be interrupted while in terminal relay mode.
            raw.c_lflag |= libc::ISIG;
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(Self { fd, original }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

fn current_terminal_size() -> Option<TerminalSize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    let r = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if r != 0 {
        return None;
    }
    let size = unsafe { size.assume_init() };
    if size.ws_row == 0 || size.ws_col == 0 {
        return None;
    }
    Some(TerminalSize {
        rows: size.ws_row,
        cols: size.ws_col,
    })
}

fn current_terminal_size_or_default() -> TerminalSize {
    current_terminal_size().unwrap_or(TerminalSize { rows: 24, cols: 80 })
}

fn require_server_socket() -> Result<String, Box<dyn Error>> {
    let socket = stax_server_socket().ok_or_else(|| {
        format!(
            "stax-server isn't running. \
             Start it with `stax-server` (or set STAX_SERVER_SOCKET if you've moved the socket)."
        )
    })?;
    Ok(format!("local://{}", socket.display()))
}

fn register_run_control_client(
    surface: &'static str,
    client: &RunControlClient,
) -> stax_vox_observe::VoxDebugRegistration {
    stax_vox_observe::register_global_caller("stax", surface, "RunControl", &client.caller)
}

fn register_profiler_client(
    surface: &'static str,
    client: &ProfilerClient,
) -> stax_vox_observe::VoxDebugRegistration {
    stax_vox_observe::register_global_caller("stax", surface, "Profiler", &client.caller)
}

async fn run_status() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("status", &client);
    let status = client.status().await.map_err(|e| format!("{e:?}"))?;
    print_server_status(&status);
    Ok(())
}

async fn run_list() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("list", &client);
    let runs = client.list_runs().await.map_err(|e| format!("{e:?}"))?;
    if runs.is_empty() {
        println!("(no runs)");
    } else {
        for run in runs {
            print_run_one_line(&run);
        }
    }
    Ok(())
}

async fn run_diagnose() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("diagnose", &client);
    let snapshot = client.diagnostics().await.map_err(|e| format!("{e:?}"))?;
    print_diagnostics(&snapshot);
    Ok(())
}

async fn run_wait(args: WaitArgs) -> Result<(), Box<dyn Error>> {
    let condition = match (args.for_samples, args.for_seconds, args.until_symbol) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            return Err(
                "--for-samples, --for-seconds, --until-symbol are mutually exclusive".into(),
            );
        }
        (Some(count), _, _) => WaitCondition::ForSamples { count },
        (_, Some(seconds), _) => WaitCondition::ForSeconds { seconds },
        (_, _, Some(needle)) => WaitCondition::UntilSymbolSeen { needle },
        _ => WaitCondition::UntilStopped,
    };

    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("wait", &client);
    let outcome = client
        .wait_active(condition, args.timeout_ms)
        .await
        .map_err(|e| format!("{e:?}"))?;
    match outcome {
        WaitOutcome::ConditionMet { summary } => {
            println!("condition met:");
            print_run_one_line(&summary);
        }
        WaitOutcome::Stopped { summary } => {
            println!("run stopped:");
            print_run_one_line(&summary);
        }
        WaitOutcome::TimedOut { summary } => {
            println!("timed out:");
            print_run_one_line(&summary);
            return Err("timed out waiting".into());
        }
        WaitOutcome::NoActiveRun => {
            return Err("no active run".into());
        }
    }
    Ok(())
}

async fn run_stop() -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: RunControlClient = vox::connect(&url).await?;
    let _debug_registration = register_run_control_client("stop", &client);
    let result = client.stop_active().await;
    match result {
        Ok(summary) => {
            println!("stopped:");
            print_run_one_line(&summary);
        }
        Err(vox::VoxError::User(err)) => return Err(format!("{err:?}").into()),
        Err(e) => return Err(format!("{e:?}").into()),
    }
    Ok(())
}

async fn run_top(args: TopArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let sort = match args.sort.as_str() {
        "self" => TopSort::BySelf,
        "total" => TopSort::ByTotal,
        other => {
            return Err(format!("unknown --sort value {other:?} (use `self` or `total`)").into());
        }
    };
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("top", &client);
    let entries = client
        .top(
            args.limit,
            sort,
            ViewParams {
                tid: args.tid,
                filter: LiveFilter {
                    time_range: None,
                    exclude_symbols: Vec::new(),
                },
            },
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    if entries.is_empty() {
        println!("(no samples yet — is a recording in progress?)");
        return Ok(());
    }
    for e in entries {
        let name = e.function_name.as_deref().unwrap_or("<unresolved>");
        let bin = e.binary.as_deref().unwrap_or("?");
        println!(
            "{:>10.3}ms  {:>8} samples  {} ({})",
            e.self_on_cpu_ns as f64 / 1e6,
            e.self_pet_samples,
            name,
            bin,
        );
    }
    Ok(())
}

async fn run_annotate(args: AnnotateArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("annotate", &client);
    let view_params = ViewParams {
        tid: args.tid,
        filter: LiveFilter {
            time_range: None,
            exclude_symbols: Vec::new(),
        },
    };
    let address = resolve_target(&client, &args.target, view_params.clone()).await?;
    let view = client
        .annotated(address, view_params)
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!(
        "; {} ({}) @ {:#x}",
        view.function_name, view.language, view.base_address
    );
    for line in view.lines {
        if let Some(hdr) = &line.source_header
            && !hdr.file.is_empty()
        {
            println!("; {}:{}", hdr.file, hdr.line);
        }
        // Token classes don't carry colour info on the terminal path;
        // just concatenate the text runs for a plain-text view.
        let plain: String = line.tokens.iter().map(|t| t.text.as_str()).collect();
        println!(
            "  {:#x}  {:>5} samples  {}",
            line.address, line.self_pet_samples, plain
        );
    }
    Ok(())
}

async fn run_threads(args: ThreadsArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("threads", &client);
    let update = client.threads().await.map_err(|e| format!("{e:?}"))?;
    print_threads(&update, args.limit);
    Ok(())
}

fn print_threads(update: &ThreadsUpdate, limit: u32) {
    let mut threads: Vec<&stax_live_proto::ThreadInfo> = update.threads.iter().collect();
    threads.sort_by(|a, b| b.on_cpu_ns.cmp(&a.on_cpu_ns));
    if threads.is_empty() {
        println!("(no thread samples yet — is a recording in progress?)");
        return;
    }
    println!(
        "{:>10} {:>10} {:>10} {:>9}  tid    name",
        "on-CPU ms", "off-CPU ms", "samples", "blocked",
    );
    let take = if limit == 0 {
        threads.len()
    } else {
        limit as usize
    };
    for t in threads.iter().take(take) {
        let off_total = off_cpu_total_ns(&t.off_cpu);
        let dominant = dominant_off_cpu_reason(&t.off_cpu);
        println!(
            "{:>10.2} {:>10.2} {:>10} {:>9}  {:<6} {}",
            t.on_cpu_ns as f64 / 1e6,
            off_total as f64 / 1e6,
            t.pet_samples,
            dominant,
            t.tid,
            t.name.as_deref().unwrap_or("(unnamed)"),
        );
    }
    if threads.len() > take {
        println!("…{} more threads", threads.len() - take);
    }
}

async fn run_probe_diff(args: ProbeDiffArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("probe-diff", &client);
    let update = client
        .probe_diff(args.tid)
        .await
        .map_err(|e| format!("{e:?}"))?;
    print_probe_diff(&update, args.recent, args.verbose);
    Ok(())
}

fn print_probe_diff(update: &ProbeDiffUpdate, recent_limit: u32, verbose: bool) {
    println!(
        "kperf samples         : {}\nprobe results         : {}\npaired                : {}",
        update.total_kperf_samples, update.total_probes, update.paired,
    );
    println!(
        "kperf-only (unpaired) : {}\nprobe-only (unpaired) : {}",
        update.kperf_only, update.probe_only,
    );
    let total_kperf = update.total_kperf_samples.max(1) as f64;
    let paired_for_kernel = update.paired.max(1) as f64;
    println!(
        "kperf kernel stacks   : {:>6} / {:<6} ({:>5.1}%)  paired {:>6} ({:>5.1}%)  frames={} max_depth={}",
        update.kperf_kernel_stack_samples,
        update.total_kperf_samples,
        update.kperf_kernel_stack_samples as f64 * 100.0 / total_kperf,
        update.paired_kernel_stack_samples,
        update.paired_kernel_stack_samples as f64 * 100.0 / paired_for_kernel,
        update.kperf_kernel_frames,
        update.max_kperf_kernel_frames,
    );
    if update.paired == 0 {
        println!("(no paired samples yet — no correlated shade probe results have landed)");
        return;
    }
    let paired_total = update.paired as f64;
    let pct = |n: u64| n as f64 * 100.0 / paired_total;
    let fp_run_stitchable: u64 = update
        .common_suffix_hist
        .iter()
        .enumerate()
        .filter(|(k, _)| *k >= stax_live_proto::STITCH_MIN_SUFFIX as usize)
        .map(|(_, &n)| n)
        .sum();
    println!(
        "pc_match              : {:>6}  ({:>5.1}%)",
        update.pc_match,
        pct(update.pc_match)
    );
    println!(
        "fp validator run (>= {})    : {:>6}  ({:>5.1}%)",
        stax_live_proto::STITCH_MIN_SUFFIX,
        fp_run_stitchable,
        pct(fp_run_stitchable)
    );
    println!(
        "would ship enriched stack   : {:>6}  ({:>5.1}%)",
        update.stitchable,
        pct(update.stitchable)
    );
    println!(
        "richer than kperf           : {:>6}  ({:>5.1}%)",
        update.richer_than_kperf,
        pct(update.richer_than_kperf)
    );
    println!(
        "dwarf richer than fp        : {:>6}  ({:>5.1}%)",
        update.dwarf_richer_than_fp,
        pct(update.dwarf_richer_than_fp)
    );
    if verbose {
        println!(
            "compact run stitchable      : {:>6}  ({:>5.1}%)",
            update.compact_stitchable,
            pct(update.compact_stitchable)
        );
        println!(
            "compact+fde run stitchable  : {:>6}  ({:>5.1}%)",
            update.compact_dwarf_stitchable,
            pct(update.compact_dwarf_stitchable)
        );
        println!(
            "dwarf run stitchable        : {:>6}  ({:>5.1}%)",
            update.dwarf_stitchable,
            pct(update.dwarf_stitchable)
        );
    }
    println!(
        "probe augments kperf  : {:>6}  ({:>5.1}%)  (kperf walked 0, probe ≥1)",
        update.probe_augmented_kperf,
        pct(update.probe_augmented_kperf),
    );
    println!(
        "probe walked deeper   : {:>6}  ({:>5.1}%)",
        update.probe_walked_deeper,
        pct(update.probe_walked_deeper),
    );
    if verbose {
        println!(
            "compact / c+fde / dwarf / fp validator: {} / {} / {} / {}",
            update.compact_used,
            update.compact_dwarf_used,
            update.framehop_used,
            update.fp_walk_used,
        );
    }

    if verbose {
        println!("\nfp common_run histogram:");
        print_run_hist(&update.common_suffix_hist, paired_total);
        println!("\ncompact common_run histogram:");
        print_run_hist(&update.compact_suffix_hist, paired_total);
        println!("\ncompact+fde common_run histogram:");
        print_run_hist(&update.compact_dwarf_suffix_hist, paired_total);
        println!("\ndwarf common_run histogram:");
        print_run_hist(&update.dwarf_suffix_hist, paired_total);

        println!("\nmatch rate by frame depth (0 = leaf, fp validator):");
        for cell in &update.depth_match {
            let rate = if cell.total == 0 {
                0.0
            } else {
                cell.matched as f64 * 100.0 / cell.total as f64
            };
            let bar = bar_str(rate, 24);
            println!(
                "  d={:<2} {:>6}/{:<6}  {:>5.1}%  {bar}",
                cell.depth, cell.matched, cell.total, rate
            );
        }

        println!("\ndrift histogram (kperf_ts → probe_done, with pc_match rate):");
        let mut prev = 0u64;
        for b in &update.drift_buckets {
            let label = if b.upper_ns == u64::MAX {
                format!(">= {}", fmt_ns(prev))
            } else {
                format!("{}–{}", fmt_ns(prev), fmt_ns(b.upper_ns))
            };
            let rate = if b.samples == 0 {
                0.0
            } else {
                b.pc_match as f64 * 100.0 / b.samples as f64
            };
            println!(
                "  {label:<22} {:>6} samples   pc_match {:>3.0}%",
                b.samples, rate
            );
            prev = b.upper_ns;
        }
    }

    let t = &update.timing;
    if verbose && t.samples > 0 {
        println!("\nprobe timing breakdown (avg / max, causal path):");
        println!(
            "  kdebug pre-read    {:>9} / {:>9}  (kperf_ts → KDREADTR start)",
            fmt_ns(t.avg_kperf_to_staxd_read_ns),
            fmt_ns(t.max_kperf_to_staxd_read_ns)
        );
        println!(
            "  kdebug to staxd    {:>9} / {:>9}  (kperf_ts → KDREADTR done)",
            fmt_ns(t.avg_kperf_to_staxd_drain_ns),
            fmt_ns(t.max_kperf_to_staxd_drain_ns)
        );
        println!(
            "  staxd read         {:>9} / {:>9}",
            fmt_ns(t.avg_staxd_read_ns),
            fmt_ns(t.max_staxd_read_ns)
        );
        println!(
            "  staxd drain→queue  {:>9} / {:>9}",
            fmt_ns(t.avg_staxd_drain_to_queue_ns),
            fmt_ns(t.max_staxd_drain_to_queue_ns)
        );
        println!(
            "  staxd queue wait   {:>9} / {:>9}",
            fmt_ns(t.avg_staxd_queue_wait_ns),
            fmt_ns(t.max_staxd_queue_wait_ns)
        );
        println!(
            "  staxd→client recv  {:>9} / {:>9}",
            fmt_ns(t.avg_staxd_send_to_client_recv_ns),
            fmt_ns(t.max_staxd_send_to_client_recv_ns)
        );
        println!(
            "  client recv→enqueue {:>8} / {:>9}",
            fmt_ns(t.avg_client_recv_to_enqueue_ns),
            fmt_ns(t.max_client_recv_to_enqueue_ns)
        );
        println!(
            "  end-to-end enqueue {:>9} / {:>9}  (kperf_ts → enqueue)",
            fmt_ns(t.avg_kperf_to_enqueue_ns),
            fmt_ns(t.max_kperf_to_enqueue_ns)
        );
        println!("  worker path:");
        println!(
            "  queue wait         {:>9} / {:>9}",
            fmt_ns(t.avg_queue_wait_ns),
            fmt_ns(t.max_queue_wait_ns)
        );
        println!(
            "  thread lookup      {:>9} / {:>9}",
            fmt_ns(t.avg_lookup_ns),
            fmt_ns(t.max_lookup_ns)
        );
        println!(
            "  suspend+state      {:>9} / {:>9}",
            fmt_ns(t.avg_suspend_state_ns),
            fmt_ns(t.max_suspend_state_ns)
        );
        println!(
            "  resume             {:>9} / {:>9}",
            fmt_ns(t.avg_resume_ns),
            fmt_ns(t.max_resume_ns)
        );
        println!(
            "  unwind             {:>9} / {:>9}",
            fmt_ns(t.avg_walk_ns),
            fmt_ns(t.max_walk_ns)
        );
        println!(
            "  worker total       {:>9} / {:>9}",
            fmt_ns(t.avg_probe_total_ns),
            fmt_ns(t.max_probe_total_ns)
        );
        println!(
            "  coalesced stale requests: {} · max worker batch: {}",
            t.coalesced_requests, t.max_worker_batch_len
        );
    }

    if !update.threads.is_empty() {
        println!("\nthreads by kperf sample count:");
        if verbose {
            println!(
                "  {:>10}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>11}  {:>9}  {:>9}  {:>9}  {:>9}  {:>9}  name",
                "tid",
                "kperf",
                "kstack",
                "probe",
                "paired",
                "k-only",
                "p-only",
                "pc_match",
                "fp-suff",
                "c-run",
                "c+fde",
                "dw-run",
                "dw>fp",
            );
        } else {
            println!(
                "  {:>10}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>11}  {:>9}  {:>9}  {:>9}  name",
                "tid",
                "kperf",
                "kstack",
                "probe",
                "paired",
                "k-only",
                "p-only",
                "pc_match",
                "fp-run",
                "enriched",
                "dw>fp",
            );
        }
        for t in &update.threads {
            let name = t.thread_name.as_deref().unwrap_or("(unnamed)");
            let denom = t.paired.max(1) as f64;
            if verbose {
                println!(
                    "  {:>10}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>5} {:>3.0}%  {:>9.2}  {:>9.2}  {:>9.2}  {:>9.2}  {:>9}  {name}",
                    t.tid,
                    t.kperf_samples,
                    t.kperf_kernel_stack_samples,
                    t.probe_results,
                    t.paired,
                    t.kperf_only,
                    t.probe_only,
                    t.pc_match,
                    t.pc_match as f64 * 100.0 / denom,
                    t.avg_common_suffix,
                    t.avg_compact_common_suffix,
                    t.avg_compact_dwarf_common_suffix,
                    t.avg_dwarf_common_suffix,
                    t.dwarf_richer_than_fp,
                );
            } else {
                println!(
                    "  {:>10}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>5} {:>3.0}%  {:>9.2}  {:>9}  {:>9}  {name}",
                    t.tid,
                    t.kperf_samples,
                    t.kperf_kernel_stack_samples,
                    t.probe_results,
                    t.paired,
                    t.kperf_only,
                    t.probe_only,
                    t.pc_match,
                    t.pc_match as f64 * 100.0 / denom,
                    t.avg_common_suffix,
                    t.stitchable,
                    t.dwarf_richer_than_fp,
                );
            }
        }
    }

    if recent_limit == 0 {
        return;
    }
    let take = recent_limit as usize;
    if verbose {
        let entries = update
            .recent
            .iter()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        println!(
            "\nmost recent {} paired entries (oldest → newest):",
            entries.len()
        );
        for entry in entries {
            print_probe_diff_entry(entry, verbose, StitchedTagBaseline::Kperf);
        }
    } else {
        let kernel_entries = update
            .richer
            .iter()
            .filter(|entry| !entry.kperf_kernel_stack.is_empty())
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        println!(
            "\nkernel-bearing stitched entries from full scan: {} shown",
            kernel_entries.len()
        );
        if kernel_entries.is_empty() {
            println!("  (no kernel-bearing stitched entries found in the full scan)");
        }
        for entry in kernel_entries {
            print_probe_diff_entry(entry, verbose, StitchedTagBaseline::Kperf);
        }

        let dwarf_entries = update
            .dwarf_richer
            .iter()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        println!(
            "\ndwarf-richer stitched entries from full scan: {} shown (tags compare against FP; use -n to show more, --verbose for comparator stacks)",
            dwarf_entries.len()
        );
        if dwarf_entries.is_empty() {
            println!("  (no DWARF-richer stitched entries found in the full scan)");
        }
        for entry in dwarf_entries {
            print_probe_diff_entry(entry, verbose, StitchedTagBaseline::Fp);
        }
    }
}

fn print_probe_diff_entry(entry: &ProbeDiffEntry, verbose: bool, baseline: StitchedTagBaseline) {
    let user_delta_vs_kperf = signed_len_delta(entry.dwarf_stack.len(), entry.kperf_stack.len());
    let user_delta_vs_fp = signed_len_delta(entry.dwarf_stack.len(), entry.probe_stack.len());
    let stitched_delta_vs_kperf_total = signed_len_delta(
        entry.stitched_stack.len(),
        entry.kperf_kernel_stack.len() + entry.kperf_stack.len(),
    );
    let dwarf_only_pcs = distinct_dwarf_only_pc_count(&entry.kperf_stack, &entry.dwarf_stack);
    let dwarf_only_vs_fp_pcs = distinct_dwarf_only_pc_count(&entry.probe_stack, &entry.dwarf_stack);
    println!(
        "\n  tid={} t={}ms drift={:+}ns fp_run={} compact_run={} compact+fde_run={} dwarf_run={} pc_match={} stitchable={} dwarf={} kperf_user={} kperf_kernel={} kperf_total={} fp_user={} dwarf_user={} stitched_total={} user_delta_vs_kperf={:+} user_delta_vs_fp={:+} stitched_delta_vs_kperf_total={:+} dwarf_only_vs_kperf_pcs={} dwarf_only_vs_fp_pcs={}",
        entry.tid,
        entry.timestamp_ns / 1_000_000,
        entry.drift_ns,
        entry.common_suffix,
        entry.compact_common_suffix,
        entry.compact_dwarf_common_suffix,
        entry.dwarf_common_suffix,
        entry.pc_match,
        entry.stitchable,
        entry.used_framehop,
        entry.kperf_stack.len(),
        entry.kperf_kernel_stack.len(),
        entry.kperf_kernel_stack.len() + entry.kperf_stack.len(),
        entry.probe_stack.len(),
        entry.dwarf_stack.len(),
        entry.stitched_stack.len(),
        user_delta_vs_kperf,
        user_delta_vs_fp,
        stitched_delta_vs_kperf_total,
        dwarf_only_pcs,
        dwarf_only_vs_fp_pcs,
    );
    if verbose {
        println!(
            "    ticks: kperf={} staxd_read={} staxd_drained={} staxd_queued={} staxd_send={} client_recv={} enqueue={} worker={} lookup_done={} state_done={} resume_done={} walk_done={}",
            entry.timing.kperf_ts_ticks,
            entry.timing.staxd_read_started_ticks,
            entry.timing.staxd_drained_ticks,
            entry.timing.staxd_queued_for_send_ticks,
            entry.timing.staxd_send_started_ticks,
            entry.timing.client_received_ticks,
            entry.timing.enqueued_ticks,
            entry.timing.worker_started_ticks,
            entry.timing.thread_lookup_done_ticks,
            entry.timing.state_done_ticks,
            entry.timing.resume_done_ticks,
            entry.timing.walk_done_ticks,
        );
        println!(
            "    path: kdebug_pre_read={} kdebug_to_staxd={} staxd_read={} drain→queue={} staxd_queue={} staxd→client={} client→enqueue={} end_to_end_enqueue={} queue={} lookup={} suspend+state={} resume={} walk={} worker_total={} kperf→state={} coalesced={} batch={}",
            fmt_ns(entry.timing.kperf_to_staxd_read_ns),
            fmt_ns(entry.timing.kperf_to_staxd_drain_ns),
            fmt_ns(entry.timing.staxd_read_ns),
            fmt_ns(entry.timing.staxd_drain_to_queue_ns),
            fmt_ns(entry.timing.staxd_queue_wait_ns),
            fmt_ns(entry.timing.staxd_send_to_client_recv_ns),
            fmt_ns(entry.timing.client_recv_to_enqueue_ns),
            fmt_ns(entry.timing.kperf_to_enqueue_ns),
            fmt_ns(entry.timing.queue_wait_ns),
            fmt_ns(entry.timing.lookup_ns),
            fmt_ns(entry.timing.suspend_state_ns),
            fmt_ns(entry.timing.resume_ns),
            fmt_ns(entry.timing.walk_ns),
            fmt_ns(entry.timing.probe_total_ns),
            fmt_ns(entry.drift_ns.unsigned_abs()),
            entry.queue.coalesced_requests,
            entry.queue.worker_batch_len,
        );
        print_stack("kperf_stack", &entry.kperf_stack);
        print_stack("fp_stack", &entry.probe_stack);
        if !entry.compact_stack.is_empty() {
            print_stack("compact_stack", &entry.compact_stack);
        }
        if !entry.compact_dwarf_stack.is_empty() {
            print_stack("compact+fde_stack", &entry.compact_dwarf_stack);
        }
        if !entry.dwarf_stack.is_empty() {
            print_stack("dwarf_stack", &entry.dwarf_stack);
        }
    }
    if !entry.kperf_kernel_stack.is_empty() {
        print_stack_with_tag(
            "kperf_kernel_stack (new)",
            &entry.kperf_kernel_stack,
            FrameTag::Kernel,
        );
    }
    if !entry.stitched_stack.is_empty() {
        print_stitched_stack(entry, baseline);
    }
}

fn print_run_hist(hist: &[u64], paired_total: f64) {
    for (k, &n) in hist.iter().enumerate() {
        if n > 0 {
            println!(
                "  k={k:<3} {n:>6}  ({:>5.1}%)",
                n as f64 * 100.0 / paired_total
            );
        }
    }
}

fn print_stack(label: &str, frames: &[stax_live_proto::ResolvedFrame]) {
    println!("    {label}:");
    print_frame_groups(frames, |_| FrameTag::Plain);
}

fn print_stack_with_tag(label: &str, frames: &[stax_live_proto::ResolvedFrame], tag: FrameTag) {
    println!("    {label}:");
    print_frame_groups(frames, |_| tag);
}

#[derive(Clone, Copy)]
enum StitchedTagBaseline {
    Kperf,
    Fp,
}

fn print_stitched_stack(entry: &ProbeDiffEntry, baseline: StitchedTagBaseline) {
    let baseline_label = match baseline {
        StitchedTagBaseline::Kperf => "kperf",
        StitchedTagBaseline::Fp => "fp",
    };
    println!("    stitched_stack (would-ship, tags vs {baseline_label}):");
    let baseline_user_addrs = match baseline {
        StitchedTagBaseline::Kperf => frame_address_set(&entry.kperf_stack),
        StitchedTagBaseline::Fp => frame_address_set(&entry.probe_stack),
    };
    print_frame_groups(&entry.stitched_stack, |frame| {
        if is_kernel_frame(frame) {
            FrameTag::Kernel
        } else if frame.address != 0 && baseline_user_addrs.contains(&frame.address) {
            FrameTag::User
        } else {
            FrameTag::Dwarf
        }
    });
}

#[derive(Clone, Copy)]
enum FrameTag {
    Plain,
    Kernel,
    User,
    Dwarf,
}

impl FrameTag {
    fn prefix(self) -> &'static str {
        match self {
            Self::Plain => "",
            Self::Kernel => "[K] ",
            Self::User => "[U] ",
            Self::Dwarf => "[D] ",
        }
    }

    fn color(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Kernel => Some("\x1b[38;5;130m"),
            Self::User => Some("\x1b[33m"),
            Self::Dwarf => Some("\x1b[32m"),
        }
    }
}

fn print_frame_groups<F>(frames: &[stax_live_proto::ResolvedFrame], mut tag: F)
where
    F: FnMut(&stax_live_proto::ResolvedFrame) -> FrameTag,
{
    let mut idx = 0usize;
    while idx < frames.len() {
        let frame = &frames[idx];
        let mut repeat = 1usize;
        while idx + repeat < frames.len() && frames[idx + repeat].address == frame.address {
            repeat += 1;
        }
        print_frame(frame, tag(frame), repeat);
        idx += repeat;
    }
}

fn print_frame(f: &stax_live_proto::ResolvedFrame, tag: FrameTag, repeat: usize) {
    let repeat_suffix = if repeat > 1 {
        format!("  ×{repeat}")
    } else {
        String::new()
    };
    let prefix = tag.prefix();
    if colors_enabled()
        && let Some(color) = tag.color()
    {
        println!(
            "      {color}{prefix}{:#018x}  {}{}\x1b[0m",
            f.address, f.display, repeat_suffix
        );
    } else {
        println!(
            "      {prefix}{:#018x}  {}{}",
            f.address, f.display, repeat_suffix
        );
    }
}

fn is_kernel_frame(frame: &stax_live_proto::ResolvedFrame) -> bool {
    frame.binary.starts_with("kernel:") || frame.address >= 0xffff_0000_0000_0000
}

fn signed_len_delta(after: usize, before: usize) -> isize {
    after as isize - before as isize
}

fn distinct_dwarf_only_pc_count(
    baseline: &[stax_live_proto::ResolvedFrame],
    enriched: &[stax_live_proto::ResolvedFrame],
) -> usize {
    let baseline_addrs = frame_address_set(baseline);
    enriched
        .iter()
        .filter(|frame| frame.address != 0 && !baseline_addrs.contains(&frame.address))
        .map(|frame| frame.address)
        .collect::<HashSet<_>>()
        .len()
}

fn frame_address_set(frames: &[stax_live_proto::ResolvedFrame]) -> HashSet<u64> {
    frames
        .iter()
        .map(|frame| frame.address)
        .filter(|&address| address != 0)
        .collect()
}

fn colors_enabled() -> bool {
    env::var_os("NO_COLOR").is_none()
}
fn bar_str(pct: f64, width: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = (pct / 100.0 * width as f64).round() as usize;
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < filled { '█' } else { '·' });
    }
    s
}

fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{}s", ns / 1_000_000_000)
    } else if ns >= 1_000_000 {
        format!("{}ms", ns / 1_000_000)
    } else if ns >= 1_000 {
        format!("{}µs", ns / 1_000)
    } else {
        format!("{ns}")
    }
}

/// Pick the largest field of the off-CPU breakdown so the user can
/// see at a glance whether a thread was idle vs. blocked vs. doing
/// IO. Returns the bucket name padded to a stable width.
fn dominant_off_cpu_reason(b: &OffCpuBreakdown) -> &'static str {
    let buckets: [(u64, &str); 10] = [
        (b.idle_ns, "idle"),
        (b.lock_ns, "lock"),
        (b.semaphore_ns, "sem"),
        (b.ipc_ns, "ipc"),
        (b.io_read_ns, "ioR"),
        (b.io_write_ns, "ioW"),
        (b.readiness_ns, "ready"),
        (b.sleep_ns, "sleep"),
        (b.connect_ns, "conn"),
        (b.other_ns, "other"),
    ];
    let mut best = ("-", 0u64);
    for (ns, name) in buckets {
        if ns > best.1 {
            best = (name, ns);
        }
    }
    best.0
}

async fn run_flame(args: FlameArgs) -> Result<(), Box<dyn Error>> {
    let url = require_server_socket()?;
    let client: ProfilerClient = vox::connect(&url).await?;
    let _debug_registration = register_profiler_client("flame", &client);
    let update = client
        .flamegraph(ViewParams {
            tid: args.tid,
            filter: LiveFilter {
                time_range: None,
                exclude_symbols: Vec::new(),
            },
        })
        .await
        .map_err(|e| format!("{e:?}"))?;
    print_flame(&update, args.max_depth, args.threshold_pct);
    Ok(())
}

fn print_flame(update: &FlamegraphUpdate, max_depth: usize, threshold_pct: f64) {
    let total = update.total_on_cpu_ns.max(1) as f64;
    println!(
        "# stax flame · total on-CPU {:.3}s · off-CPU {:.3}s",
        update.total_on_cpu_ns as f64 / 1e9,
        off_cpu_total_ns(&update.total_off_cpu) as f64 / 1e9,
    );
    if let Some(tid) = update.root.children.first().map(|_| None::<u32>).flatten() {
        // placeholder — root has no tid annotation; left as a hook
        // for future per-thread renders.
        let _ = tid;
    }
    println!();
    println!("```");
    print_flame_node(
        &update.root,
        &update.strings,
        total,
        threshold_pct,
        0,
        max_depth,
    );
    println!("```");
}

fn off_cpu_total_ns(b: &OffCpuBreakdown) -> u64 {
    b.idle_ns
        + b.lock_ns
        + b.semaphore_ns
        + b.ipc_ns
        + b.io_read_ns
        + b.io_write_ns
        + b.readiness_ns
        + b.sleep_ns
        + b.connect_ns
        + b.other_ns
}

fn print_flame_node(
    node: &FlameNode,
    strings: &[String],
    total_ns: f64,
    threshold_pct: f64,
    depth: usize,
    max_depth: usize,
) {
    let pct = node.on_cpu_ns as f64 / total_ns * 100.0;
    if depth > 0 && pct < threshold_pct {
        return;
    }

    let label = if depth == 0 {
        "(root)".to_owned()
    } else {
        let name = node
            .function_name
            .and_then(|i| strings.get(i as usize).map(String::as_str))
            .unwrap_or("<unresolved>");
        let bin = node
            .binary
            .and_then(|i| strings.get(i as usize).map(String::as_str))
            .unwrap_or("?");
        format!("{name}  ({bin})")
    };
    let indent = "  ".repeat(depth);
    println!(
        "{:>8.2}ms {:>5.1}%  {indent}{prefix}{label}",
        node.on_cpu_ns as f64 / 1e6,
        pct,
        indent = indent,
        prefix = if depth == 0 { "" } else { "└─ " },
        label = label,
    );

    if depth + 1 > max_depth {
        if !node.children.is_empty() {
            let truncated = node.children.len();
            println!(
                "{indent}   …{truncated} more frame{plural}",
                indent = "  ".repeat(depth + 1),
                truncated = truncated,
                plural = if truncated == 1 { "" } else { "s" }
            );
        }
        return;
    }

    // Sort children by on_cpu_ns descending for a focused view.
    let mut children: Vec<&FlameNode> = node.children.iter().collect();
    children.sort_by(|a, b| b.on_cpu_ns.cmp(&a.on_cpu_ns));
    for child in children {
        print_flame_node(
            child,
            strings,
            total_ns,
            threshold_pct,
            depth + 1,
            max_depth,
        );
    }
}

fn parse_address(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    u64::from_str_radix(rest, 16).ok()
}

/// Look up the address to feed to `subscribe_annotated`. `target`
/// is either a hex address (returned as-is) or a substring of a
/// demangled function name; in the latter case we ask the server
/// for the top-N leaf-self functions and return the hottest one
/// whose name contains the substring (case-insensitive).
async fn resolve_target(
    client: &ProfilerClient,
    target: &str,
    params: ViewParams,
) -> Result<u64, Box<dyn Error>> {
    if let Some(addr) = parse_address(target) {
        return Ok(addr);
    }
    let needle = target.to_lowercase();
    // 256 entries is enough to catch any function the user is
    // realistically asking about; we sort by self_pet_samples on
    // the server side already.
    let entries = client
        .top(256, TopSort::BySelf, params)
        .await
        .map_err(|e| format!("{e:?}"))?;
    if entries.is_empty() {
        return Err("no samples on the server (run a recording first, then retry)".into());
    }
    let hit = entries.iter().find(|e| {
        e.function_name
            .as_deref()
            .map(|n| n.to_lowercase().contains(&needle))
            .unwrap_or(false)
    });
    match hit {
        Some(e) => {
            eprintln!(
                "stax: matched {:?} → {} ({} self samples)",
                target,
                e.function_name.as_deref().unwrap_or("<unresolved>"),
                e.self_pet_samples,
            );
            Ok(e.address)
        }
        None => {
            // Help the user out by showing what *did* land in top.
            let mut suggestions: Vec<&str> = entries
                .iter()
                .filter_map(|e| e.function_name.as_deref())
                .take(8)
                .collect();
            suggestions.dedup();
            let hint = if suggestions.is_empty() {
                String::new()
            } else {
                format!(
                    "\nhottest names in this run:\n  - {}",
                    suggestions.join("\n  - "),
                )
            };
            Err(format!("no symbol matching {target:?} in the current run{hint}").into())
        }
    }
}

fn print_server_status(status: &ServerStatus) {
    if let Some(active) = status.active.first() {
        println!("active run:");
        print_run_one_line(active);
    } else {
        println!("no active run");
    }
}

fn print_diagnostics(snapshot: &DiagnosticsSnapshot) {
    println!("stax diagnostics");
    if let Some(active) = snapshot.active.first() {
        println!("active run:");
        print_run_one_line(active);
    } else {
        println!("active run: none");
    }
    print_telemetry(&snapshot.telemetry);
}

fn print_telemetry(snapshot: &TelemetrySnapshot) {
    println!();
    println!("telemetry: {}", snapshot.component);

    if !snapshot.phases.is_empty() {
        println!("phases:");
        for phase in &snapshot.phases {
            println!(
                "  {:24} {:18} {:>8}  {}",
                phase.name,
                phase.state,
                format_duration_ns(phase.elapsed_ns),
                phase.detail
            );
        }
    }

    if !snapshot.gauges.is_empty() {
        println!("gauges:");
        for gauge in &snapshot.gauges {
            println!("  {:32} {}", gauge.name, gauge.value);
        }
    }

    if !snapshot.counters.is_empty() {
        println!("counters:");
        for counter in &snapshot.counters {
            println!("  {:32} {}", counter.name, counter.value);
        }
    }

    if !snapshot.histograms.is_empty() {
        println!("histograms:");
        for histogram in &snapshot.histograms {
            print_histogram(histogram);
        }
    }

    if !snapshot.recent_events.is_empty() {
        println!("recent events:");
        for event in &snapshot.recent_events {
            println!("  {}  {:24} {}", event.at_unix_ns, event.name, event.detail);
        }
    }
}

fn print_histogram(histogram: &HistogramSnapshot) {
    let avg = if histogram.count == 0 {
        0
    } else {
        histogram.sum / histogram.count
    };
    println!(
        "  {} count={} avg={} max={} overflow={}",
        histogram.name,
        histogram.count,
        format_duration_ns(avg),
        format_duration_ns(histogram.max),
        histogram.overflow
    );
    for bucket in &histogram.buckets {
        if bucket.count != 0 {
            println!(
                "    <= {:>8}: {}",
                format_duration_ns(bucket.le),
                bucket.count
            );
        }
    }
}

fn format_duration_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    } else if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.2}µs", ns as f64 / 1_000.0)
    } else {
        format!("{ns}ns")
    }
}

fn print_run_one_line(run: &RunSummary) {
    let pid = run
        .target_pid
        .map(|p| format!("pid {p}"))
        .unwrap_or_else(|| "no pid".to_owned());
    let state = match run.state {
        stax_live_proto::RunState::Recording => "recording",
        stax_live_proto::RunState::Stopped => "stopped",
    };
    println!(
        "  run {}  [{state}]  {}  {} kperf / {} intervals  ({})",
        run.id.0, pid, run.pet_samples, run.off_cpu_intervals, run.label
    );
}
