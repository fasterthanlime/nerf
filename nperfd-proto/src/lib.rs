//! Wire schema for the nperfd RPC.
//!
//! The deliberate design choice: this protocol carries kdebug records
//! verbatim. `KdBufWire` is field-for-field identical to xnu's
//! `kd_buf`, and `SessionConfig` is just the parameters our recorder
//! already passes to kperf. Everything that turns those records into
//! `Sample`s, attributes off-CPU intervals, builds the flamegraph,
//! resolves symbols, and renders the live UI lives client-side.
//!
//! This makes the wire stable: the things that change rapidly during
//! development (sample shape, off-CPU classification, view models)
//! never reach the daemon. The things that *can* change here are
//! either (a) Apple changing kdebug or kperf, in which case both ends
//! update in lockstep, or (b) us adding new knobs to `SessionConfig`,
//! which is a forward-compatible addition.
//!
//! The matching daemon binary lives in `nperfd`. The matching
//! consumer for the client lives in `nperf-live` (and eventually a
//! standalone CLI mode).

use facet::Facet;

/// Parameters for a recording session, the bare minimum the daemon
/// needs to set up kperf + kdebug + kpc on behalf of the client.
///
/// Field shapes match the values our existing in-process recorder
/// already passes through to the kperf framework, so the daemon is
/// just a remote call into the same code paths.
#[derive(Clone, Debug, Facet)]
pub struct SessionConfig {
    /// Target pid to attach to. The daemon authorises this against
    /// the connection's peer credentials before starting.
    pub target_pid: u32,
    /// PET sampling frequency, in Hz. The daemon converts to a timer
    /// period via `kperf_ns_to_ticks`.
    pub frequency_hz: u32,
    /// kdebug ringbuffer size, in records. ~1M is several seconds of
    /// traffic on a busy system.
    pub buf_records: u32,
    /// Bitmask passed to `kperf_action_samplers_set`. The client
    /// chooses what samplers to enable
    /// (TH_INFO | USTACK | KSTACK | PMC_THREAD, etc.).
    pub samplers: u32,
    /// PMU event configs to load via `kpc_set_config`, encoded by the
    /// client's resolution of named events through `kpep_db`. Empty
    /// = no configurable counters, FIXED-class only.
    pub pmu_event_configs: Vec<u64>,
    /// Bitwise-OR of `KPC_CLASS_*_MASK` for the counter classes the
    /// client wants enabled.
    pub class_mask: u32,
    /// kdebug debugid range filter (KDBG_RANGETYPE). The daemon
    /// installs this verbatim via `KERN_KDSETREG`.
    pub filter_range_value1: u32,
    pub filter_range_value2: u32,
}

/// Wire-format mirror of xnu's `kd_buf`. Layout matches LP64 exactly
/// so the daemon can drain `KERN_KDREADTR` straight into a
/// `Vec<KdBufWire>` and the client can re-cast it into the in-memory
/// `KdBuf` consumed by the parser.
#[derive(Clone, Copy, Debug, Facet)]
pub struct KdBufWire {
    pub timestamp: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    /// Carries the current thread id in samples emitted by kperf.
    pub arg5: u64,
    pub debugid: u32,
    pub cpuid: u32,
    pub unused: u64,
}

/// One drain pass — what the daemon's `KERN_KDREADTR` loop produces
/// per cycle, batched so we don't ship a vox message per record.
/// `Vec<KdBufWire>` rather than a sequence of single records because
/// the parser expects ordered runs to find sample boundaries.
#[derive(Clone, Debug, Facet)]
pub struct KdBufBatch {
    pub records: Vec<KdBufWire>,
    /// Wall-clock timestamp the daemon recorded when this batch was
    /// drained, in nanoseconds since UNIX epoch. Used by the client
    /// only for diagnostics / latency tracking; sample timestamps
    /// inside `records` are kernel mach-time, not this clock.
    pub drained_at_unix_ns: u64,
}

/// Returned from a successfully-completed `record` call. The client
/// already counted samples on its side; this exists so the daemon can
/// surface its own view of the session for diagnostics.
#[derive(Clone, Debug, Facet)]
pub struct RecordSummary {
    /// Total records drained from `KERN_KDREADTR` over the session.
    pub records_drained: u64,
    /// Wall-clock duration of the session, ns.
    pub session_ns: u64,
}

/// Errors the daemon can surface to the client. Variant names map to
/// the place in the recorder where the error originated, so a UI can
/// render distinct messages for each case.
#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum RecordError {
    /// The daemon itself isn't running as root. Shouldn't happen via
    /// launchd, but possible when running by hand.
    NotRoot,
    /// The connection's peer uid doesn't own (or have access to) the
    /// target pid.
    NotAuthorized { caller_uid: u32, target_uid: u32 },
    /// Another client already has the kperf session. kperf is
    /// single-owner globally, so this is a hard refusal until the
    /// holding client disconnects.
    Busy { holder_uid: u32, holder_pid: u32, since_unix_ns: u64 },
    /// `pbi_ruid` lookup via libproc failed — typically because the
    /// pid is gone.
    NoSuchTarget(u32),
    /// A kperf framework call returned non-zero. `op` names the
    /// specific call; `code` is the return value (xnu calls these
    /// "kpc errors" but the code is opaque to us).
    Kperf { op: String, code: i32 },
    /// A `KERN_KDEBUG` sysctl failed. `op` is the sysctl name (e.g.
    /// "KERN_KDREADTR"); `message` is the rendered errno.
    Sysctl { op: String, message: String },
    /// We held ktrace ownership and lost it mid-session — typically
    /// Instruments / xctrace claimed it via its private entitlement.
    Evicted,
}

/// Cheap probe response — `status()` is what a client calls before
/// `record()` to find out if the daemon is reachable and idle.
#[derive(Clone, Debug, Facet)]
pub struct DaemonStatus {
    /// nperfd version string. Not used for compatibility — vox handles
    /// schema evolution — but useful in diagnostics.
    pub version: String,
    pub state: SessionState,
    /// Architecture string the daemon is running on ("aarch64",
    /// "x86_64"). Lets the client refuse to talk to a daemon whose
    /// kperf surface is wrong for its build.
    pub host_arch: String,
}

#[derive(Clone, Debug, Facet)]
#[repr(u8)]
pub enum SessionState {
    Idle,
    Recording {
        target_pid: u32,
        holder_uid: u32,
        holder_pid: u32,
        since_unix_ns: u64,
    },
}

/// The RPC. One streaming method (`record`) and one cheap probe
/// (`status`); deliberately kept small so the daemon's surface stays
/// auditable.
#[vox::service]
pub trait Nperfd {
    /// Configure kperf+kdebug for `config.target_pid` and stream the
    /// kdebug ringbuffer over `records`. Returns when the client
    /// closes `records` (clean shutdown), the recorder errors out
    /// (Sysctl / Kperf / Evicted), or the daemon process is signalled.
    async fn record(
        &self,
        config: SessionConfig,
        records: vox::Tx<KdBufBatch>,
    ) -> Result<RecordSummary, RecordError>;

    async fn status(&self) -> DaemonStatus;
}
