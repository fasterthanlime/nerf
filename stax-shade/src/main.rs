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

mod walker;

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

    /// Sampling rate for the framehop user-stack walker, in Hz.
    /// Set to 0 to disable framehop-side sampling entirely (useful
    /// when smoke-testing kperf integration without the walker).
    #[facet(args::named, default = default_walker_hz())]
    walker_hz: u32,
}

fn default_walker_hz() -> u32 {
    // 100 Hz is conservative — kperf typically runs at 1000 Hz, and
    // the framehop loop on bee.app's 27 threads finished one snapshot
    // in ~400µs, so 1000 Hz is feasible. Starting low keeps the log
    // signal-to-noise high and gives us headroom while we wire the
    // sample stream out to stax-server.
    100
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
    let mode = match (cli.attach, cli.launch, cli.command.first()) {
        (Some(pid), false, _) => AttachMode::Existing(pid),
        (None, true, Some(_)) => AttachMode::Launch(cli.command.clone()),
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

    let attached = match mode {
        AttachMode::Existing(pid) => {
            let task = task_for_pid(pid)?;
            tracing::info!(pid, task_port = task, "attached to existing process");
            Attached {
                pid,
                task,
                pre_resume: None,
            }
        }
        AttachMode::Launch(argv) => launch_suspended(argv)?,
    };
    let pid = attached.pid;
    let task = attached.task;

    // If a server socket was provided, dial in and register so the
    // server knows we're up and which run we belong to. We hold
    // the resulting client for the lifetime of the shade so the
    // periodic walker can publish samples through it. When no
    // socket is configured we just walk locally (smoke-test mode).
    let (server_client, server_run_id) = match (cli.server_socket.as_deref(), cli.run_id) {
        (Some(socket), Some(run_id)) => {
            let c = register_with_server(socket, run_id, pid).await?;
            (Some(c), Some(run_id))
        }
        _ => {
            tracing::warn!(
                "no --server-socket / --run-id; running standalone — \
                 stax-server will not learn about this attachment"
            );
            (None, None)
        }
    };

    // Walk the target's loaded images once and build a framehop
    // unwinder + AVMA→image map out of the parsed sections, then
    // take one snapshot of every thread to validate the path
    // end-to-end and (if --walker-hz > 0) hand the unwinder off
    // to a periodic sampling task.
    let _walker_handle = if let Some((mut unwinder, image_map)) =
        build_unwinder_from_target(task)
    {
        snapshot_once(task, &mut unwinder, &image_map);
        if cli.walker_hz > 0 {
            Some(spawn_periodic_walker(
                task,
                unwinder,
                image_map,
                cli.walker_hz,
                server_client.clone(),
                server_run_id,
            ))
        } else {
            tracing::info!("--walker-hz=0, framehop sampling disabled");
            None
        }
    } else {
        None
    };

    // For --launch we held the target suspended through
    // task_for_pid + register_shade so neither kperf-side nor
    // shade-side could miss the very first instructions. Now
    // that the server knows about us and (eventually) staxd has
    // the kperf session running, resume.
    if let Some(pre_resume) = attached.pre_resume {
        pre_resume.resume()?;
    }

    park_until_signal().await;
    Ok(())
}

enum AttachMode {
    Existing(u32),
    Launch(Vec<String>),
}

struct Attached {
    pid: u32,
    task: mach2::port::mach_port_t,
    /// `Some` for `--launch`: target was started suspended via
    /// POSIX_SPAWN_START_SUSPENDED and is waiting for us to resume
    /// it. `None` for `--attach`: target was already running.
    pre_resume: Option<PreResume>,
}

struct PreResume {
    task: mach2::port::mach_port_t,
}

impl PreResume {
    fn resume(self) -> eyre::Result<()> {
        use mach2::kern_return::KERN_SUCCESS;
        use mach2::task::task_resume;
        // SAFETY: task is a valid Mach port acquired via task_for_pid
        // on the just-spawned child. task_resume is safe to call on
        // a suspended task port owned by us.
        let kr = unsafe { task_resume(self.task) };
        if kr != KERN_SUCCESS {
            eyre::bail!("task_resume failed: kr={kr}");
        }
        tracing::info!("target resumed");
        Ok(())
    }
}

/// Long-running periodic walker: every `1_000_000 / hz` µs, walk
/// every thread of `task` and emit aggregate stats every second
/// (samples per second, average frame depth, p50/p99 elapsed).
///
/// Lives on a tokio task so the main thread can keep waiting on
/// SIGTERM. The task takes ownership of the unwinder + image_map
/// + framehop cache; when the runtime drops on shutdown, the task
/// drops and so do they.
///
/// Streaming samples to stax-server lands in the next slice — for
/// now the loop runs purely for throughput / overhead validation.
#[cfg(target_arch = "aarch64")]
fn spawn_periodic_walker(
    task: mach2::port::mach_port_t,
    unwinder: framehop::aarch64::UnwinderAarch64<Vec<u8>>,
    image_map: walker::ImageMap,
    hz: u32,
    server_client: Option<ShadeRegistryClient>,
    server_run_id: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    // SAFETY (justification): mach_port_t is a u32 and the right
    // is task-scoped to the shade. Sending it to another task in
    // the same process is fine; the kernel doesn't require the
    // sender to be the original acquirer.
    struct Send<T>(T);
    unsafe impl<T> std::marker::Send for Send<T> {}

    let task_send = Send(task);
    tokio::task::spawn(async move {
        let task = task_send.0;
        // Drop the wrapper so subsequent moves are clean.
        let _ = image_map; // ensure map captured (we use it below)
        let mut unwinder = unwinder;
        let image_map = image_map;
        let mut cache = framehop::aarch64::CacheAarch64::new();

        let period = std::time::Duration::from_micros(1_000_000 / u64::from(hz.max(1)));
        let mut interval = tokio::time::interval(period);
        // Skip-missed: if a tick falls behind we don't pile them
        // up — better to drop than burst.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let report_period = std::time::Duration::from_secs(1);
        let mut window_started = std::time::Instant::now();
        let mut window_samples = 0u64;
        let mut window_frames = 0u64;
        let mut window_elapsed_us_total = 0u64;
        let mut window_max_us = 0u64;

        tracing::info!(hz, period_us = period.as_micros() as u64, "periodic walker started");

        // Publish-side state: we batch samples between server
        // pushes so we're not making a vox call every 10ms. With
        // batch_period=200ms the server sees ~5 publishes/sec
        // carrying ~540 samples each at 100Hz × 27 threads.
        let publish_period = std::time::Duration::from_millis(200);
        let mut publish_started = std::time::Instant::now();
        let mut pending_batch: Vec<stax_shade_proto::WalkerSample> = Vec::new();
        let mut window_published = 0u64;
        let mut window_publish_errors = 0u64;
        // Flips on the first user error from publish_walker_samples
        // ("no active run", "run id mismatch"). Once stale we stop
        // trying — stax-server will SIGTERM us on run-end anyway,
        // and continuing to push wastes a vox call per batch.
        let mut publishing_stale = false;

        loop {
            interval.tick().await;
            let started = std::time::Instant::now();
            let samples = walker::snapshot_all_threads(task, &mut unwinder, &mut cache);
            let elapsed_us = started.elapsed().as_micros() as u64;

            window_samples += samples.len() as u64;
            for s in &samples {
                window_frames += s.frames.len() as u64;
            }
            window_elapsed_us_total += elapsed_us;
            window_max_us = window_max_us.max(elapsed_us);

            // Convert to wire samples and accumulate. Drop empty
            // walks (thread suspended but PC=0) — they carry no
            // information and would just waste server-side merge
            // work. Use a single nanosecond clock for the whole
            // pass; per-thread skew within ~1ms doesn't matter
            // for visualisation.
            if server_client.is_some() && server_run_id.is_some() && !publishing_stale {
                let timestamp_ns = sample_timestamp_ns();
                for s in samples {
                    if s.pc == 0 || s.frames.is_empty() {
                        continue;
                    }
                    pending_batch.push(stax_shade_proto::WalkerSample {
                        tid: s.thread,
                        timestamp_ns,
                        frames: s.frames,
                    });
                }
            } else {
                pending_batch.clear();
            }

            if let (Some(client), Some(rid)) = (server_client.as_ref(), server_run_id)
                && !pending_batch.is_empty()
                && publish_started.elapsed() >= publish_period
                && !publishing_stale
            {
                let batch = std::mem::take(&mut pending_batch);
                let n = batch.len();
                match client.publish_walker_samples(rid, batch).await {
                    Ok(()) => window_published += n as u64,
                    Err(vox::VoxError::User(msg)) => {
                        // Run id mismatch / no active run — server
                        // says we're stale. Stop trying; SIGTERM
                        // from stax-server will take us down.
                        tracing::warn!(error = %msg, "server rejected walker samples; halting publish");
                        window_publish_errors += 1;
                        publishing_stale = true;
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "publish_walker_samples failed");
                        window_publish_errors += 1;
                    }
                }
                publish_started = std::time::Instant::now();
            }

            if window_started.elapsed() >= report_period {
                let pass_count = window_started.elapsed().as_secs_f64();
                let avg_us = window_elapsed_us_total
                    .checked_div(if pass_count > 0.0 {
                        (pass_count * f64::from(hz)) as u64
                    } else {
                        1
                    })
                    .unwrap_or(0);
                tracing::info!(
                    samples = window_samples,
                    frames = window_frames,
                    published = window_published,
                    publish_errors = window_publish_errors,
                    avg_us,
                    max_us = window_max_us,
                    map_entries = image_map.len_for_log(),
                    "walker 1s window"
                );
                window_started = std::time::Instant::now();
                window_samples = 0;
                window_frames = 0;
                window_elapsed_us_total = 0;
                window_max_us = 0;
                window_published = 0;
                window_publish_errors = 0;
            }
        }
    })
}

