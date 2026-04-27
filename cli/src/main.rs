use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::process::exit;

use figue as args;
use stax_core::{args::{Cli, Command, RecordArgs}, cmd_record_mac, cmd_setup_mac};

fn main_impl() -> Result<(), Box<dyn Error>> {
    if env::var("RUST_LOG").is_err() {
        // cranelift_jit/cranelift_codegen log every JIT'd function at info,
        // which floods the terminal once we start the live RPC server.
        env::set_var("RUST_LOG", "info,cranelift_jit=warn,cranelift_codegen=warn");
    }

    env_logger::init();

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
    }
    Ok(())
}

fn main() {
    if let Err(error) = main_impl() {
        eprintln!("error: {error}");
        exit(1);
    }
}

fn run_record(args: RecordArgs) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Two ways the recorder's LiveSink can land somewhere queryable:
    //   1. `--serve <addr>` → spin up an in-process WS aggregator
    //      (legacy path; the all-in-one mode we've shipped so far).
    //   2. otherwise → forward to a running stax-server over its
    //      local socket, so agents can query via RunControl /
    //      Profiler. Falls through silently if stax-server isn't
    //      running (recording proceeds but nothing aggregates yet —
    //      eventually we'll surface a "no sink wired" warning).
    let (live_sink, _forwarder): (Option<Box<dyn stax_core::live_sink::LiveSink>>, _) =
        if let Some(ref addr) = args.serve {
            let (sink, _server_handle) = runtime.block_on(stax_live::start(addr))?;
            (Some(Box::new(sink)), None)
        } else if let Some(socket) = stax_server_socket() {
            match runtime.block_on(connect_to_server(&socket, &args)) {
                Ok((id, sink, fwd)) => {
                    eprintln!("stax: registered run {} with stax-server at {}", id.0, socket.display());
                    (Some(Box::new(sink)), Some(fwd))
                }
                Err(e) => {
                    eprintln!("stax: stax-server unreachable ({e}); recording without a sink");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

    let result = cmd_record_mac::main_with_live_sink(args, live_sink);
    // Drop the runtime explicitly so the forwarder task gets cancelled
    // before we exit; otherwise it can race with process teardown and
    // log noisy errors about the vox transport disappearing.
    drop(runtime);
    result
}

/// Resolve the stax-server socket the same way the daemon picks it on
/// startup: `STAX_SERVER_SOCKET` env override, else
/// `$XDG_RUNTIME_DIR/stax-server.sock`, else `/tmp/stax-server-$UID.sock`.
/// Returns `None` if no candidate path actually exists on disk.
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

async fn connect_to_server(
    socket: &std::path::Path,
    args: &RecordArgs,
) -> eyre::Result<(
    stax_live_proto::RunId,
    stax_core::ingest_sink::IngestSink,
    tokio::task::JoinHandle<()>,
)> {
    let label = args
        .command
        .first()
        .cloned()
        .unwrap_or_else(|| match args.pid {
            Some(p) => format!("pid {p}"),
            None => "(unnamed)".to_owned(),
        });
    let config = stax_live_proto::RunConfig {
        label,
        frequency_hz: args.frequency,
    };
    stax_core::ingest_sink::connect_and_register(&socket.to_string_lossy(), config).await
}
