//! `stax setup` (macOS only).
//!
//! Two modes, dispatched on euid at runtime:
//!
//!   * **non-root**: explain the current install path. `cargo xtask
//!     install` builds, copies, codesigns, and bootstraps the
//!     unprivileged server.
//!
//!   * **root** (`sudo stax setup`): install `staxd` as a
//!     LaunchDaemon. Copies `~$SUDO_USER/.cargo/bin/staxd` to
//!     `/usr/local/bin/staxd`, drops the LaunchDaemon plist into
//!     `/Library/LaunchDaemons/`, and `launchctl bootstrap`s it.
//!     After this, `stax record …` runs without sudo because the
//!     privileged kperf calls happen in `staxd`.

use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::{self};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use crate::args;

/// LaunchDaemon plist installed at `/Library/LaunchDaemons/`. Embedded
/// verbatim as a constant so a freshly-installed `stax` doesn't have
/// to find the source tree at install time. The canonical version on
/// disk is `staxd/launchd/eu.bearcove.staxd.plist`; if you change
/// one, update both.
const NPERFD_LAUNCHD_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>eu.bearcove.staxd</string>

    <key>UserName</key>
    <string>root</string>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/staxd</string>
        <string>--socket</string>
        <string>/var/run/staxd.sock</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>staxd=info,stax_vox_observe=info,stax_mac_kperf_sys=info</string>
    </dict>

    <!-- No StandardOutPath / StandardErrorPath: staxd logs via
         os_log under subsystem eu.bearcove.staxd. View with
         `sudo log stream --predicate 'subsystem == "eu.bearcove.staxd"'`
         or Console.app. -->
</dict>
</plist>
"#;

const PLIST_PATH: &str = "/Library/LaunchDaemons/eu.bearcove.staxd.plist";
const BINARY_INSTALL_PATH: &str = "/usr/local/bin/staxd";
const LAUNCHD_LABEL: &str = "eu.bearcove.staxd";

pub fn main(args: args::SetupArgs) -> Result<(), Box<dyn Error>> {
    if is_root() {
        install_daemon(&args)
    } else {
        codesign_self(&args)
    }
}

fn is_root() -> bool {
    // SAFETY: getuid is always-safe on Unix.
    unsafe { libc::geteuid() == 0 }
}

// ---------------------------------------------------------------------------
// Non-root: codesign self
// ---------------------------------------------------------------------------

/// Non-root entry point: tell the user the modern install path.
/// `cargo xtask install` handles build/copy/codesign for the
/// user binaries and bootstraps stax-server. Nothing to do here.
fn codesign_self(_args: &args::SetupArgs) -> Result<(), Box<dyn Error>> {
    println!("`stax setup` (no sudo) is a no-op now.");
    println!();
    println!("Build + install everything with:");
    println!();
    println!("    cargo xtask install");
    println!();
    println!("…that codesigns copied binaries and bootstraps");
    println!("stax-server as a per-user LaunchAgent.");
    println!();
    println!("Then, one-time only, install the privileged daemon:");
    println!();
    println!("    sudo stax setup");
    Ok(())
}

// ---------------------------------------------------------------------------
// Root: install staxd as LaunchDaemon
// ---------------------------------------------------------------------------

fn install_daemon(args: &args::SetupArgs) -> Result<(), Box<dyn Error>> {
    let staged = locate_staged_daemon()?;
    println!(":: found staged daemon at {}", staged.display());

    if !args.yes {
        println!(
            r#"
This will install staxd as a LaunchDaemon (runs as root, owns kperf).

Steps:
  1. Copy {} -> {}
  2. Write {} from embedded plist
  3. launchctl bootstrap system {} (or load on older macOS)

After install, `stax record …` works without sudo because the
privileged kperf calls happen in staxd.

Press Enter to continue, or Ctrl-C to cancel."#,
            staged.display(),
            BINARY_INSTALL_PATH,
            PLIST_PATH,
            PLIST_PATH,
        );
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
    }

    println!(":: copying binary -> {}", BINARY_INSTALL_PATH);
    fs::copy(&staged, BINARY_INSTALL_PATH)
        .map_err(|err| format!("copying staxd to {}: {err}", BINARY_INSTALL_PATH))?;
    fs::set_permissions(BINARY_INSTALL_PATH, fs::Permissions::from_mode(0o755))?;

    ensure_staxd_codesigned(BINARY_INSTALL_PATH)?;

    println!(":: writing LaunchDaemon plist -> {}", PLIST_PATH);
    fs::write(PLIST_PATH, NPERFD_LAUNCHD_PLIST)
        .map_err(|err| format!("writing {}: {err}", PLIST_PATH))?;
    fs::set_permissions(PLIST_PATH, fs::Permissions::from_mode(0o644))?;

    // Reload via launchctl. `bootout` returns before launchd has
    // always fully removed the job from the domain; bootstrapping
    // immediately after can fail with the opaque "Bootstrap failed:
    // 5: Input/output error". Wait until `launchctl print` agrees
    // the label is gone before bootstrapping it again.
    let label_target = format!("system/{LAUNCHD_LABEL}");
    if is_launchd_job_loaded(&label_target) {
        println!(":: launchctl bootout {label_target}");
        let _ = Command::new("launchctl")
            .args(["bootout", &label_target])
            .status();
        wait_until_launchd_job_unloaded(&label_target)?;
    } else {
        println!(":: launchctl bootout {label_target} (not loaded)");
    }

    println!(":: launchctl bootstrap system {PLIST_PATH}");
    let status = bootstrap_launch_daemon()?;
    if !status.success() {
        return Err(format!(
            "launchctl bootstrap exited with {} — try `launchctl load {}` manually",
            status, PLIST_PATH,
        )
        .into());
    }

    println!();
    println!(":: staxd installed and running.");
    println!(":: socket    : /var/run/staxd.sock");
    println!(":: logs      : sudo log stream --predicate 'subsystem == \"eu.bearcove.staxd\"'");
    println!(":: now: stax record --serve 127.0.0.1:8080 -- /bin/foo");
    Ok(())
}