/// Coarse-grained ns-since-epoch clock for tagging walker samples.
/// `Instant` doesn't expose a unix epoch; the aggregator only
/// requires monotonicity and rough alignment with kperf's
/// timestamps, so `SystemTime::now()` is good enough.
fn sample_timestamp_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// One-shot validation: enumerate threads, walk each one's stack
/// via framehop, log a per-thread frame count + an example stack
/// rendered as `<basename>+<offset>` so it's eyeballable without
/// pulling out `atos`.
#[cfg(target_arch = "aarch64")]
fn snapshot_once(
    task: mach2::port::mach_port_t,
    unwinder: &mut framehop::aarch64::UnwinderAarch64<Vec<u8>>,
    image_map: &walker::ImageMap,
) {
    let mut cache = framehop::aarch64::CacheAarch64::new();
    let started = std::time::Instant::now();
    let samples = walker::snapshot_all_threads(task, unwinder, &mut cache);
    let elapsed_us = started.elapsed().as_micros();

    let total_frames: usize = samples.iter().map(|s| s.frames.len()).sum();
    let with_frames = samples.iter().filter(|s| !s.frames.is_empty()).count();
    tracing::info!(
        threads = samples.len(),
        with_frames,
        total_frames,
        elapsed_us = elapsed_us as u64,
        "framehop one-shot snapshot"
    );

    if let Some(deepest) = samples.iter().max_by_key(|s| s.frames.len()) {
        tracing::info!(
            thread = deepest.thread,
            pc = format!("{:#x}", deepest.pc),
            depth = deepest.frames.len(),
            "deepest stack"
        );
        for (i, addr) in deepest.frames.iter().enumerate().take(32) {
            match image_map.lookup(*addr) {
                Some(entry) => {
                    let basename = entry
                        .path
                        .rsplit('/')
                        .next()
                        .unwrap_or(entry.path.as_str());
                    let offset = addr - entry.load_address;
                    tracing::info!(
                        "  frame[{i:>2}] {:#x}  {basename}+{:#x}",
                        addr,
                        offset
                    );
                }
                None => {
                    tracing::info!("  frame[{i:>2}] {:#x}  (no module)", addr);
                }
            }
        }
        if deepest.frames.len() > 32 {
            tracing::info!("  …and {} more", deepest.frames.len() - 32);
        }
        if let Some(err) = &deepest.error {
            tracing::info!("  walk ended on: {err}");
        }
    }
}

