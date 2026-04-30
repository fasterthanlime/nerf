//! `stax-server` — long-running unprivileged daemon.
//!
//! Hosts the run registry (one active + history) plus the live
//! aggregator + binary registry. Three vox services are exposed over
//! the local socket:
//!
//! - `RunControl` — agent-facing lifecycle (status / wait / stop / list).
//! - `RunIngest` — recorder pushes IngestBatches into the active run.
//! - `Profiler`  — query the live aggregator (top, flamegraph, annotate, …).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use metrix::{CounterHandle, GaugeHandle, HistogramHandle, PhaseHandle, TelemetryRegistry};
use parking_lot::{Mutex, RwLock};
use stax_live::source::SourceResolver;
use stax_live::{
    Aggregator, BinaryRegistry, IntervalKind, LiveServer, LiveSymbolOwned, LoadedBinary, PmuSample,
};
use stax_live_proto::{
    DiagnosticsSnapshot, IngestBatch, IngestEvent, LaunchEnvVar, LaunchRequest, ProfilerDispatcher,
    RunConfig, RunControl, RunControlDispatcher, RunControlError, RunId, RunIngest,
    RunIngestDispatcher, RunIngestError, RunState, RunSummary, ServerStatus, StopReason,
    TerminalBroker, TerminalBrokerDispatcher, TerminalBrokerError, TerminalInput, TerminalOutput,
    WaitCondition, WaitOutcome, WireBinaryLoaded, WireBinaryUnloaded,
};
use stax_shade_proto::{
    ShadeAck, ShadeCommand, ShadeError, ShadeInfo, ShadeRegistry, ShadeRegistryDispatcher,
};
use vox::VoxListener;

const DEFAULT_SOCK_NAME: &str = "stax-server.sock";
const DEFAULT_WS_BIND: &str = "127.0.0.1:8080";
const STAX_SERVER_CHANNEL_CAPACITY: u32 = 64;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    init_logging();
    let _vox_sigusr1_dump = stax_vox_observe::install_global_sigusr1_dump("stax-server");

    let socket = resolve_socket_path();
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }

    let server = ServerState::new(socket.clone());
    let _telemetry_registration = stax_vox_observe::register_global_telemetry(
        "stax-server",
        "server",
        server.telemetry.registry.clone(),
    );
    server.attach_local_shared_cache();

    let local_listener =
        vox::transport::local::LocalLinkAcceptor::bind(socket.to_string_lossy().into_owned())?;
    tracing::info!("stax-server listening on local://{}", socket.display());

    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));

    let ws_addr =
        std::env::var("STAX_SERVER_WS_BIND").unwrap_or_else(|_| DEFAULT_WS_BIND.to_owned());
    let mut ws_listener = vox::WsListener::bind(&ws_addr).await?;
    let ws_local = ws_listener.local_addr()?;
    tracing::info!("stax-server listening on ws://{ws_local}");

    server.spawn_shade_liveness_watchdog();

    let local_loop = tokio::spawn({
        let server = server.clone();
        async move {
            loop {
                let link = match local_listener.accept().await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("stax-server: local accept failed: {e}");
                        continue;
                    }
                };
                spawn_session_local(server.clone(), link);
            }
        }
    });
    let ws_loop = tokio::spawn({
        let server = server.clone();
        async move {
            loop {
                let link = match ws_listener.accept().await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("stax-server: ws accept failed: {e}");
                        continue;
                    }
                };
                spawn_session_ws(server.clone(), link);
            }
        }
    });

    tokio::select! {
        _ = local_loop => {},
        _ = ws_loop => {},
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("stax-server: SIGINT, shutting down");
        }
    }
    Ok(())
}

/// Per-session bookkeeping. Currently just tracks the shade-pid (if
/// any) registered through this session, so the post-`closed()`
/// cleanup can call `shade_session_closed`. Shared between the
/// acceptor closure and the cleanup tail.
type ShadeSlot = Arc<parking_lot::Mutex<Option<u32>>>;

/// Build a fresh routing factory for one session. Per-session so
/// concurrent sessions get independent `ShadeSlot`s.
fn build_factory(
    server: ServerState,
    shade_slot: ShadeSlot,
) -> impl vox::ConnectionAcceptor + 'static {
    vox::acceptor_fn(
        move |request: &vox::ConnectionRequest,
              connection: vox::PendingConnection|
              -> Result<(), vox::Metadata<'static>> {
            match request.service() {
                "RunControl" => {
                    connection.handle_with(RunControlDispatcher::new(server.clone()));
                    Ok(())
                }
                "RunIngest" => {
                    connection.handle_with(
                        RunIngestDispatcher::new(server.clone())
                            .with_middleware(vox::ServerLogging::default()),
                    );
                    Ok(())
                }
                "TerminalBroker" => {
                    connection.handle_with(TerminalBrokerDispatcher::new(server.clone()));
                    Ok(())
                }
                "Profiler" => {
                    connection.handle_with(ProfilerDispatcher::new(server.profiler()));
                    Ok(())
                }
                "ShadeRegistry" => {
                    connection.handle_with(ShadeRegistryDispatcher::new(ShadeRegistryImpl {
                        server: server.clone(),
                        shade_slot: shade_slot.clone(),
                    }));
                    Ok(())
                }
                other => {
                    tracing::warn!("stax-server: rejecting unknown service {other:?}");
                    Err(vec![])
                }
            }
        },
    )
}

/// Local-socket accept path uses non_resumable so the daemon notices
/// when the recorder process disappears (resumable would keep the
/// session in recovery mode and the per-channel send would silently
/// succeed into a void).
fn spawn_session_local(server: ServerState, link: vox::transport::local::LocalLink) {
    let shade_slot: ShadeSlot = Arc::new(parking_lot::Mutex::new(None));
    let factory = build_factory(server.clone(), shade_slot.clone());
    let observer = server.telemetry.vox_observer("local");
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .channel_capacity(STAX_SERVER_CHANNEL_CAPACITY)
            .observer(observer)
            .non_resumable()
            .keepalive(vox::SessionKeepaliveConfig {
                ping_interval: std::time::Duration::from_secs(5),
                pong_timeout: std::time::Duration::from_secs(30),
            })
            .on_connection(factory)
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => {
                let _debug_registration = stax_vox_observe::register_global_caller(
                    "stax-server",
                    "local",
                    "root",
                    &client.caller,
                );
                client.caller.closed().await;
            }
            Err(e) => tracing::warn!("stax-server: local session establish failed: {e:?}"),
        }
        cleanup_session_shade(&server, &shade_slot);
    });
}

/// WS-side accept path. Browsers don't usually deliver the
/// non-resumable + keepalive guarantees the local-socket peers
/// (recorder, shade) need, so the WS side stays plain
/// resumable-by-default; we still want the per-session slot so a
/// browser-resident shade (future debugging tool) would have a
/// cleanup path too.
fn spawn_session_ws(server: ServerState, link: <vox::WsListener as vox::VoxListener>::Link) {
    let shade_slot: ShadeSlot = Arc::new(parking_lot::Mutex::new(None));
    let factory = build_factory(server.clone(), shade_slot.clone());
    let observer = server.telemetry.vox_observer("ws");
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .channel_capacity(STAX_SERVER_CHANNEL_CAPACITY)
            .observer(observer)
            .on_connection(factory)
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => {
                let _debug_registration = stax_vox_observe::register_global_caller(
                    "stax-server",
                    "ws",
                    "root",
                    &client.caller,
                );
                client.caller.closed().await;
            }
            Err(e) => tracing::warn!("stax-server: ws session establish failed: {e:?}"),
        }
        cleanup_session_shade(&server, &shade_slot);
    });
}

