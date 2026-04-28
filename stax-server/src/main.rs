//! `stax-server` — long-running unprivileged daemon.
//!
//! Hosts the run registry (one active + history) plus the live
//! aggregator + binary registry. Three vox services are exposed over
//! the local socket:
//!
//! - `RunControl` — agent-facing lifecycle (status / wait / stop / list).
//! - `RunIngest` — recorder pushes IngestEvents into the active run.
//! - `Profiler`  — query the live aggregator (top, flamegraph, annotate, …).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use stax_live::source::SourceResolver;
use stax_live::{
    Aggregator, BinaryRegistry, IntervalKind, LiveServer, LiveSymbolOwned, LoadedBinary, PmuSample,
};
use stax_live_proto::{
    IngestEvent, ProfilerDispatcher, RunConfig, RunControl, RunControlDispatcher, RunId, RunIngest,
    RunIngestDispatcher, RunState, RunSummary, ServerStatus, StopReason, WaitCondition,
    WaitOutcome,
};
use stax_shade_proto::{ShadeAck, ShadeInfo, ShadeRegistry, ShadeRegistryDispatcher};
use vox::VoxListener;

const DEFAULT_SOCK_NAME: &str = "stax-server.sock";
const DEFAULT_WS_BIND: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    init_logging();

    let socket = resolve_socket_path();
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }

    let server = ServerState::new(socket.clone());
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
                    connection.handle_with(RunIngestDispatcher::new(server.clone()));
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
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .non_resumable()
            .keepalive(vox::SessionKeepaliveConfig {
                ping_interval: std::time::Duration::from_secs(5),
                pong_timeout: std::time::Duration::from_secs(30),
            })
            .on_connection(factory)
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => client.caller.closed().await,
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
    tokio::spawn(async move {
        let result = vox::acceptor_on(link)
            .on_connection(factory)
            .establish::<vox::NoopClient>()
            .await;
        match result {
            Ok(client) => client.caller.closed().await,
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
    source: Arc<Mutex<SourceResolver>>,
    paused: Arc<AtomicBool>,
    started_at_unix_ns: u64,
    next_run_id: Arc<AtomicU64>,
    /// Local-socket path the server is listening on. Used to tell
    /// auto-spawned stax-shade processes how to dial back in.
    socket_path: Arc<PathBuf>,
}

struct Inner {
    active: Option<RunSummary>,
    /// Notify that wakes the active run's drainer task so it drops
    /// its `Rx<IngestEvent>`. Set when a run starts, cleared by
    /// `stop_active` (which then notifies, and by the drainer
    /// itself when its Rx closes naturally).
    cancel: Option<Arc<tokio::sync::Notify>>,
    /// Currently-attached shade, if any. Set by `register_shade`
    /// when stax-shade dials in; cleared when its session closes.
    /// One active run = at most one shade.
    active_shade: Option<ShadeInfo>,
    /// Process handle for the auto-spawned stax-shade child. Set
    /// when `apply_event` sees `TargetAttached` and there's no
    /// shade yet; reaped + cleared by `stop_active` /
    /// `finalize_run` (or when the child exits on its own and the
    /// session-close cleanup fires). One active run = at most one
    /// child.
    shade_child: Option<std::process::Child>,
    history: Vec<RunSummary>,
}

impl ServerState {
    fn new(socket_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                cancel: None,
                active_shade: None,
                shade_child: None,
                history: Vec::new(),
            })),
            aggregator: Arc::new(RwLock::new(Aggregator::default())),
            binaries: Arc::new(RwLock::new(BinaryRegistry::new())),
            source: Arc::new(Mutex::new(SourceResolver::new())),
            paused: Arc::new(AtomicBool::new(false)),
            started_at_unix_ns: now_unix_ns(),
            next_run_id: Arc::new(AtomicU64::new(1)),
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
            source: self.source.clone(),
            paused: self.paused.clone(),
        }
    }

    /// Validate + record a shade registration. Used by
    /// `ShadeRegistryImpl` (per-session wrapper) so the per-session
    /// `ShadeSlot` can be populated atomically with the
    /// server-side `active_shade`.
    fn try_register_shade(&self, info: ShadeInfo) -> ShadeAck {
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
        }
    }

    /// Spawn a `stax-shade` child process to attach to `pid`.
    /// Stores the `Child` handle on `Inner::shade_child` so we
    /// can reap on cleanup. Best-effort: if the binary isn't
    /// found or `Command::spawn` fails we log and move on. The
    /// shade will dial back in on its own and call
    /// `register_shade`, at which point `active_shade` gets
    /// populated.
    fn spawn_shade(&self, run_id: RunId, pid: u32) {
        let bin = match resolve_shade_path() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "stax-shade binary not found in any of the candidate \
                     locations; target-side helpers are disabled for this run. \
                     `cargo xtask install` should land it in ~/.cargo/bin/."
                );
                return;
            }
        };
        let socket = self.socket_path.to_string_lossy().into_owned();
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--attach")
            .arg(pid.to_string())
            .arg("--server-socket")
            .arg(&socket)
            .arg("--run-id")
            .arg(run_id.0.to_string())
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
                tracing::info!(
                    run_id = run_id.0,
                    pid,
                    shade_pid = child.id(),
                    bin = %bin.display(),
                    "spawned stax-shade"
                );
                self.inner.lock().shade_child = Some(child);
            }
            Err(e) => {
                tracing::warn!(
                    "failed to spawn {}: {e}; target-side helpers are disabled for this run",
                    bin.display()
                );
            }
        }
    }

    /// Reap the shade child (if any), preferring a clean exit but
    /// killing if it's still running after a brief grace period.
    /// Called from `stop_active` and `finalize_run`. The shade is
    /// supposed to notice its vox session close on its own; this
    /// is the belt-and-suspenders.
    fn reap_shade_child(&self) {
        let mut child = match self.inner.lock().shade_child.take() {
            Some(c) => c,
            None => return,
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
        // Give it a moment to notice the session close on its
        // own. The shade's session is closed by the accept loop
        // when our end of the vox connection drops — that should
        // happen as soon as `active_shade` clears and we drop our
        // refs. But polling here is simpler than orchestrating
        // shutdown signals.
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
        }
        tracing::warn!(
            shade_pid = child.id(),
            "stax-shade didn't exit within 1s of run end; sending SIGTERM"
        );
        let _ = child.kill();
        let _ = child.wait();
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
                    }
                }
            }
        });
    }
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
/// 2. The standard `fmt` layer (stderr), useful when running in
///    foreground for development.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax_server=info"));

    let oslog = tracing_oslog::OsLogger::new("eu.bearcove.stax-server", "default");

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(oslog)
        .init();
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
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

    async fn stop_active(&self) -> Result<RunSummary, String> {
        // Mark as stopped + grab the cancel handle under the lock,
        // then notify outside the lock so the drainer can run
        // freely. The drainer is responsible for moving the run
        // from `active` to `history` once its Rx is closed; we
        // return the snapshot we just produced so the caller has
        // something to print without waiting on the recorder.
        let (snapshot, cancel) = {
            let mut inner = self.inner.lock();
            let snapshot = match inner.active.as_mut() {
                Some(summary) => {
                    summary.state = RunState::Stopped;
                    summary.stop_reason = Some(StopReason::UserStop);
                    summary.stopped_at_unix_ns = Some(now_unix_ns());
                    summary.clone()
                }
                None => return Err("no active run".to_owned()),
            };
            // The shade has nothing left to attach to; release
            // the slot so a follow-up run on the same server can
            // start a fresh shade. The shade process itself is
            // tolerated until its session-close cleanup fires (or
            // the liveness watchdog notices it died). It would be
            // tidier to actively detach via the Shade trait once
            // we have a shutdown method on it; for now best-effort.
            inner.active_shade = None;
            (snapshot, inner.cancel.take())
        };
        if let Some(cancel) = cancel {
            cancel.notify_waiters();
        }
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
    async fn register_shade(&self, info: ShadeInfo) -> Result<ShadeAck, String> {
        let ack = self.server.try_register_shade(info.clone());
        if ack.accepted {
            *self.shade_slot.lock() = Some(info.shade_pid);
        }
        Ok(ack)
    }
}