/// Snapshot the target's loaded images and build a framehop
/// `UnwinderAarch64` from them. Logs a coverage summary; returns
/// `None` only if the dyld walk itself failed — a shade running
/// without an unwinder is still useful for kperf-side bookkeeping.
fn build_unwinder_from_target(
    task: mach2::port::mach_port_t,
) -> Option<(framehop::aarch64::UnwinderAarch64<Vec<u8>>, walker::ImageMap)> {
    let walker = stax_target_images::TargetImageWalker::new(task);
    let images = match walker.enumerate() {
        Ok(images) => images,
        Err(e) => {
            tracing::warn!("dyld walk failed: {e}");
            return None;
        }
    };

    // Pre-walk byte totals for the log line — we lose ownership
    // once we hand the sections to framehop.
    let mut unwind_bytes_total = 0usize;
    let preview: Vec<(u64, String)> = images
        .iter()
        .take(8)
        .map(|i| (i.load_address, i.path.clone()))
        .collect();
    let total_count = images.len();
    for img in &images {
        if let Some(s) = img.sections.as_ref() {
            if let Some(b) = s.unwind_info.as_ref() {
                unwind_bytes_total += b.bytes.len();
            }
            if let Some(b) = s.eh_frame.as_ref() {
                unwind_bytes_total += b.bytes.len();
            }
        }
    }

    let (unwinder, image_map, stats) = walker::build_unwinder(images);
    tracing::info!(
        count = stats.images_total,
        modules = stats.modules_added,
        with_unwind_info = stats.with_unwind_info,
        with_eh_frame = stats.with_eh_frame,
        skipped_no_sections = stats.skipped_no_sections,
        skipped_no_text = stats.skipped_no_text,
        unwind_bytes = unwind_bytes_total,
        "framehop unwinder built from dyld image list"
    );
    for (addr, path) in &preview {
        tracing::debug!(load_address = format!("{addr:#x}"), path = %path, "  image");
    }
    if total_count > preview.len() {
        tracing::debug!("  …and {} more", total_count - preview.len());
    }
    Some((unwinder, image_map))
}