/// If this session had a shade registered through it, tell the
/// server so it can clear `active_shade`. Tolerates the slot being
/// empty (most sessions don't have a shade — e.g. `stax top` from
/// a CLI agent).
fn cleanup_session_shade(server: &ServerState, slot: &ShadeSlot) {
    if let Some(pid) = slot.lock().take() {
        server.shade_session_closed(pid);
    }
}

fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("STAX_SERVER_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join(DEFAULT_SOCK_NAME);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/stax-server-{uid}.sock"))
}

/// Shared state. The aggregator + binary registry persist across
/// runs (a new run resets them); historical Profiler queries aren't
/// addressable yet — that ships when `Profiler` learns to take a
/// `RunId`.
#[derive(Clone)]
struct ServerState {
    inner: Arc<Mutex<Inner>>,
    aggregator: Arc<RwLock<Aggregator>>,
    binaries: Arc<RwLock<BinaryRegistry>>,
    revision: Arc<AtomicU64>,
    source: Arc<Mutex<SourceResolver>>,
    paused: Arc<AtomicBool>,
    telemetry: ServerTelemetry,
    started_at_unix_ns: u64,
    next_run_id: Arc<AtomicU64>,
    terminal: Arc<Mutex<TerminalState>>,
    /// Local-socket path the server is listening on. Used to tell
    /// auto-spawned stax-shade processes how to dial back in.
    socket_path: Arc<PathBuf>,
}

#[derive(Clone)]
struct ServerTelemetry {
    registry: TelemetryRegistry,
    run_phase: PhaseHandle,
    ingest_phase: PhaseHandle,
    reliable_phase: PhaseHandle,
    runs_started: CounterHandle,
    runs_stopped: CounterHandle,
    ingest_recv_errors: CounterHandle,
    ingest_channel_closed: CounterHandle,
    ingest_samples: CounterHandle,
    ingest_probe_results: CounterHandle,
    ingest_on_cpu: CounterHandle,
    ingest_off_cpu: CounterHandle,
    ingest_binaries_loaded: CounterHandle,
    ingest_binaries_unloaded: CounterHandle,
    ingest_target_attached: CounterHandle,
    ingest_thread_names: CounterHandle,
    ingest_wakeups: CounterHandle,
    reliable_target_attached: CounterHandle,
    reliable_binaries_loaded: CounterHandle,
    reliable_binaries_unloaded: CounterHandle,
    active_run_id: GaugeHandle,
    active_pet_samples: GaugeHandle,
    active_off_cpu_intervals: GaugeHandle,
    ingest_drainer_active: GaugeHandle,
    reliable_call_latency: HistogramHandle,
}

impl ServerTelemetry {
    fn new() -> Self {
        let registry = TelemetryRegistry::new("stax-server");
        Self {
            run_phase: registry.phase("run.lifecycle"),
            ingest_phase: registry.phase("ingest.drainer"),
            reliable_phase: registry.phase("ingest.reliable"),
            runs_started: registry.counter("runs.started"),
            runs_stopped: registry.counter("runs.stopped"),
            ingest_recv_errors: registry.counter("ingest.recv_errors"),
            ingest_channel_closed: registry.counter("ingest.channel_closed"),
            ingest_samples: registry.counter("ingest.events.sample"),
            ingest_probe_results: registry.counter("ingest.events.probe_result"),
            ingest_on_cpu: registry.counter("ingest.events.on_cpu"),
            ingest_off_cpu: registry.counter("ingest.events.off_cpu"),
            ingest_binaries_loaded: registry.counter("ingest.events.binary_loaded"),
            ingest_binaries_unloaded: registry.counter("ingest.events.binary_unloaded"),
            ingest_target_attached: registry.counter("ingest.events.target_attached"),
            ingest_thread_names: registry.counter("ingest.events.thread_name"),
            ingest_wakeups: registry.counter("ingest.events.wakeup"),
            reliable_target_attached: registry.counter("ingest.reliable.target_attached"),
            reliable_binaries_loaded: registry.counter("ingest.reliable.binary_loaded"),
            reliable_binaries_unloaded: registry.counter("ingest.reliable.binary_unloaded"),
            active_run_id: registry.gauge("active.run_id"),
            active_pet_samples: registry.gauge("active.pet_samples"),
            active_off_cpu_intervals: registry.gauge("active.off_cpu_intervals"),
            ingest_drainer_active: registry.gauge("ingest.drainer.active"),
            reliable_call_latency: registry.histogram("ingest.reliable.latency_ns"),
            registry,
        }
    }

    fn record_ingest_event(&self, event: &IngestEvent) {
        match event {
            IngestEvent::Sample(_) => self.ingest_samples.inc(1),
            IngestEvent::ProbeResult(_) => self.ingest_probe_results.inc(1),
            IngestEvent::OnCpuInterval(_) => self.ingest_on_cpu.inc(1),
            IngestEvent::OffCpuInterval(_) => self.ingest_off_cpu.inc(1),
            IngestEvent::BinaryLoaded(_) => self.ingest_binaries_loaded.inc(1),
            IngestEvent::BinaryUnloaded(_) => self.ingest_binaries_unloaded.inc(1),
            IngestEvent::TargetAttached { .. } => self.ingest_target_attached.inc(1),
            IngestEvent::ThreadName { .. } => self.ingest_thread_names.inc(1),
            IngestEvent::Wakeup(_) => self.ingest_wakeups.inc(1),
        }
    }

    fn set_active_counts(&self, pet_samples: u64, off_cpu_intervals: u64) {
        self.active_pet_samples.set(saturating_i64(pet_samples));
        self.active_off_cpu_intervals
            .set(saturating_i64(off_cpu_intervals));
    }

    fn vox_observer(&self, surface: &'static str) -> stax_vox_observe::VoxObserverLogger {
        stax_vox_observe::VoxObserverLogger::new("stax-server", surface)
            .with_telemetry(self.registry.clone())
    }
}

struct Inner {
    active: Option<RunSummary>,
    /// Notify that wakes the active run's drainer task so it drops
    /// its `Rx<IngestBatch>`. Set when a run starts, cleared by
    /// `stop_active` (which then notifies, and by the drainer
    /// itself when its Rx closes naturally).
    cancel: Option<Arc<tokio::sync::Notify>>,
    /// Currently-attached shade, if any. Set by `register_shade`
    /// when stax-shade dials in; cleared when its session closes.
    /// One active run = at most one shade.
    active_shade: Option<ShadeInfo>,
    /// Server->shade control channel for the active run. This is
    /// the clean stop path: server owns run lifecycle, shade owns
    /// target/staxd teardown.
    active_shade_commands: Option<vox::Tx<ShadeCommand>>,
    ingest_attached: bool,
    /// Process handle for the server-spawned stax-shade child.
    /// Set by RunControl start calls; reaped + cleared by
    /// `stop_active` / `finalize_run` (or when the child exits on
    /// its own and the session-close cleanup fires). One active run
    /// = at most one child.
    shade_child: Option<ShadeChild>,
    history: Vec<RunSummary>,
}

