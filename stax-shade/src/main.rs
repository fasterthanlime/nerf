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
//! lives here. It's codesigned with `com.apple.security.cs.debugger`
//! at install time so it can acquire those ports without sudo.
//!
//! Isolating that capability matters for two reasons:
//!
//! 1. **Failure containment.** A crash in the unwinder, a
//!    misaligned write, or a bad exception-port dance shouldn't
//!    take down the run registry / aggregator (`stax-server`) or
//!    the kperf owner (`staxd`). One target = one shade = one
//!    blast radius.
//! 2. **Surface reduction.** `stax` (CLI) and `stax-server` no
//!    longer need `cs.debugger`. They're plain unprivileged
//!    user-space processes.
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
//!   sets up whatever it needs (kperf via stax-server → staxd,
//!   framehop unwinder, breakpoints if requested), then resumes
//!   the target. Never miss an event.
//!
//! ## What this binary does *today*
//!
//! Stage A (this commit): scaffolding only. Parses args, opens the
//! Mach task port (or reports the entitlement failure), logs, idles
//! waiting for stdin EOF or SIGTERM, exits cleanly. No vox
//! protocol, no framehop, no peek/poke yet — those land in
//! follow-up commits on top of this skeleton.

#![cfg(target_os = "macos")]

use std::process::ExitCode;

use facet::Facet;
use figue as args;
use stax_shade_proto::{ShadeAck, ShadeCapabilities, ShadeInfo, ShadeRegistryClient};

#[derive(Facet, Debug)]
struct Cli {
    #[facet(flatten)]
    builtins: args::FigueBuiltins,

    /// Attach to a running process by PID.
    #[facet(args::named, default)]
    attach: Option<u32>,

    /// Local socket path of the spawning stax-server. Reserved for
    /// the vox session that lands in stage B.
    #[facet(args::named, default)]
    server_socket: Option<String>,

    /// Run id (assigned by stax-server) this attachment belongs to.
    /// Reserved for stage B.
    #[facet(args::named, default)]
    run_id: Option<u64>,

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

    if let Err(e) = run(cli).await {
        tracing::error!("stax-shade failed: {e:?}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(cli: Cli) -> eyre::Result<()> {
    let pid = match (cli.attach, cli.launch, cli.command.first()) {
        (Some(pid), false, _) => pid,
        (None, true, Some(_)) => {
            // Stage B doesn't implement the launch path yet — left
            // as a hard error so we never silently fall through.
            // Implementation sketch:
            // posix_spawnattr_setflags(POSIX_SPAWN_START_SUSPENDED),
            // posix_spawn, task_for_pid on the freshly-spawned pid,
            // set up exception ports if branch-stepping is enabled,
            // then task_resume.
            eyre::bail!("--launch path is unimplemented (stages A/B)")
        }
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

    let task = task_for_pid(pid)?;
    tracing::info!(pid, task_port = task, "attached to existing process");

    // If a server socket was provided, dial in and register so the
    // server knows we're up and which run we belong to. When
    // there's no socket (manual smoke-test invocation) we just
    // park, same as stage A.
    if let (Some(socket), Some(run_id)) = (cli.server_socket.as_deref(), cli.run_id) {
        register_with_server(socket, run_id, pid).await?;
    } else {
        tracing::warn!(
            "no --server-socket / --run-id; running standalone — \
             stax-server will not learn about this attachment"
        );
    }

    park_until_signal().await;
    Ok(())
}

async fn register_with_server(socket: &str, run_id: u64, target_pid: u32) -> eyre::Result<()> {
    let url = format!("local://{socket}");
    let client: ShadeRegistryClient = vox::connect(&url).await?;
    let info = ShadeInfo {
        run_id,
        target_pid,
        shade_pid: std::process::id(),
        capabilities: ShadeCapabilities {
            // Truthful for stage B: we have neither walker nor
            // peek/poke wired yet, just the registration handshake.
            // Each capability flips to true in the commit that
            // implements its server-callable surface.
            peek: false,
            poke: false,
            framehop_walker: false,
            breakpoint_step: false,
        },
    };
    match client.register_shade(info).await {
        Ok(ShadeAck { accepted: true, .. }) => {
            tracing::info!(run_id, "registered with stax-server");
            Ok(())
        }
        Ok(ShadeAck { accepted: false, reason }) => {
            eyre::bail!(
                "stax-server rejected registration: {}",
                reason.unwrap_or_else(|| "(no reason)".to_owned())
            )
        }
        Err(vox::VoxError::User(msg)) => eyre::bail!("server returned error: {msg}"),
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
             (is stax-shade codesigned with com.apple.security.cs.debugger? \
             try `cargo xtask install`)"
        );
    }
    Ok(task)
}

/// Idle until SIGINT/SIGTERM (or stdin closes). Stage C will
/// replace this with awaiting on the vox session's `closed()`
/// future once the server can actually call into `Shade` and
/// drive a real teardown.
async fn park_until_signal() {
    let stdin_close = tokio::task::spawn_blocking(|| {
        use std::io::Read;
        let mut sink = [0u8; 1];
        let _ = std::io::stdin().read(&mut sink);
    });
    tokio::select! {
        _ = stdin_close => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stax_shade=info"));

    let oslog = tracing_oslog::OsLogger::new("eu.bearcove.stax-shade", "default");

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(oslog)
        .init();
}
