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
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use crate::args;

/// Entitlements applied to the user-facing `stax` binary.
const NPERF_ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.cs.debugger</key>
	<true/>
</dict>
</plist>
"#;

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

    <key>StandardOutPath</key>
    <string>/var/log/staxd.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/staxd.log</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>staxd=info,stax_mac_kperf_sys=info</string>
    </dict>
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

fn codesign_self(args: &args::SetupArgs) -> Result<(), Box<dyn Error>> {
    let exe = env::current_exe()?;

    if !args.yes {
        println!(
            r#"
On macOS, attaching to an existing process via task_for_pid requires the
com.apple.security.cs.debugger entitlement. This subcommand will ad-hoc
codesign your stax binary with that entitlement (signed for your local
machine only -- not redistributable). The following command will run:

    codesign --force --options runtime --sign - \
      --entitlements <tempfile> {}

Press Enter to continue, or Ctrl-C to cancel."#,
            exe.display(),
        );
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
    }

    let entitlements_path = stage_entitlements()?;
    let status = Command::new("codesign")
        .arg("--force")
        .arg("--options")
        .arg("runtime")
        .arg("--sign")
        .arg("-")
        .arg("--entitlements")
        .arg(&entitlements_path)
        .arg(&exe)
        .status()?;
    let _ = fs::remove_file(&entitlements_path);

    if !status.success() {
        return Err(format!("codesign exited with {}", status).into());
    }

    println!("Code signing successful: {}", exe.display());
    println!("You can now run `stax record …` without sudo.");
    println!();
    println!(
        "To install staxd as a LaunchDaemon (so the privileged kperf \
         calls happen there, not in your CLI), run: sudo stax setup",
    );
    Ok(())
}

fn stage_entitlements() -> io::Result<PathBuf> {
    let mut path = env::temp_dir();
    path.push(format!("stax-entitlements-{}.xml", std::process::id()));
    let mut f = fs::File::create(&path)?;
    f.write_all(NPERF_ENTITLEMENTS.as_bytes())?;
    f.flush()?;
    Ok(path)
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
    println!(":: logs      : /var/log/staxd.log");
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

