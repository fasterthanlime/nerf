use facet::Facet;
use figue as args;

pub enum TargetProcess {
    ByPid(u32),
    Launch { program: String, args: Vec<String> },
}

/// stax — live profiler frontend that drives the staxd daemon backend
/// over a local socket and streams aggregated samples over WebSocket.
#[derive(Facet, Debug)]
pub struct Cli {
    #[facet(args::subcommand)]
    pub command: Command,

    #[facet(flatten)]
    pub builtins: args::FigueBuiltins,
}

#[derive(Facet, Debug)]
#[repr(u8)]
pub enum Command {
    /// Record live profiling data, streamed over `--serve`.
    Record(RecordArgs),

    /// Codesign this stax binary (or, when run as root, install staxd
    /// as a LaunchDaemon).
    Setup(SetupArgs),
}

#[derive(Facet, Debug)]
pub struct RecordArgs {
    /// PET sampling frequency, in Hz.
    #[facet(args::named, args::short = 'F', default = 900)]
    pub frequency: u32,

    /// Stop sampling after this many seconds. Unlimited by default
    /// (Ctrl-C to stop).
    #[facet(args::named, args::short = 'l', default)]
    pub time_limit: Option<u64>,

    /// Profile an existing process by PID instead of launching one.
    #[facet(args::named, args::short = 'p', default)]
    pub pid: Option<u32>,

    /// Start a live RPC/WebSocket server on the given host:port (e.g.
    /// 127.0.0.1:8080).
    #[facet(args::named, default)]
    pub serve: Option<String>,

    /// Local socket path of the running `staxd` daemon. Defaults to the
    /// path `sudo stax setup` installs.
    #[facet(args::named, default = "/var/run/staxd.sock")]
    pub daemon_socket: String,

    /// Command to launch and profile. Use `--` to keep the target's
    /// flags from being interpreted by stax:
    ///
    ///     stax record -- /bin/foo --some-flag bar baz
    #[facet(args::positional, default)]
    pub command: Vec<String>,
}

impl RecordArgs {
    pub fn target(&self) -> Result<TargetProcess, String> {
        match (self.pid, self.command.split_first()) {
            (Some(_), Some(_)) => {
                Err("specify either --pid or a command to launch, not both".to_owned())
            }
            (Some(pid), None) => Ok(TargetProcess::ByPid(pid)),
            (None, Some((program, rest))) => Ok(TargetProcess::Launch {
                program: program.clone(),
                args: rest.to_vec(),
            }),
            (None, None) => Err("specify either --pid <PID> or a command to launch".to_owned()),
        }
    }
}

#[derive(Facet, Debug)]
pub struct SetupArgs {
    /// Skip the confirmation prompt before running `codesign`.
    #[facet(args::named, args::short = 'y', default)]
    pub yes: bool,
}
