//! Schema for the stax-shade ↔ stax-server protocol.
//!
//! Two services, opposing directions on the same vox session:
//!
//! - **`ShadeRegistry`** is exposed by `stax-server`. After
//!   `stax-shade` dials in and acquires the Mach task port, it
//!   calls `register_shade` once to identify itself; that's how
//!   the server knows *which* run an inbound shade connection
//!   belongs to (a single server may host multiple sequential
//!   runs, and a shade only handles one).
//!
//! - **`Shade`** is exposed by `stax-shade`. Once registered, the
//!   server calls into the shade for the active probing
//!   primitives — peek, poke, walker control, breakpoint
//!   management. This crate currently stubs peek + poke; the
//!   walker channel and breakpoint surface land in follow-up
//!   commits as they're implemented on the shade side.
//!
//! The session is kept alive for the duration of the attachment.
//! Server-side `closed()` cascades when the shade exits (clean
//! detach) or crashes (the cs.debugger blast-radius rationale —
//! see `stax-shade/src/main.rs`).

use facet::Facet;

/// Capabilities a particular shade build advertises at registration
/// time. Lets the server feature-gate behaviour without a version
/// bump every time we add a new probe primitive.
#[derive(Clone, Debug, Facet)]
pub struct ShadeCapabilities {
    /// Shade can read raw bytes from the target via `mach_vm_read`.
    pub peek: bool,
    /// Shade can write raw bytes via `mach_vm_write` (function-call
    /// syping, breakpoint installation, etc).
    pub poke: bool,
    /// Shade has a framehop unwinder ready and can stream
    /// accurately-walked user backtraces.
    pub framehop_walker: bool,
    /// Shade can install breakpoints + drive single-step exception
    /// handling for branch-coverage inside hot functions.
    pub breakpoint_step: bool,
}

#[derive(Clone, Debug, Facet)]
pub struct ShadeInfo {
    /// Run id this shade is attached for. Server-issued at
    /// `RunControl::start_run` time and forwarded to the shade
    /// via its `--run-id` flag.
    pub run_id: u64,
    /// PID the shade has a task port for.
    pub target_pid: u32,
    /// Process ID of the shade itself, so the server can correlate
    /// with launchd / process-tree views.
    pub shade_pid: u32,
    pub capabilities: ShadeCapabilities,
}

#[derive(Clone, Debug, Facet)]
pub struct ShadeAck {
    /// Whether the server accepted the registration. `false` would
    /// mean the run is no longer active or the run id doesn't
    /// match — the shade should detach + exit.
    pub accepted: bool,
    /// Human-readable reason when `accepted == false`.
    pub reason: Option<String>,
}

/// Server-side handshake. The shade dials in, calls
/// `register_shade` once, then keeps the session open so the
/// server can call back into the shade's `Shade` service.
#[vox::service]
pub trait ShadeRegistry {
    async fn register_shade(&self, info: ShadeInfo) -> Result<ShadeAck, String>;
}

/// Shade-side primitives. Stubs in this commit; implementations
/// land alongside their server-side callers (framehop walker
/// first, then peek/poke, then branch-stepping).
#[vox::service]
pub trait Shade {
    /// Read `len` bytes starting at `addr` (target AVMA) via
    /// `mach_vm_read`.
    async fn peek(&self, addr: u64, len: u32) -> Result<Vec<u8>, String>;

    /// Write `bytes` starting at `addr` (target AVMA) via
    /// `mach_vm_write`. Caller is responsible for restoring the
    /// original bytes — the shade does not maintain a poke
    /// history.
    async fn poke(&self, addr: u64, bytes: Vec<u8>) -> Result<(), String>;
}

/// All service descriptors exposed by stax-shade-proto.
pub fn all_services() -> Vec<&'static vox::session::ServiceDescriptor> {
    vec![shade_registry_service_descriptor(), shade_service_descriptor()]
}