struct ShadeChild {
    pid: u32,
    child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

#[derive(Default)]
struct TerminalState {
    pending: HashMap<u64, PendingTerminal>,
}

struct PendingTerminal {
    input_from_frontend: vox::Rx<TerminalInput>,
    output_to_frontend: vox::Tx<TerminalOutput>,
}

enum ShadeTarget {
    Attach(u32),
    Launch(LaunchRequest),
}

impl ServerState {
    fn new(socket_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                cancel: None,
                active_shade: None,
                active_shade_commands: None,
                ingest_attached: false,
                shade_child: None,
                history: Vec::new(),
            })),
            aggregator: Arc::new(RwLock::new(Aggregator::default())),
            binaries: Arc::new(RwLock::new(BinaryRegistry::new())),
            revision: Arc::new(AtomicU64::new(1)),
            source: Arc::new(Mutex::new(SourceResolver::new())),
            paused: Arc::new(AtomicBool::new(false)),
            telemetry: ServerTelemetry::new(),
            started_at_unix_ns: now_unix_ns(),
            next_run_id: Arc::new(AtomicU64::new(1)),
            terminal: Arc::new(Mutex::new(TerminalState::default())),
            socket_path: Arc::new(socket_path),
        }
    }

    /// Open the host's dyld shared cache and plug it into the
    /// binary registry as a Mach-O byte source. This is what makes
    /// `stax annotate` against a libsystem / libdispatch / etc.
    /// address actually return disassembly: the recorder ships
    /// `BinaryLoaded` events with symbols for those images, but
    /// it can't ferry the bytes (an mmap doesn't cross processes),
    /// so the server opens the cache itself.
    ///
    /// No-op + warning when the cache isn't found; symbol-name
    /// queries still work, just disassembly of cache-resident code
    /// won't.
    #[cfg(target_os = "macos")]
    fn attach_local_shared_cache(&self) {
        match stax_mac_shared_cache::SharedCache::for_host() {
            Some(cache) => {
                let cache = Arc::new(cache);
                let mut binaries = self.binaries.write();
                binaries.set_macho_byte_source(cache.clone());
                binaries.set_shared_cache(cache);
                tracing::info!(
                    "stax-server: dyld shared cache mapped for symbol lookup + disassembly fallback"
                );
            }
            None => {
                tracing::warn!(
                    "stax-server: no dyld shared cache available; \
                     dyld-resident symbols will surface as <unresolved>"
                );
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn attach_local_shared_cache(&self) {}

    /// View suitable for hosting the existing `Profiler` impl.
    fn profiler(&self) -> LiveServer {
        LiveServer {
            aggregator: self.aggregator.clone(),
            binaries: self.binaries.clone(),
            revision: self.revision.clone(),
            source: self.source.clone(),
            paused: self.paused.clone(),
        }
    }

    /// Validate + record a shade registration. Used by
    /// `ShadeRegistryImpl` (per-session wrapper) so the per-session
    /// `ShadeSlot` can be populated atomically with the
    /// server-side `active_shade`.
    fn try_register_shade(&self, info: ShadeInfo, commands: vox::Tx<ShadeCommand>) -> ShadeAck {
        let mut inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return ShadeAck {
                accepted: false,
                reason: Some("no active run".to_owned()),
            };
        };
        if active.id.0 != info.run_id {
            return ShadeAck {
                accepted: false,
                reason: Some(format!(
                    "run id mismatch: shade for {}, server's active run is {}",
                    info.run_id, active.id.0
                )),
            };
        }
        if inner.active_shade.is_some() {
            return ShadeAck {
                accepted: false,
                reason: Some("a shade is already registered for this run".to_owned()),
            };
        }
        tracing::info!(
            run_id = info.run_id,
            target_pid = info.target_pid,
            shade_pid = info.shade_pid,
            "shade registered"
        );
        inner.active_shade = Some(info);
        inner.active_shade_commands = Some(commands);
        ShadeAck {
            accepted: true,
            reason: None,
        }
    }

    /// Called by the accept-loop after a session closes, with the
    /// `shade_pid` (if any) that registered through this session.
    /// Clears `active_shade` if it still matches — the run may
    /// have stopped between registration and close, in which case
    /// `stop_active` already cleared it and we no-op.
    fn shade_session_closed(&self, shade_pid: u32) {
        let mut inner = self.inner.lock();
        if let Some(info) = inner.active_shade.as_ref()
            && info.shade_pid == shade_pid
        {
            tracing::info!(shade_pid, "shade session closed; clearing active_shade");
            inner.active_shade = None;
            inner.active_shade_commands = None;
        }
    }

    /// Spawn a `stax-shade` child process to attach to `pid`.
    /// Stores the `Child` handle on `Inner::shade_child` so we
    /// can reap on cleanup. Best-effort: if the binary isn't
    /// found or `Command::spawn` fails we log and move on. The
    /// shade will dial back in on its own and call
    /// `register_shade`, at which point `active_shade` gets
    /// populated.
    fn spawn_shade(
        &self,
        run_id: RunId,
        target: ShadeTarget,
        frequency_hz: u32,
        correlate_frequency_hz: u32,
        race_kperf: bool,
        correlate_kperf: bool,
        daemon_socket: String,
        time_limit_secs: Option<u64>,
    ) -> Result<(), String> {
        let bin = match resolve_shade_path() {
            Some(p) => p,
            None => {
                return Err(
                    "stax-shade binary not found; run `cargo xtask install` or set STAX_SHADE_BIN"
                        .to_owned(),
                );
            }
        };
        let socket = self.socket_path.to_string_lossy().into_owned();
        let mut cmd = std::process::Command::new(&bin);
        let mut launch_command: Option<Vec<String>> = None;
        let target_log = match &target {
            ShadeTarget::Attach(pid) => format!("pid {pid}"),
            ShadeTarget::Launch(request) => {
                format!(
                    "launch {}",
                    request
                        .command
                        .first()
                        .map(String::as_str)
                        .unwrap_or("<empty>")
                )
            }
        };
        match target {
            ShadeTarget::Attach(pid) => {
                cmd.arg("--attach").arg(pid.to_string());
            }
            ShadeTarget::Launch(request) => {
                cmd.arg("--launch").arg("--cwd").arg(&request.cwd);
                if let Some(size) = request.terminal_size {
                    cmd.arg("--terminal-rows")
                        .arg(size.rows.to_string())
                        .arg("--terminal-cols")
                        .arg(size.cols.to_string());
                }
                cmd.current_dir(&request.cwd);
                cmd.env_clear();
                for LaunchEnvVar { key, value } in request.env {
                    cmd.env(key, value);
                }
                launch_command = Some(request.command);
            }
        }
        cmd.arg("--server-socket")
            .arg(&socket)
            .arg("--run-id")
            .arg(run_id.0.to_string())
            .arg("--daemon-socket")
            .arg(daemon_socket)
            .arg("--frequency")
            .arg(frequency_hz.to_string());
        if correlate_kperf {
            cmd.arg("--correlate-frequency")
                .arg(correlate_frequency_hz.to_string());
        }
        if race_kperf {
            cmd.arg("--race-kperf");
        }
        if correlate_kperf {
            cmd.arg("--correlate-kperf");
        }
        if let Some(limit) = time_limit_secs {
            cmd.arg("--time-limit").arg(limit.to_string());
        }
        if let Some(command) = launch_command {
            cmd.arg("--").args(command);
        }
        cmd
            // Don't inherit our stdin/out/err — the shade logs via
            // os_log on its own subsystem; nothing useful would
            // come out of the inherited fds. Stdin null also stops
            // the legacy stdin-EOF park-loop from racing the vox
            // session-close path.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let shade_pid = child.id();
                tracing::info!(
                    run_id = run_id.0,
                    target = %target_log,
                    shade_pid,
                    bin = %bin.display(),
                    "spawned stax-shade"
                );
                let child = Arc::new(std::sync::Mutex::new(Some(child)));
                self.inner.lock().shade_child = Some(ShadeChild {
                    pid: shade_pid,
                    child: child.clone(),
                });
                self.spawn_shade_child_monitor(run_id, shade_pid, child);
                Ok(())
            }
            Err(e) => Err(format!("failed to spawn {}: {e}", bin.display())),
        }
    }

    /// Reap the shade child (if any), preferring a clean exit but
    /// killing if it's still running after a brief grace period.
    /// Called from `stop_active` and `finalize_run`. The shade is
    /// supposed to notice its vox session close on its own; this
    /// is the belt-and-suspenders.
    fn reap_shade_child(&self) {
        let shade_child = match self.inner.lock().shade_child.take() {
            Some(c) => c,
            None => return,
        };
        reap_taken_shade_child(shade_child);
    }

    fn spawn_shade_child_monitor(
        &self,
        run_id: RunId,
        shade_pid: u32,
        child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
    ) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                tick.tick().await;
                let status = {
                    let mut guard = match child.lock() {
                        Ok(guard) => guard,
                        Err(e) => {
                            tracing::warn!("stax-shade child mutex poisoned: {e}");
                            return;
                        }
                    };
                    let Some(child) = guard.as_mut() else {
                        return;
                    };
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            *guard = None;
                            status
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::warn!(shade_pid, "try_wait on stax-shade child failed: {e}");
                            return;
                        }
                    }
                };
                state.shade_child_exited(run_id, shade_pid, status);
                return;
            }
        });
    }

    fn shade_child_exited(&self, run_id: RunId, shade_pid: u32, status: std::process::ExitStatus) {
        let should_finalize = {
            let mut inner = self.inner.lock();
            if let Some(child) = inner.shade_child.as_ref()
                && child.pid == shade_pid
            {
                inner.shade_child = None;
            }
            matches!(inner.active.as_ref(), Some(active) if active.id == run_id)
        };
        if !should_finalize {
            return;
        }

        let reason = if status.success() {
            tracing::info!(run_id = run_id.0, shade_pid, ?status, "stax-shade exited");
            StopReason::TargetExited
        } else {
            tracing::warn!(run_id = run_id.0, shade_pid, ?status, "stax-shade exited");
            StopReason::RecorderError {
                message: format!("stax-shade exited: {status}"),
            }
        };
        self.finalize_run(run_id, reason);
    }

    fn detach_ingest(&self, run_id: RunId, reason: &'static str) {
        let mut inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return;
        };
        if active.id != run_id {
            return;
        }
        inner.ingest_attached = false;
        tracing::warn!(
            run_id = run_id.0,
            reason,
            "ingest channel detached; keeping run alive until shade exits or explicit stop"
        );
    }

    /// Belt-and-suspenders for the case where vox's session
    /// keepalive fails to surface a dead shade in a timely way:
    /// every few seconds, if there's an `active_shade`, check
    /// whether its pid is still alive via `kill(pid, 0)`. ESRCH
    /// → process is gone → clear. Cheap (one syscall per tick)
    /// and correctness-critical: the alternative is a permanently
    /// stuck `active_shade` slot blocking new attachments.
    fn spawn_shade_liveness_watchdog(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                let pid = match state.inner.lock().active_shade.as_ref() {
                    Some(info) => info.shade_pid,
                    None => continue,
                };
                if !pid_is_alive(pid) {
                    let mut inner = state.inner.lock();
                    if let Some(info) = inner.active_shade.as_ref()
                        && info.shade_pid == pid
                    {
                        tracing::warn!(
                            shade_pid = pid,
                            "shade pid no longer alive; clearing active_shade"
                        );
                        inner.active_shade = None;
                        inner.active_shade_commands = None;
                    }
                }
            }
        });
    }

    fn begin_run(&self, config: RunConfig) -> Result<(RunId, Arc<tokio::sync::Notify>), String> {
        let stale_child = {
            let mut inner = self.inner.lock();
            match inner.active.as_ref() {
                Some(active) if active.state == RunState::Stopped => {
                    let active = inner.active.take().expect("checked above");
                    tracing::warn!(
                        run_id = active.id.0,
                        "stale stopped run was still marked active; clearing it before new run"
                    );
                    if !inner.history.iter().any(|run| run.id == active.id) {
                        inner.history.push(active);
                    }
                    inner.cancel = None;
                    inner.active_shade = None;
                    inner.active_shade_commands = None;
                    inner.ingest_attached = false;
                    inner.shade_child.take()
                }
                _ => None,
            }
        };
        if let Some(shade_child) = stale_child {
            reap_taken_shade_child(shade_child);
        }

        let id = RunId(self.next_run_id.fetch_add(1, Ordering::Relaxed));
        let summary = RunSummary {
            id,
            state: RunState::Recording,
            stop_reason: None,
            started_at_unix_ns: now_unix_ns(),
            stopped_at_unix_ns: None,
            target_pid: None,
            label: config.label,
            pet_samples: 0,
            off_cpu_intervals: 0,
        };
        let cancel = Arc::new(tokio::sync::Notify::new());
        {
            let mut inner = self.inner.lock();
            if inner.active.is_some() {
                return Err("another run is already active; \
                    call RunControl::stop_active or wait_active first"
                    .to_owned());
            }
            inner.active = Some(summary);
            inner.cancel = Some(cancel.clone());
            inner.ingest_attached = false;
        }

        *self.aggregator.write() = Aggregator::default();
        *self.binaries.write() = BinaryRegistry::new();
        self.bump_revision();
        self.attach_local_shared_cache();

        tracing::info!(
            "stax-server: run {} started (frequency_hz={} correlate_frequency_hz={})",
            id.0,
            config.frequency_hz,
            config.correlate_frequency_hz
        );
        self.telemetry.runs_started.inc(1);
        self.telemetry.active_run_id.set(saturating_i64(id.0));
        self.telemetry.set_active_counts(0, 0);
        self.telemetry.run_phase.enter(
            "recording",
            format!(
                "run_id={} frequency_hz={} correlate_frequency_hz={}",
                id.0, config.frequency_hz, config.correlate_frequency_hz
            ),
        );
        self.telemetry.registry.event(
            "run.started",
            format!(
                "run_id={} label={}",
                id.0,
                self.inner
                    .lock()
                    .active
                    .as_ref()
                    .map(|run| run.label.as_str())
                    .unwrap_or("")
            ),
        );
        Ok((id, cancel))
    }

    fn cancel_for_run(&self, run_id: RunId) -> Result<Arc<tokio::sync::Notify>, String> {
        let inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return Err("no active run".to_owned());
        };
        if active.id != run_id {
            return Err(format!(
                "run id mismatch: ingest for {}, active run is {}",
                run_id.0, active.id.0
            ));
        }
        inner
            .cancel
            .clone()
            .ok_or_else(|| "active run has no cancel handle".to_owned())
    }

    fn spawn_ingest_drainer(
        &self,
        id: RunId,
        cancel: Arc<tokio::sync::Notify>,
        mut events: vox::Rx<IngestBatch>,
    ) {
        let state = self.clone();
        let telemetry = self.telemetry.clone();
        tokio::spawn(async move {
            let mut counts = IngestDrainCounts::default();
            let mut first_event_logged = false;
            let mut last_log = std::time::Instant::now();
            let exit_reason: &'static str;
            telemetry.ingest_drainer_active.set(1);
            telemetry
                .ingest_phase
                .enter("waiting", format!("run_id={}", id.0));
            telemetry
                .registry
                .event("ingest.drainer.started", format!("run_id={}", id.0));
            tracing::info!(run_id = id.0, "stax-server: ingest drainer started");
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.notified() => {
                        exit_reason = "cancel";
                        telemetry.ingest_phase.enter("cancelled", format!("run_id={}", id.0));
                        break;
                    }
                    recv = events.recv() => match recv {
                        Ok(Some(batch_sref)) => {
                            let state = state.clone();
                            let _ = batch_sref.map(|batch| {
                                let batch_len = batch.events.len();
                                for event in batch.events {
                                    let kind = ingest_event_kind(&event);
                                    counts.record(&event);
                                    telemetry.record_ingest_event(&event);
                                    if !first_event_logged {
                                        first_event_logged = true;
                                        telemetry.ingest_phase.enter("draining", format!("run_id={} first_kind={kind}", id.0));
                                        telemetry.registry.event("ingest.first_event", format!("run_id={} kind={kind}", id.0));
                                        tracing::info!(
                                            run_id = id.0,
                                            kind,
                                            batch_len,
                                            counts = %counts.summary(),
                                            "stax-server: ingest drainer received first batch"
                                        );
                                    }
                                    state.apply_event(id, event);
                                }
                            });
                            if last_log.elapsed() >= Duration::from_secs(2) {
                                tracing::info!(
                                    run_id = id.0,
                                    counts = %counts.summary(),
                                    "stax-server: ingest drainer progress"
                                );
                                last_log = std::time::Instant::now();
                            }
                        }
                        Ok(None) => {
                            exit_reason = "channel_closed";
                            telemetry.ingest_channel_closed.inc(1);
                            telemetry.ingest_phase.enter("channel_closed", format!("run_id={}", id.0));
                            break;
                        }
                        Err(err) => {
                            telemetry.ingest_recv_errors.inc(1);
                            telemetry.ingest_phase.enter("recv_error", format!("run_id={} error={err:?}", id.0));
                            tracing::warn!(
                                run_id = id.0,
                                error = ?err,
                                counts = %counts.summary(),
                                "stax-server: ingest drainer recv failed"
                            );
                            exit_reason = "recv_error";
                            break;
                        }
                    },
                }
            }
            tracing::info!(
                run_id = id.0,
                reason = exit_reason,
                counts = %counts.summary(),
                "stax-server: ingest drainer exiting"
            );
            telemetry.ingest_drainer_active.set(0);
            telemetry.registry.event(
                "ingest.drainer.exiting",
                format!("run_id={} reason={exit_reason}", id.0),
            );
            drop(events);
            if exit_reason == "cancel" {
                state.finalize_run(id, StopReason::UserStop);
            } else {
                state.detach_ingest(id, exit_reason);
            }
        });
    }

    fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn attach_terminal_channels(
        &self,
        run_id: RunId,
        input_to_shade: vox::Tx<TerminalInput>,
        mut output_from_shade: vox::Rx<TerminalOutput>,
    ) -> Result<(), String> {
        let PendingTerminal {
            mut input_from_frontend,
            output_to_frontend,
        } = self
            .terminal
            .lock()
            .pending
            .remove(&run_id.0)
            .ok_or_else(|| format!("no pending terminal for run {}", run_id.0))?;

        tracing::info!(run_id = run_id.0, "terminal channels attached");

        tokio::spawn(async move {
            loop {
                match input_from_frontend.recv().await {
                    Ok(Some(input_sref)) => {
                        let mut input = None;
                        let _ = input_sref.map(|value| {
                            input = Some(value);
                        });
                        if input_to_shade
                            .send(input.expect("input set"))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = input_to_shade.send(TerminalInput::Close).await;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("terminal input channel failed: {e:?}");
                        let _ = input_to_shade.send(TerminalInput::Close).await;
                        break;
                    }
                }
            }
        });

        tokio::spawn(async move {
            loop {
                match output_from_shade.recv().await {
                    Ok(Some(output_sref)) => {
                        let mut output = None;
                        let _ = output_sref.map(|value| {
                            output = Some(value);
                        });
                        if output_to_frontend
                            .send(output.expect("output set"))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("terminal output channel failed: {e:?}");
                        let _ = output_to_frontend
                            .send(TerminalOutput::Error {
                                message: format!("terminal output channel failed: {e:?}"),
                            })
                            .await;
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    fn discard_start_failed(&self, run_id: RunId, message: String) {
        self.terminal.lock().pending.remove(&run_id.0);
        let mut inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return;
        };
        if active.id != run_id {
            return;
        }
        let mut summary = inner.active.take().expect("checked above");
        summary.state = RunState::Stopped;
        summary.stop_reason = Some(StopReason::RecorderError { message });
        summary.stopped_at_unix_ns = Some(now_unix_ns());
        inner.cancel = None;
        inner.active_shade = None;
        inner.active_shade_commands = None;
        inner.ingest_attached = false;
        inner.shade_child = None;
        inner.history.push(summary);
        self.telemetry.active_run_id.set(0);
        self.telemetry.runs_stopped.inc(1);
        self.telemetry
            .run_phase
            .enter("start_failed", format!("run_id={}", run_id.0));
        self.telemetry
            .registry
            .event("run.start_failed", format!("run_id={}", run_id.0));
    }
}

#[derive(Default)]
struct IngestDrainCounts {
    samples: u64,
    probe_results: u64,
    on_cpu: u64,
    off_cpu: u64,
    binaries_loaded: u64,
    binaries_unloaded: u64,
    target_attached: u64,
    thread_names: u64,
    wakeups: u64,
}

impl IngestDrainCounts {
    fn record(&mut self, event: &IngestEvent) {
        match event {
            IngestEvent::Sample(_) => self.samples += 1,
            IngestEvent::ProbeResult(_) => self.probe_results += 1,
            IngestEvent::OnCpuInterval(_) => self.on_cpu += 1,
            IngestEvent::OffCpuInterval(_) => self.off_cpu += 1,
            IngestEvent::BinaryLoaded(_) => self.binaries_loaded += 1,
            IngestEvent::BinaryUnloaded(_) => self.binaries_unloaded += 1,
            IngestEvent::TargetAttached { .. } => self.target_attached += 1,
            IngestEvent::ThreadName { .. } => self.thread_names += 1,
            IngestEvent::Wakeup(_) => self.wakeups += 1,
        }
    }

    fn summary(&self) -> String {
        format!(
            "samples={} probes={} on_cpu={} off_cpu={} bin_load={} bin_unload={} target={} threads={} wakeups={}",
            self.samples,
            self.probe_results,
            self.on_cpu,
            self.off_cpu,
            self.binaries_loaded,
            self.binaries_unloaded,
            self.target_attached,
            self.thread_names,
            self.wakeups,
        )
    }
}

fn ingest_event_kind(event: &IngestEvent) -> &'static str {
    match event {
        IngestEvent::Sample(_) => "sample",
        IngestEvent::ProbeResult(_) => "probe_result",
        IngestEvent::OnCpuInterval(_) => "on_cpu",
        IngestEvent::OffCpuInterval(_) => "off_cpu",
        IngestEvent::BinaryLoaded(_) => "binary_loaded",
        IngestEvent::BinaryUnloaded(_) => "binary_unloaded",
        IngestEvent::TargetAttached { .. } => "target_attached",
        IngestEvent::ThreadName { .. } => "thread_name",
        IngestEvent::Wakeup(_) => "wakeup",
    }
}

fn reap_taken_shade_child(shade_child: ShadeChild) {
    let mut guard = match shade_child.child.lock() {
        Ok(guard) => guard,
        Err(e) => {
            tracing::warn!("stax-shade child mutex poisoned: {e}");
            return;
        }
    };
    let Some(mut child) = guard.take() else {
        return;
    };
    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::info!(?status, "stax-shade child already exited");
            return;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("try_wait on stax-shade child: {e}");
            return;
        }
    }
    // Give it a moment to notice the session close on its own.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
    }
    tracing::warn!(
        shade_pid = shade_child.pid,
        "stax-shade didn't exit within 1s of run end; sending SIGTERM"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// Find the stax-shade binary. Checks (in order):
///   1. `STAX_SHADE_BIN` env override (used by tests + tarballs)
///   2. `~/.cargo/bin/stax-shade` (where `cargo xtask install`
///      drops it)
///   3. `/usr/local/bin/stax-shade` (manual / packaged install)
fn resolve_shade_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STAX_SHADE_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home)
            .join(".cargo")
            .join("bin")
            .join("stax-shade");
        if p.exists() {
            return Some(p);
        }
    }
    let p = PathBuf::from("/usr/local/bin/stax-shade");
    if p.exists() {
        return Some(p);
    }
    None
}

