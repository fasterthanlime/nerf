//! `stax setup` (macOS only).
//!
//! Two modes, dispatched on euid at runtime:
//!
//!   * **non-root**: ad-hoc-codesign the current `stax` binary with the
//!     `com.apple.security.cs.debugger` entitlement. (Adapted from
//!     samply/src/mac/codesign_setup.rs, 1920bd32, MIT OR Apache-2.0.)
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
        <string>staxd=info,stax_mac_kperf_sys=info</string>
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
/// We used to codesign `stax` itself with cs.debugger here; that
/// entitlement now lives on `stax-shade` exclusively (see
/// stax-shade/src/main.rs for the why) and `cargo xtask install`
/// handles its codesign automatically. Nothing to do here.
fn codesign_self(_args: &args::SetupArgs) -> Result<(), Box<dyn Error>> {
    println!("`stax setup` (no sudo) is a no-op now.");
    println!();
    println!("Build + install everything with:");
    println!();
    println!("    cargo xtask install");
    println!();
    println!("…that codesigns stax-shade (the only entitlement-bearing");
    println!("binary in the architecture) and bootstraps stax-server");
    println!("as a per-user LaunchAgent.");
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
    fs::copy(&staged, BINARY_INSTALL_PATH).map_err(|err| {
        format!("copying staxd to {}: {err}", BINARY_INSTALL_PATH)
    })?;
    fs::set_permissions(BINARY_INSTALL_PATH, fs::Permissions::from_mode(0o755))?;

    codesign_staxd(BINARY_INSTALL_PATH)?;

    println!(":: writing LaunchDaemon plist -> {}", PLIST_PATH);
    fs::write(PLIST_PATH, NPERFD_LAUNCHD_PLIST).map_err(|err| {
        format!("writing {}: {err}", PLIST_PATH)
    })?;
    fs::set_permissions(PLIST_PATH, fs::Permissions::from_mode(0o644))?;

    // Reload via launchctl. `bootstrap` is the modern verb (macOS 10.10+);
    // we run `bootout` first to handle the "already loaded from a previous
    // setup" case, ignoring its exit status because it errors out cleanly
    // when nothing was loaded.
    println!(":: launchctl bootout system/{LAUNCHD_LABEL} (best-effort)");
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("system/{LAUNCHD_LABEL}")])
        .status();

    println!(":: launchctl bootstrap system {PLIST_PATH}");
    let status = Command::new("launchctl")
        .args(["bootstrap", "system", PLIST_PATH])
        .status()?;
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
    println!(
        ":: logs      : sudo log stream --predicate 'subsystem == \"eu.bearcove.staxd\"'"
    );
    println!(":: now: stax record --serve 127.0.0.1:8080 -- /bin/foo");
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

/// Ad-hoc codesign `staxd` at its final install path with the
/// cs.debugger entitlement. Required so the race-against-return
/// probe (probe.rs) can call task_for_pid on hardened-runtime
/// targets even when running as root. Mirrors the entitlement set
/// xtask applies to stax-shade.
///
/// Why here and not in `cargo xtask install`: signing the binary
/// at one path and then `fs::copy`'ing it to another invalidates
/// the embedded signature on macOS. Sign in place at the final
/// destination.
fn codesign_staxd(path: &str) -> Result<(), Box<dyn Error>> {
    const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.cs.debugger</key>
	<true/>
	<key>com.apple.security.get-task-allow</key>
	<true/>
	<key>com.apple.security.cs.allow-jit</key>
	<true/>
	<key>com.apple.security.cs.allow-unsigned-executable-memory</key>
	<true/>
</dict>
</plist>
"#;
    let mut ent_path = env::temp_dir();
    ent_path.push(format!("stax-setup-staxd-ent-{}.xml", std::process::id()));
    fs::write(&ent_path, ENTITLEMENTS)?;

    println!(":: codesigning {path} with com.apple.security.cs.debugger");
    let status = Command::new("codesign")
        .arg("--sign")
        .arg("-")
        .arg("--force")
        .arg("--options=runtime")
        .arg("--entitlements")
        .arg(&ent_path)
        .arg(path)
        .status()?;

    let _ = fs::remove_file(&ent_path);

    if !status.success() {
        return Err(format!("codesign exited with {status}").into());
    }
    Ok(())
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