/// Spawn a fresh child via `posix_spawn` with
/// `POSIX_SPAWN_START_SUSPENDED`, acquire its task port, and
/// hand back the suspended-attachment record. The caller is
/// expected to do whatever pre-resume setup it needs (register
/// with stax-server, wait for kperf to be primed, install
/// breakpoints, …) and then call `PreResume::resume`.
///
/// Argv: `argv[0]` is the program path; the rest are passed to
/// the child as-is. We use `posix_spawn` (not `posix_spawnp`) so
/// the program path is taken literally — callers that want PATH
/// resolution can invoke `which(1)` first.
fn launch_suspended(argv: Vec<String>) -> eyre::Result<Attached> {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::ptr;

    if argv.is_empty() {
        eyre::bail!("--launch requires at least one positional argument (the program path)");
    }

    let program = CString::new(argv[0].as_str())
        .map_err(|_| eyre::eyre!("program path contains an interior NUL"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| eyre::eyre!("argv contains an interior NUL"))?;
    let mut argv_p: Vec<*mut c_char> = argv_c
        .iter()
        .map(|c| c.as_ptr() as *mut c_char)
        .collect();
    argv_p.push(ptr::null_mut());

    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    // SAFETY: posix_spawnattr_init writes through the out-pointer.
    // We pair it with destroy below so the kernel side cleans up.
    let r = unsafe { libc::posix_spawnattr_init(&mut attr) };
    if r != 0 {
        eyre::bail!("posix_spawnattr_init: {r}");
    }
    // The whole point: child stays parked at its first instruction
    // until we task_resume. SETSIGDEF is recommended by Apple's
    // header so the child gets a clean signal mask regardless of
    // ours.
    let flags = libc::POSIX_SPAWN_START_SUSPENDED | libc::POSIX_SPAWN_SETSIGDEF;
    let r = unsafe { libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short) };
    if r != 0 {
        unsafe {
            libc::posix_spawnattr_destroy(&mut attr);
        }
        eyre::bail!("posix_spawnattr_setflags: {r}");
    }

    let mut pid: libc::pid_t = 0;
    let r = unsafe {
        libc::posix_spawn(
            &mut pid,
            program.as_ptr(),
            ptr::null(),
            &attr,
            argv_p.as_ptr(),
            // Inherit our environment as-is — we want PATH /
            // DYLD_* / etc. flowing through to the child.
            extern_environ(),
        )
    };
    unsafe {
        libc::posix_spawnattr_destroy(&mut attr);
    }
    if r != 0 {
        eyre::bail!("posix_spawn({}): {r}", argv[0]);
    }
    let pid_u32 = pid as u32;
    tracing::info!(pid = pid_u32, program = %argv[0], "spawned target (suspended)");

    let task = task_for_pid(pid_u32).inspect_err(|_| {
        // Best-effort: the child is suspended and we own it; if
        // task_for_pid failed, there's no point leaving the
        // process around. SIGKILL it and reap.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    })?;

    Ok(Attached {
        pid: pid_u32,
        task,
        pre_resume: Some(PreResume { task }),
    })
}

unsafe extern "C" {
    static environ: *mut *mut std::os::raw::c_char;
}

fn extern_environ() -> *const *mut std::os::raw::c_char {
    // SAFETY: read of process-wide global. macOS exposes
    // `environ` as the canonical envp; posix_spawn accepts a
    // const pointer to it.
    unsafe { environ as *const _ }
}

async fn register_with_server(
    socket: &str,
    run_id: u64,
    target_pid: u32,
) -> eyre::Result<ShadeRegistryClient> {
    let url = format!("local://{socket}");
    let client: ShadeRegistryClient = vox::connect(&url).await?;
    let info = ShadeInfo {
        run_id,
        target_pid,
        shade_pid: std::process::id(),
        capabilities: ShadeCapabilities {
            peek: false,
            poke: false,
            // Truthful: walker is wired and pushing samples on the
            // same vox session below.
            framehop_walker: true,
            breakpoint_step: false,
        },
    };
    match client.register_shade(info).await {
        Ok(ShadeAck { accepted: true, .. }) => {
            tracing::info!(run_id, "registered with stax-server");
            Ok(client)
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

/// Idle until SIGINT or SIGTERM. Stage C will replace this with
/// awaiting on the vox session's `closed()` future once the server
/// can actually call into `Shade` and drive a real teardown.
///
/// Earlier versions also raced a `spawn_blocking(read stdin)` so
/// closing the parent's pipe would terminate the shade. That made
/// ctrl-c hang: the blocking-pool thread was stuck in a `read()`
/// syscall forever, and tokio's runtime drop waits for the
/// blocking pool. Signals alone are enough — stax-server kills the
/// shade with SIGTERM at run-end.
async fn park_until_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("install SIGINT handler failed: {e}");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("install SIGTERM handler failed: {e}");
            return;
        }
    };
    tokio::select! {
        _ = sigint.recv() => tracing::info!("received SIGINT, shutting down"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
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