/// Liveness check via `kill(pid, 0)`. Returns true while the
/// kernel still has a process table entry — a zombie counts as
/// alive (`kill(0)` returns 0 / `EPERM` for those), but a fully
/// reaped pid returns `ESRCH`. That's exactly what we want for
/// "is the shade process gone?"
fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: kill with sig=0 sends no signal; safe for any pid.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return true;
    }
    // SAFETY: errno_location is FFI-safe.
    let errno = unsafe { *libc::__error() };
    errno != libc::ESRCH
}

/// Fan tracing out to two sinks:
///
/// 1. `os_log` under subsystem `eu.bearcove.stax-server` so events
///    are visible from `log stream --predicate 'subsystem ==
///    "eu.bearcove.stax-server"'` (or Console.app) without root,
///    even when the daemon was started by launchd. This is the
///    always-on production path; we deliberately don't write a
///    log file to disk.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax_server=info,vox::server=debug"));

    let oslog = tracing_oslog::OsLogger::new("eu.bearcove.stax-server", "default");

    tracing_subscriber::registry()
        .with(filter)
        .with(oslog)
        .init();
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn saturating_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

impl RunControl for ServerState {
    async fn status(&self) -> ServerStatus {
        let inner = self.inner.lock();
        ServerStatus {
            server_started_at_unix_ns: self.started_at_unix_ns,
            active: inner.active.clone().into_iter().collect(),
        }
    }

