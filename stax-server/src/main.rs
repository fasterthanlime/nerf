//! `stax-server` — long-running unprivileged daemon.
//!
//! Owns the run registry (one active + history) and serves a vox
//! local-socket transport. Agents (`stax status`, `stax wait`, …) and
//! the recorder (in a follow-up commit) connect here.
//!
//! Future commits will add the WS transport for the web UI and a
//! `RunIngest` service for the recorder to stream sample events into.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use stax_live_proto::{
    RunControl, RunControlDispatcher, RunSummary, ServerStatus, WaitCondition, WaitOutcome,
};

const DEFAULT_SOCK_NAME: &str = "stax-server.sock";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "info,stax_server=info");
        }
    }
    env_logger::init();

    let socket = resolve_socket_path();
    if socket.exists() {
        // Stale socket from a previous run; safe to remove since we
        // own the path.
        std::fs::remove_file(&socket)?;
    }

    let server = ServerState::new();

    let listener = vox::transport::local::LocalLinkAcceptor::bind(
        socket.to_string_lossy().into_owned(),
    )?;
    log::info!("stax-server listening on local://{}", socket.display());

    // Permissive perms for now; tighten when we know who else needs
    // to talk to us.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));

    loop {
        let link = match listener.accept().await {
            Ok(l) => l,
            Err(e) => {
                log::warn!("stax-server: accept failed: {e}");
                continue;
            }
        };
        let server = server.clone();
        tokio::spawn(async move {
            let dispatcher = RunControlDispatcher::new(server);
            let result = vox::acceptor_on(link)
                .non_resumable()
                .on_connection(dispatcher)
                .establish::<vox::NoopClient>()
                .await;
            match result {
                Ok(client) => client.caller.closed().await,
                Err(e) => log::warn!("stax-server: session establish failed: {e:?}"),
            }
        });
    }
}

/// Pick the socket path. `STAX_SERVER_SOCKET` overrides; otherwise
/// `$XDG_RUNTIME_DIR/stax-server.sock` if set, falling back to
/// `/tmp/stax-server-$UID.sock`.
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

#[derive(Clone)]
struct ServerState {
    inner: Arc<Mutex<Inner>>,
    started_at_unix_ns: u64,
}

struct Inner {
    /// `None` while no run is in progress.
    active: Option<RunSummary>,
    /// Historical runs, oldest first. Bounded by an eviction policy
    /// in a follow-up; for now it grows unbounded for the duration of
    /// the server process.
    history: Vec<RunSummary>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                history: Vec::new(),
            })),
            started_at_unix_ns: now_unix_ns(),
        }
    }
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
            active: inner.active.clone(),
            server_started_at_unix_ns: self.started_at_unix_ns,
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

    async fn wait_active(
        &self,
        condition: WaitCondition,
        timeout_ms: Option<u64>,
    ) -> WaitOutcome {
        // Without a real run registry yet, this only honours the
        // "no active run" / "timed out" cases. The substantive logic
        // (sample-count thresholds, symbol watchers) lands once the
        // recorder→server ingest is wired in.
        let active = self.inner.lock().active.clone();
        let Some(summary) = active else {
            return WaitOutcome::NoActiveRun;
        };
        let deadline_ms = match (&condition, timeout_ms) {
            (WaitCondition::ForSeconds { seconds }, _) => Some(seconds * 1000),
            (_, Some(ms)) => Some(ms),
            _ => None,
        };
        if let Some(ms) = deadline_ms {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        WaitOutcome::TimedOut { summary }
    }

    async fn stop_active(&self) -> Result<RunSummary, String> {
        let mut inner = self.inner.lock();
        match inner.active.take() {
            Some(summary) => {
                inner.history.push(summary.clone());
                Ok(summary)
            }
            None => Err("no active run".to_owned()),
        }
    }
}