fn bootstrap_launch_daemon() -> Result<std::process::ExitStatus, Box<dyn Error>> {
    let mut last_status = Command::new("launchctl")
        .args(["bootstrap", "system", PLIST_PATH])
        .status()?;
    if last_status.success() {
        return Ok(last_status);
    }

    // launchd sometimes reports EIO while the old label is still
    // settling out of the system domain. A tiny retry removes the
    // flaky "run the exact same command again and it works" path.
    for attempt in 2..=5 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        println!(":: launchctl bootstrap retry {attempt}/5");
        last_status = Command::new("launchctl")
            .args(["bootstrap", "system", PLIST_PATH])
            .status()?;
        if last_status.success() {
            return Ok(last_status);
        }
    }
    Ok(last_status)
}

fn is_launchd_job_loaded(label_target: &str) -> bool {
    Command::new("launchctl")
        .args(["print", label_target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn wait_until_launchd_job_unloaded(label_target: &str) -> Result<(), Box<dyn Error>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while is_launchd_job_loaded(label_target) {
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "launchctl bootout: {label_target} still loaded after 10s; \
                 try `launchctl bootout {label_target}` manually"
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

/// Find `staxd` to install. Walk the candidate paths in order:
/// `~$SUDO_USER/.cargo/bin/staxd` (where `cargo xtask install`
/// dropped it), `~/.cargo/bin/staxd` (root's own — unusual), and
/// `/usr/local/bin/staxd` (if the user staged it manually).
fn locate_staged_daemon() -> Result<PathBuf, Box<dyn Error>> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(user_home) = sudo_user_home() {
        candidates.push(user_home.join(".cargo").join("bin").join("staxd"));
    }
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cargo").join("bin").join("staxd"));
    }

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "couldn't find a staged `staxd` binary. Looked in:\n{}\n\
         Run `cargo xtask install` first (as your normal user, not under sudo).",
        candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .into())
}

/// Keep the real team signature produced by `cargo xtask install`
/// when the copy preserved it. If not, sign with an available real
/// identity. Use `STAX_CODESIGN_IDENTITY=-` only for explicit ad-hoc
/// signing.
fn ensure_staxd_codesigned(path: &str) -> Result<(), Box<dyn Error>> {
    if has_team_signature(path) {
        println!(":: preserving existing team signature on {path}");
        return Ok(());
    }

    let identity = resolve_codesign_identity()?;

    println!(":: codesigning {path} with identity={identity}");
    let mut command = Command::new("codesign");
    command.arg("--sign").arg(&identity).arg("--force");
    if identity != "-" {
        command.arg("--timestamp=none");
    }
    command.arg(path);

    let status = command.status()?;

    if !status.success() {
        return Err(format!("codesign exited with {status}").into());
    }
    Ok(())
}

fn has_team_signature(path: &str) -> bool {
    let Ok(output) = Command::new("codesign")
        .args(["-dv", "--verbose=4", path])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    !stderr.contains("Signature=adhoc") && !stderr.contains("TeamIdentifier=not set")
}

fn resolve_codesign_identity() -> Result<String, Box<dyn Error>> {
    if let Ok(identity) = env::var("STAX_CODESIGN_IDENTITY") {
        if identity.trim().is_empty() {
            return Err("STAX_CODESIGN_IDENTITY is set but empty".into());
        }
        return Ok(identity);
    }

    for prefix in ["Developer ID Application", "Apple Development"] {
        if let Some(identity) = find_codesign_identity(prefix)? {
            return Ok(identity);
        }
    }
    Err(
        "no Developer ID Application or Apple Development codesign identity found; \
         rerun `cargo xtask install` as your normal user first, or set \
         STAX_CODESIGN_IDENTITY=- only if you explicitly want ad-hoc signing"
            .into(),
    )
}

fn find_codesign_identity(prefix: &str) -> Result<Option<String>, Box<dyn Error>> {
    let output = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().find_map(|line| {
        let identity = line.split('"').nth(1)?;
        identity.starts_with(prefix).then(|| identity.to_string())
    }))
}

/// When invoked via `sudo`, $SUDO_USER carries the original username.
/// Resolve their home directory via getpwnam_r — handles `/Users/foo`
/// on macOS and `/home/foo` on Linux without hardcoding either.
fn sudo_user_home() -> Option<PathBuf> {
    let user = env::var_os("SUDO_USER")?;
    home_dir_for_user(user.as_os_str())
}

fn home_dir_for_user(name: &OsStr) -> Option<PathBuf> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_name = CString::new(name.as_bytes()).ok()?;

    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result_ptr: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwnam_r writes into pwd / buf and sets *result_ptr.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            &mut result_ptr,
        )
    };
    if rc != 0 || result_ptr.is_null() {
        return None;
    }
    // SAFETY: pw_dir is a NUL-terminated C string owned by `buf` while
    // `result_ptr` is non-null.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}
