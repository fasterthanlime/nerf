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
    /// Record live profiling data. Forwards events to the running
    /// `stax-server` for the web UI and `stax {top,annotate,…}` to
    /// query.
    Record(RecordArgs),

    /// Codesign this stax binary (or, when run as root, install staxd
    /// as a LaunchDaemon).
    Setup(SetupArgs),

    /// Print the current state of stax-server (active run + history).
    Status,

    /// List every run stax-server has hosted (active + history).
    List,

    /// Dump stax-server diagnostics: telemetry phases, counters,
    /// histograms, and recent events.
    Diagnose,

    /// Ask running stax processes to dump SIGUSR1 telemetry/debug
    /// snapshots into unified logging.
    Dump,

    /// Block until a condition fires, the active run stops, or the
    /// timeout elapses.
    Wait(WaitArgs),

    /// Ask stax-server to stop the active run cleanly.
    Stop,

    /// Snapshot the top-N functions from the active run.
    Top(TopArgs),

    /// Disassemble + annotate a function from the active run.
    Annotate(AnnotateArgs),

    /// Print the on-CPU flamegraph as an indented tree.
    Flame(FlameArgs),

    /// Per-thread on/off-CPU breakdown for the active run.
    Threads(ThreadsArgs),

    /// One snapshot of the kperf-vs-probe diff: pairs each PET
    /// sample with the matching race-against-return probe result,
    /// prints suffix-match histogram, drift histogram, and the most
    /// recent N entries with both stacks symbolicated.
    ProbeDiff(ProbeDiffArgs),
}

#[derive(Facet, Debug)]
pub struct WaitArgs {
    /// Stop waiting after at least this many PET samples have landed.
    /// Mutually exclusive with --for-seconds and --until-symbol.
    #[facet(args::named, default)]
    pub for_samples: Option<u64>,

    /// Stop waiting after this many seconds, even if the run is still
    /// recording. Mutually exclusive with --for-samples and
    /// --until-symbol.
    #[facet(args::named, default)]
    pub for_seconds: Option<u64>,

    /// Stop waiting once a symbol containing this substring has been
    /// observed (case-sensitive). Mutually exclusive with the others.
    #[facet(args::named, default)]
    pub until_symbol: Option<String>,

    /// Hard deadline for the whole wait, in milliseconds. Returns
    /// `TimedOut` if exceeded.
    #[facet(args::named, default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Facet, Debug)]
pub struct TopArgs {
    /// Maximum number of entries to return.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,

    /// Sort by `self` (leaf) or `total` (any frame). Default: `self`.
    #[facet(args::named, default = "self")]
    pub sort: String,

    /// Filter to one thread by tid. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
}

#[derive(Facet, Debug)]
pub struct ThreadsArgs {
    /// Maximum number of threads to print (sorted by on-CPU
    /// descending). 0 to print all.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,
}

#[derive(Facet, Debug)]
pub struct ProbeDiffArgs {
    /// Restrict to a single thread. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,

    /// How many "recent paired entries" to print at the end.
    /// 0 prints just the histograms.
    #[facet(args::named, args::short = 'n', default = 8)]
    pub recent: u32,
}

#[derive(Facet, Debug)]
pub struct FlameArgs {
    /// Maximum tree depth to print. The flamegraph the server
    /// returns is unbounded; this just controls how deep the CLI
    /// prints (children below the cut-off are summarised as
    /// `…<N more frames>`).
    #[facet(args::named, args::short = 'd', default = 12)]
    pub max_depth: usize,

    /// Hide nodes whose share of the total on-CPU time falls
    /// below this percent. `0` to print everything.
    #[facet(args::named, default = 1.0)]
    pub threshold_pct: f64,

    /// Filter to one thread by tid. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
}

#[derive(Facet, Debug)]
pub struct AnnotateArgs {
    /// Function to annotate. Either a hex address (`0x10004ad60`)
    /// or a substring of the demangled symbol name; the substring
    /// is matched against the current run's top-N leaf samples and
    /// the hottest match wins. Operates on whichever run the
    /// server has active (or last finished); historical runs by
    /// id are not yet addressable.
    #[facet(args::positional)]
    pub target: String,

    /// Filter to one thread by tid. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
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

    /// For each parsed PET sample, have stax-shade suspend that
    /// sampled thread and emit a paired user-stack probe. This is an
    /// evaluation mode for kperf/probe stitching latency, not the
    /// default profiler path.
    #[facet(args::named, default)]
    pub race_kperf: bool,

    /// Evaluation mode: independently sample shade-side user stacks
    /// at `--correlate-frequency` (or PET frequency by default),
    /// then correlate with nearest kperf samples by `(tid, timestamp)`.
    #[facet(args::named, default)]
    pub correlate_kperf: bool,

    /// Total process-wide shade-side probe frequency for
    /// `--correlate-kperf`. Defaults to `--frequency`.
    #[facet(args::named, default)]
    pub correlate_frequency: Option<u32>,

    /// Profile an existing process by PID instead of launching one.
    #[facet(args::named, args::short = 'p', default)]
    pub pid: Option<u32>,

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