impl RunIngest for ServerState {
    async fn start_run(
        &self,
        config: RunConfig,
        mut events: vox::Rx<IngestEvent>,
    ) -> Result<RunId, String> {
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
        }

        // Reset aggregator + binary registry for this run. Historical
        // queries against previous runs are deferred (Profiler doesn't
        // take a RunId yet).
        *self.aggregator.write() = Aggregator::default();
        *self.binaries.write() = BinaryRegistry::new();
        // Re-attach the host's dyld shared cache. Without this every
        // cache-resident address (libsystem_*, CoreFoundation, dyld,
        // …) shows as <unmapped> after the first run of the server's
        // lifetime, since the previous attach lived on the old
        // BinaryRegistry instance we just replaced.
        self.attach_local_shared_cache();

        tracing::info!(
            "stax-server: run {} started (frequency_hz={})",
            id.0,
            config.frequency_hz
        );

        let state = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.notified() => break,
                    recv = events.recv() => match recv {
                        Ok(Some(event_sref)) => {
                            let state = state.clone();
                            let _ = event_sref.map(|event| {
                                state.apply_event(id, event);
                            });
                        }
                        _ => break,
                    },
                }
            }
            // Drop the Rx explicitly so the recorder's Tx::send
            // surfaces an error and its forwarder flips
            // stop_requested → recorder bails out of drive_session
            // → the whole pipeline cascades down.
            drop(events);
            state.finalize_run(id, StopReason::TargetExited);
        });

        Ok(id)
    }
}