    async fn list_runs(&self) -> Vec<RunSummary> {
        let inner = self.inner.lock();
        let mut out = inner.history.clone();
        if let Some(active) = inner.active.clone() {
            out.push(active);
        }
        out
    }

    async fn diagnostics(&self) -> DiagnosticsSnapshot {
        let inner = self.inner.lock();
        DiagnosticsSnapshot {
            server_started_at_unix_ns: self.started_at_unix_ns,
            active: inner.active.clone().into_iter().collect(),
            telemetry: self.telemetry.registry.snapshot(),
        }
    }

    async fn start_attach(
        &self,
        pid: u32,
        config: RunConfig,
        daemon_socket: String,
        time_limit_secs: Option<u64>,
    ) -> Result<RunId, RunControlError> {
        let frequency_hz = config.frequency_hz;
        let correlate_frequency_hz = config.correlate_frequency_hz;
        let race_kperf = config.race_kperf;
        let correlate_kperf = config.correlate_kperf;
        let (run_id, _) = self.begin_run(config)?;
        if let Err(e) = self.spawn_shade(
            run_id,
            ShadeTarget::Attach(pid),
            frequency_hz,
            correlate_frequency_hz,
            race_kperf,
            correlate_kperf,
            daemon_socket,
            time_limit_secs,
        ) {
            self.discard_start_failed(run_id, e.clone());
            return Err(e.into());
        }
        Ok(run_id)
    }

