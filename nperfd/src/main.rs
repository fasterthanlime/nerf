//! `nperfd` — root daemon for nperf.
//!
//! Listens on a vox local (Unix-domain) socket; each connection gets
//! to call `status()` freely and `record()` once. The daemon owns
//! kperf + kdebug + kpc and streams the raw kdebug ringbuffer back to
//! the client, who runs the parser, builds samples, symbolicates, and
//! drives whatever UI / archive sink it wants.
//!
//! kperf is single-owner across the whole machine. We mirror that
//! constraint at the daemon level: one active `record()` session at a
//! time, second caller is refused with `RecordError::Busy`. We do
//! *not* try to coordinate across daemons (Instruments / xctrace
//! still steals ktrace if you launch them) — we just surface the
//! eviction to the in-flight client cleanly.
//!
//! v0 scope: trusts the connection's peer (no SO_PEERCRED check yet,
//! TODO). The deployment story is the LaunchDaemon plist locking the
//! socket down to a known group, so even without per-call uid checks
//! the door is bouncered. Real per-call auth is one of the first
//! follow-ups.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use eyre::{Context, Result};
use tokio::sync::Mutex;
use tracing::{info, warn};

use nperfd_proto::{
    DaemonStatus, KdBufBatch, Nperfd, NperfdDispatcher, RecordError, RecordSummary,
    SessionConfig, SessionState,
};

mod session;

/// Default socket path. Production deployments pass their own via
/// `--socket` (the LaunchDaemon plist normally puts it under
/// `/var/run/`). The default is `/tmp/` for hand-running during
/// development; root daemon, but no permission gymnastics required.
const DEFAULT_SOCKET: &str = "/tmp/nperfd.sock";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nperfd=info,nerf_mac_kperf_sys=info".into()),
        )
        .init();

    let socket_path = parse_socket_arg();

    if socket_path.exists() {
        // Stale socket from a crashed previous run. Vox's bind would
        // fail otherwise. Safe to remove because we're the daemon and
        // we own this path.
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }

    let server = NperfdServer::new();
    let dispatcher = NperfdDispatcher::new(server);

    let listener = vox::transport::local::LocalLinkAcceptor::bind(
        socket_path.to_string_lossy().into_owned(),
    )
    .with_context(|| format!("binding {}", socket_path.display()))?;
    info!("nperfd listening on local://{}", socket_path.display());

    // Best-effort permissive perms for development. Production via
    // launchd uses the plist's Sockets dict so we don't run this
    // branch.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(
        &socket_path,
        std::fs::Permissions::from_mode(0o666),
    );

    let serve = tokio::spawn(async move {
        if let Err(e) = vox::serve_listener(listener, dispatcher).await {
            warn!("vox serve_listener exited: {e}");
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("nperfd: SIGINT, shutting down"),
        r = serve => {
            if let Err(e) = r { warn!("serve task panicked: {e}"); }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

fn parse_socket_arg() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket"
            && let Some(p) = args.next()
        {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(DEFAULT_SOCKET)
}

/// Single-active-session daemon state. The mutex serialises both
/// session bookkeeping and the actual kperf run; we hold it across
/// the whole `record()` body so a second caller can't sneak in even
/// during teardown.
#[derive(Clone)]
struct NperfdServer {
    session: Arc<Mutex<Option<SessionInfo>>>,
}

#[derive(Clone)]
struct SessionInfo {
    target_pid: u32,
    started_at_unix_ns: u64,
}

impl NperfdServer {
    fn new() -> Self {
        Self { session: Arc::new(Mutex::new(None)) }
    }
}

impl Nperfd for NperfdServer {
    async fn status(&self) -> DaemonStatus {
        let state = match self.session.lock().await.as_ref() {
            None => SessionState::Idle,
            Some(s) => SessionState::Recording {
                target_pid: s.target_pid,
                holder_uid: 0, // TODO: track when SO_PEERCRED is wired
                holder_pid: 0,
                since_unix_ns: s.started_at_unix_ns,
            },
        };
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            state,
            host_arch: host_arch().to_string(),
        }
    }

    async fn record(
        &self,
        config: SessionConfig,
        records: vox::Tx<KdBufBatch>,
    ) -> Result<RecordSummary, RecordError> {
        // Try to claim the single session slot. We don't `.await`
        // anything between the check and the insert, so there's no
        // race window where two clients both think they're the holder.
        let mut guard = self.session.lock().await;
        if let Some(holder) = guard.as_ref() {
            return Err(RecordError::Busy {
                holder_uid: 0,
                holder_pid: 0,
                since_unix_ns: holder.started_at_unix_ns,
            });
        }
        let started_at_unix_ns = unix_ns_now();
        *guard = Some(SessionInfo { target_pid: config.target_pid, started_at_unix_ns });
        drop(guard);

        info!(
            "record session start pid={} freq={}Hz buf_records={}",
            config.target_pid, config.frequency_hz, config.buf_records
        );

        let result = session::run(config, records).await;

        // Always release the slot. The session driver tore down kperf+
        // kdebug on its own when it returned; here we just release the
        // per-daemon "someone is recording" flag so the next caller
        // can try.
        *self.session.lock().await = None;
        info!(
            "record session end: {:?}",
            result.as_ref().map(|s| s.records_drained)
        );
        result
    }
}

fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
}

fn unix_ns_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