impl ServerState {
    /// Translate one ingest event into aggregator / binary-registry
    /// updates. Mirrors the in-process drainer in `stax-live::start`.
    fn apply_event(&self, run_id: RunId, event: IngestEvent) {
        // Update run summary counters first (under run-lock). If
        // this is a TargetAttached we may also need to spawn the
        // shade — defer that to *after* the lock so the spawn
        // syscall (fork+exec) doesn't block other lock takers.
        let mut spawn_shade_for_pid: Option<u32> = None;
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
                    if inner.shade_child.is_none() && inner.active_shade.is_none() {
                        spawn_shade_for_pid = Some(*pid);
                    }
                }
                _ => {}
            }
        }
        if let Some(pid) = spawn_shade_for_pid {
            self.spawn_shade(run_id, pid);
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
            }
            IngestEvent::OnCpuInterval(i) => {
                self.aggregator.write().record_interval(
                    i.tid,
                    i.start_ns,
                    i.end_ns,
                    IntervalKind::OnCpu,
                );
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
            }
            IngestEvent::Wakeup(w) => {
                self.aggregator.write().record_wakeup(
                    w.timestamp_ns,
                    w.waker_tid,
                    w.wakee_tid,
                    w.waker_user_stack,
                    w.waker_kernel_stack,
                );
            }
            IngestEvent::ThreadName { tid, name, .. } => {
                self.aggregator.write().set_thread_name(tid, name);
            }
            IngestEvent::BinaryLoaded(b) => {
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
            }
            IngestEvent::BinaryUnloaded { base_avma, .. } => {
                self.binaries.write().remove(base_avma);
            }
            IngestEvent::TargetAttached { pid, task_port } => {
                self.binaries.write().set_target(pid, task_port);
            }
            IngestEvent::ProbeResult(p) => {
                self.aggregator
                    .write()
                    .record_probe_result(stax_live::ProbeResultRecord {
                        tid: p.tid,
                        kperf_ts: p.kperf_ts_ns,
                        probe_done_ns: p.probe_done_ns,
                        mach_pc: p.mach_pc,
                        mach_lr: p.mach_lr,
                        mach_fp: p.mach_fp,
                        mach_sp: p.mach_sp,
                        mach_walked: p.mach_walked.into_boxed_slice(),
                        used_framehop: p.used_framehop,
                    });
            }
        }
    }

    fn finalize_run(&self, run_id: RunId, default_reason: StopReason) {
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
        tracing::info!(
            "stax-server: run {} stopped after {} samples / {} intervals",
            summary.id.0,
            summary.pet_samples,
            summary.off_cpu_intervals
        );
        inner.history.push(summary);
        drop(inner); // release before reaping
        self.reap_shade_child();
    }
}