    async fn start_launch(
        &self,
        request: LaunchRequest,
        terminal_input: vox::Rx<TerminalInput>,
        terminal_output: vox::Tx<TerminalOutput>,
    ) -> Result<RunId, RunControlError> {
        if request.command.is_empty() {
            return Err(RunControlError::Internal {
                message: "launch command is empty".to_owned(),
            });
        }
        let frequency_hz = request.config.frequency_hz;
        let correlate_frequency_hz = request.config.correlate_frequency_hz;
        let race_kperf = request.config.race_kperf;
        let correlate_kperf = request.config.correlate_kperf;
        let daemon_socket = request.daemon_socket.clone();
        let time_limit_secs = request.time_limit_secs;
        let (run_id, _) = self.begin_run(request.config.clone())?;
        self.terminal.lock().pending.insert(
            run_id.0,
            PendingTerminal {
                input_from_frontend: terminal_input,
                output_to_frontend: terminal_output,
            },
        );
        if let Err(e) = self.spawn_shade(
            run_id,
            ShadeTarget::Launch(request),
            frequency_hz,
            correlate_frequency_hz,
            race_kperf,
            correlate_kperf,
            daemon_socket,
            time_limit_secs,
        ) {
            self.discard_start_failed(run_id, e.clone());
            return Err(e.into());
        }
        Ok(run_id)
    }

    async fn wait_active(&self, condition: WaitCondition, timeout_ms: Option<u64>) -> WaitOutcome {
        // Polling implementation while we sketch the lifecycle. Will
        // graduate to event-driven (notify on state-transition) once
        // the rest of the daemon stabilises.
        let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
        let condition_deadline = match &condition {
            WaitCondition::ForSeconds { seconds } => {
                Some(std::time::Instant::now() + Duration::from_secs(*seconds))
            }
            _ => None,
        };
        loop {
            let active = self.inner.lock().active.clone();
            let Some(active) = active else {
                return WaitOutcome::NoActiveRun;
            };
            if active.state == RunState::Stopped {
                return WaitOutcome::Stopped { summary: active };
            }
            let condition_met = match &condition {
                WaitCondition::UntilStopped => false,
                WaitCondition::ForSamples { count } => active.pet_samples >= *count,
                WaitCondition::ForSeconds { .. } => condition_deadline
                    .map(|d| std::time::Instant::now() >= d)
                    .unwrap_or(false),
                WaitCondition::UntilSymbolSeen { needle } => {
                    self.binaries.read().any_symbol_contains(needle)
                }
            };
            if condition_met {
                return WaitOutcome::ConditionMet { summary: active };
            }
            if let Some(d) = deadline
                && std::time::Instant::now() >= d
            {
                return WaitOutcome::TimedOut { summary: active };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stop_active(&self) -> Result<RunSummary, RunControlError> {
        // Mark as stopped + grab the cancel handle under the lock,
        // then notify outside the lock so the drainer can run
        // freely. The drainer is responsible for moving the run
        // from `active` to `history` once its Rx is closed; we
        // return the snapshot we just produced so the caller has
        // something to print without waiting on the recorder.
        let (snapshot, cancel, shade_commands) = {
            let mut inner = self.inner.lock();
            let snapshot = match inner.active.as_mut() {
                Some(summary) => {
                    summary.state = RunState::Stopped;
                    summary.stop_reason = Some(StopReason::UserStop);
                    summary.stopped_at_unix_ns = Some(now_unix_ns());
                    summary.clone()
                }
                None => return Err(RunControlError::NoActiveRun),
            };
            // The shade has nothing left to attach to; release
            // the slot so a follow-up run on the same server can
            // start a fresh shade. The shade process itself is
            // tolerated until its session-close cleanup fires (or
            // the liveness watchdog notices it died). It would be
            // tidier to actively detach via the Shade trait once
            // we have a shutdown method on it; for now best-effort.
            inner.active_shade = None;
            let shade_commands = inner.active_shade_commands.take();
            inner.ingest_attached = false;
            (snapshot, inner.cancel.take(), shade_commands)
        };
        if let Some(commands) = shade_commands
            && let Err(e) = commands.send(ShadeCommand::Stop).await
        {
            tracing::warn!("failed to send stop command to shade: {e:?}");
        }
        if let Some(cancel) = cancel {
            cancel.notify_waiters();
        }
        self.telemetry
            .run_phase
            .enter("stopping", format!("run_id={}", snapshot.id.0));
        self.telemetry
            .registry
            .event("run.stop_requested", format!("run_id={}", snapshot.id.0));
        // Reap the auto-spawned shade (if any). Has to happen
        // outside the inner lock — reap_shade_child takes it
        // again and may sleep up to 1s waiting for the child to
        // exit on its own.
        self.reap_shade_child();
        Ok(snapshot)
    }
}

/// Per-session `ShadeRegistry` impl. Captures the shade-pid into
/// the per-session `ShadeSlot` on successful registration so the
/// accept-loop's post-`closed()` cleanup knows which shade this
/// session owned.
#[derive(Clone)]
struct ShadeRegistryImpl {
    server: ServerState,
    shade_slot: ShadeSlot,
}

impl ShadeRegistry for ShadeRegistryImpl {
    async fn register_shade(
        &self,
        info: ShadeInfo,
        commands: vox::Tx<ShadeCommand>,
    ) -> Result<ShadeAck, ShadeError> {
        let ack = self.server.try_register_shade(info.clone(), commands);
        if ack.accepted {
            *self.shade_slot.lock() = Some(info.shade_pid);
        }
        Ok(ack)
    }
}

impl TerminalBroker for ServerState {
    async fn attach_terminal(
        &self,
        run_id: RunId,
        input_to_shade: vox::Tx<TerminalInput>,
        output_from_shade: vox::Rx<TerminalOutput>,
    ) -> Result<(), TerminalBrokerError> {
        self.attach_terminal_channels(run_id, input_to_shade, output_from_shade)
            .map_err(Into::into)
    }
}

impl RunIngest for ServerState {
    async fn start_run(
        &self,
        config: RunConfig,
        events: vox::Rx<IngestBatch>,
    ) -> Result<RunId, RunIngestError> {
        let (id, cancel) = self.begin_run(config)?;
        self.inner.lock().ingest_attached = true;
        self.spawn_ingest_drainer(id, cancel, events);
        Ok(id)
    }

    async fn attach_run(
        &self,
        run_id: RunId,
        events: vox::Rx<IngestBatch>,
    ) -> Result<(), RunIngestError> {
        let cancel = self.cancel_for_run(run_id)?;
        self.inner.lock().ingest_attached = true;
        self.spawn_ingest_drainer(run_id, cancel, events);
        Ok(())
    }

    async fn publish_target_attached(
        &self,
        run_id: RunId,
        pid: u32,
        task_port: u64,
    ) -> Result<(), RunIngestError> {
        let started = std::time::Instant::now();
        self.telemetry
            .reliable_phase
            .enter("target_attached", format!("run_id={}", run_id.0));
        self.telemetry.reliable_target_attached.inc(1);
        let result: Result<(), RunIngestError> = self
            .apply_target_attached(run_id, pid, task_port)
            .map_err(Into::into);
        self.telemetry
            .reliable_call_latency
            .record_duration(started.elapsed());
        self.telemetry.reliable_phase.enter("idle", "");
        result
    }

    async fn publish_binaries_loaded(
        &self,
        run_id: RunId,
        binaries: Vec<WireBinaryLoaded>,
    ) -> Result<(), RunIngestError> {
        let started = std::time::Instant::now();
        let count = binaries.len() as u64;
        self.telemetry.reliable_phase.enter(
            "binaries_loaded",
            format!("run_id={} count={count}", run_id.0),
        );
        self.telemetry.reliable_binaries_loaded.inc(count);
        let result: Result<(), RunIngestError> = (|| {
            for binary in binaries {
                self.apply_binary_loaded(run_id, binary)?;
            }
            Ok(())
        })();
        self.telemetry
            .reliable_call_latency
            .record_duration(started.elapsed());
        self.telemetry.reliable_phase.enter("idle", "");
        result
    }

    async fn publish_binaries_unloaded(
        &self,
        run_id: RunId,
        binaries: Vec<WireBinaryUnloaded>,
    ) -> Result<(), RunIngestError> {
        let started = std::time::Instant::now();
        let count = binaries.len() as u64;
        self.telemetry.reliable_phase.enter(
            "binaries_unloaded",
            format!("run_id={} count={count}", run_id.0),
        );
        self.telemetry.reliable_binaries_unloaded.inc(count);
        let result: Result<(), RunIngestError> = (|| {
            for binary in binaries {
                self.apply_binary_unloaded(run_id, binary.base_avma)?;
            }
            Ok(())
        })();
        self.telemetry
            .reliable_call_latency
            .record_duration(started.elapsed());
        self.telemetry.reliable_phase.enter("idle", "");
        result
    }
}

impl ServerState {
    fn ensure_active_run(&self, run_id: RunId) -> Result<(), String> {
        let inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return Err("no active run".to_owned());
        };
        if active.id != run_id {
            return Err(format!(
                "run id mismatch: ingest for {}, active run is {}",
                run_id.0, active.id.0
            ));
        }
        Ok(())
    }

    fn apply_target_attached(&self, run_id: RunId, pid: u32, task_port: u64) -> Result<(), String> {
        {
            let mut inner = self.inner.lock();
            let Some(active) = inner.active.as_mut() else {
                return Err("no active run".to_owned());
            };
            if active.id != run_id {
                return Err(format!(
                    "run id mismatch: ingest for {}, active run is {}",
                    run_id.0, active.id.0
                ));
            }
            active.target_pid = Some(pid);
        }
        self.binaries.write().set_target(pid, task_port);
        self.bump_revision();
        Ok(())
    }

    fn apply_binary_loaded(&self, run_id: RunId, b: WireBinaryLoaded) -> Result<(), String> {
        self.ensure_active_run(run_id)?;
        let path = b.path.clone();
        let base_avma = b.base_avma;
        let vmsize = b.vmsize;
        let is_executable = b.is_executable;
        let symbol_count = b.symbols.len();
        let symbols = b
            .symbols
            .into_iter()
            .map(|s| LiveSymbolOwned {
                start_svma: s.start_svma,
                end_svma: s.end_svma,
                name: s.name,
            })
            .collect();
        self.binaries.write().insert(LoadedBinary {
            path: b.path,
            base_avma: b.base_avma,
            avma_end: b.base_avma.saturating_add(b.vmsize),
            text_svma: b.text_svma,
            arch: b.arch,
            is_executable: b.is_executable,
            symbols,
            text_bytes: b.text_bytes,
        });
        if is_executable {
            tracing::info!(
                path = %path,
                base_avma = format_args!("{base_avma:#x}"),
                end = format_args!("{:#x}", base_avma.saturating_add(vmsize)),
                symbols = symbol_count,
                "stax-server: registered executable binary"
            );
        } else {
            tracing::debug!(
                path = %path,
                base_avma = format_args!("{base_avma:#x}"),
                end = format_args!("{:#x}", base_avma.saturating_add(vmsize)),
                symbols = symbol_count,
                "stax-server: registered binary"
            );
        }
        self.bump_revision();
        Ok(())
    }

    fn apply_binary_unloaded(&self, run_id: RunId, base_avma: u64) -> Result<(), String> {
        self.ensure_active_run(run_id)?;
        tracing::debug!(
            base_avma = format_args!("{base_avma:#x}"),
            "retaining unloaded binary mapping for historical samples"
        );
        self.bump_revision();
        Ok(())
    }

    /// Translate one ingest event into aggregator / binary-registry
    /// updates. Mirrors the in-process drainer in `stax-live::start`.
    fn apply_event(&self, run_id: RunId, event: IngestEvent) {
        // Update run summary counters first (under run-lock).
        {
            let mut inner = self.inner.lock();
            let Some(active) = inner.active.as_mut() else {
                return;
            };
            if active.id != run_id {
                return;
            }
            match &event {
                IngestEvent::Sample(_) => active.pet_samples += 1,
                IngestEvent::OffCpuInterval(_) => active.off_cpu_intervals += 1,
                IngestEvent::TargetAttached { pid, .. } => {
                    active.target_pid = Some(*pid);
                }
                _ => {}
            }
            self.telemetry
                .set_active_counts(active.pet_samples, active.off_cpu_intervals);
        }
        match event {
            IngestEvent::Sample(s) => {
                self.aggregator.write().record_pet_sample(
                    s.tid,
                    s.timestamp_ns,
                    &s.user_backtrace,
                    &s.kernel_backtrace,
                    PmuSample {
                        cycles: s.cycles,
                        instructions: s.instructions,
                        l1d_misses: s.l1d_misses,
                        branch_mispreds: s.branch_mispreds,
                    },
                );
                self.bump_revision();
            }
            IngestEvent::OnCpuInterval(i) => {
                self.aggregator.write().record_interval(
                    i.tid,
                    i.start_ns,
                    i.end_ns,
                    IntervalKind::OnCpu,
                );
                self.bump_revision();
            }
            IngestEvent::OffCpuInterval(i) => {
                let stack = i.stack.into_boxed_slice();
                let waker_user_stack = i.waker_user_stack.map(|s| s.into_boxed_slice());
                self.aggregator.write().record_interval(
                    i.tid,
                    i.start_ns,
                    i.end_ns,
                    IntervalKind::OffCpu {
                        stack,
                        waker_tid: i.waker_tid,
                        waker_user_stack,
                    },
                );
                self.bump_revision();
            }
            IngestEvent::Wakeup(w) => {
                self.aggregator.write().record_wakeup(
                    w.timestamp_ns,
                    w.waker_tid,
                    w.wakee_tid,
                    w.waker_user_stack,
                    w.waker_kernel_stack,
                );
                self.bump_revision();
            }
            IngestEvent::ThreadName { tid, name, .. } => {
                self.aggregator.write().set_thread_name(tid, name);
                self.bump_revision();
            }
            IngestEvent::BinaryLoaded(b) => {
                if let Err(e) = self.apply_binary_loaded(run_id, b) {
                    tracing::warn!("channel BinaryLoaded ignored: {e}");
                }
            }
            IngestEvent::BinaryUnloaded(b) => {
                if let Err(e) = self.apply_binary_unloaded(run_id, b.base_avma) {
                    tracing::warn!("channel BinaryUnloaded ignored: {e}");
                }
            }
            IngestEvent::TargetAttached { pid, task_port } => {
                if let Err(e) = self.apply_target_attached(run_id, pid, task_port) {
                    tracing::warn!("channel TargetAttached ignored: {e}");
                }
            }
            IngestEvent::ProbeResult(p) => {
                self.aggregator
                    .write()
                    .record_probe_result(stax_live::ProbeResultRecord {
                        tid: p.tid,
                        timing: p.timing.into(),
                        queue: p.queue.into(),
                        mach_pc: p.mach_pc,
                        mach_lr: p.mach_lr,
                        mach_fp: p.mach_fp,
                        mach_sp: p.mach_sp,
                        mach_walked: p.mach_walked.into_boxed_slice(),
                        compact_walked: p.compact_walked.into_boxed_slice(),
                        compact_dwarf_walked: p.compact_dwarf_walked.into_boxed_slice(),
                        dwarf_walked: p.dwarf_walked.into_boxed_slice(),
                        used_framehop: p.used_framehop,
                    });
                self.bump_revision();
            }
        }
    }

    fn finalize_run(&self, run_id: RunId, default_reason: StopReason) {
        self.terminal.lock().pending.remove(&run_id.0);
        let mut inner = self.inner.lock();
        let Some(active) = inner.active.as_ref() else {
            return;
        };
        if active.id != run_id {
            return;
        }
        let mut summary = inner.active.take().expect("checked above");
        // `stop_active` already set state + reason + timestamp. If
        // we got here naturally (channel closed because the
        // recorder finished on its own), fill them in.
        if summary.state != RunState::Stopped {
            summary.state = RunState::Stopped;
            summary.stop_reason = Some(default_reason);
            summary.stopped_at_unix_ns = Some(now_unix_ns());
        }
        inner.cancel = None;
        // Run is over → the shade slot is no longer claimable.
        inner.active_shade = None;
        inner.active_shade_commands = None;
        inner.ingest_attached = false;
        tracing::info!(
            "stax-server: run {} stopped after {} samples / {} intervals",
            summary.id.0,
            summary.pet_samples,
            summary.off_cpu_intervals
        );
        self.telemetry.runs_stopped.inc(1);
        self.telemetry.active_run_id.set(0);
        self.telemetry
            .set_active_counts(summary.pet_samples, summary.off_cpu_intervals);
        self.telemetry
            .run_phase
            .enter("stopped", format!("run_id={}", summary.id.0));
        self.telemetry
            .registry
            .event("run.stopped", format!("run_id={}", summary.id.0));
        inner.history.push(summary);
        drop(inner); // release before reaping
        self.reap_shade_child();
    }
}
