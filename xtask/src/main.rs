//! `cargo xtask <subcommand>` — dev-only entrypoints.
//!
//! Available subcommands:
//!   - `install`        Build stax in release mode, copy to ~/.cargo/bin/,
//!                      and (on macOS) codesign copied binaries.
//!   - `build-daemon`   Build staxd in release mode and print the
//!                      one-time `sudo cp` / `launchctl load` instructions
//!                      for the LaunchDaemon plist.
//!   - `codegen`        Generate TypeScript bindings for stax-live into
//!                      frontend/src/generated/.

use std::env;
use std::error::Error;
use std::fs;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

mod codegen;

const BIN_NAME: &str = "stax";
const DAEMON_BIN: &str = "staxd";
const SERVER_BIN: &str = "stax-server";
const SHADE_BIN: &str = "stax-shade";

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let task = args.get(1).map(String::as_str).unwrap_or("");
    match task {
        "install" => install()?,
        "build-daemon" => build_daemon()?,
        "codegen" => codegen::run()?,
        "" | "help" | "--help" | "-h" => {
            print_usage();
        }
        other => {
            eprintln!("xtask: unknown subcommand {:?}", other);
            print_usage();
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_usage() {
    eprintln!("Usage: cargo xtask <subcommand>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!(
        "  install              Build {bin} (release), copy to ~/.cargo/bin/{bin}, codesign on macOS",
        bin = BIN_NAME
    );
    eprintln!(
        "  build-daemon         Build {bin} (release) and print install instructions",
        bin = DAEMON_BIN
    );
    eprintln!(
        "  codegen              Generate TypeScript bindings for stax-live into frontend/src/generated/"
    );
}

fn install() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();
    let cargo_bin = cargo_bin_dir()?;
    fs::create_dir_all(&cargo_bin)?;

    // One workspace-wide build; never `cargo build -p <pkg>` per
    // binary, because per-package builds resolve features
    // independently and you can end up with mismatched feature
    // unification across artifacts that all link against the same
    // shared deps. One pass keeps the lockfile-resolved feature
    // set consistent across every binary we install.
    println!(":: Building workspace (release)...");
    cargo_build_release_workspace(&workspace_root)?;

    #[cfg(target_os = "macos")]
    let codesign_identity = resolve_codesign_identity()?;

    for bin in [BIN_NAME, DAEMON_BIN, SERVER_BIN, SHADE_BIN] {
        let src = workspace_root.join("target").join("release").join(bin);
        if !src.exists() {
            return Err(format!(
                "expected built binary at {} but it wasn't there",
                src.display()
            )
            .into());
        }
        let dst = cargo_bin.join(bin);
        println!(":: Copying {} -> {}", src.display(), dst.display());
        fs::copy(&src, &dst)?;

        #[cfg(target_os = "macos")]
        {
            // Re-sign every copied binary at the destination. Binaries
            // that carry team-bound entitlements, such as app groups,
            // must be signed by a real team identity; an ad-hoc
            // signature embeds the XML but leaves TeamIdentifier unset.
            if bin == SHADE_BIN {
                let entitlements = workspace_root
                    .join(SHADE_BIN)
                    .join("entitlements")
                    .join("eu.bearcove.stax-shade.debugger.plist");
                codesign_binary(&dst, Some(&entitlements), &codesign_identity)?;
            } else if bin == SERVER_BIN {
                // App-group membership so the sandboxed Mac client
                // can reach the unix socket inside
                // ~/Library/Group Containers/B2N6FSRTPV.eu.bearcove.stax/
                // without triggering a TCC "access data from other
                // apps" prompt.
                let entitlements = workspace_root
                    .join(SERVER_BIN)
                    .join("entitlements")
                    .join("eu.bearcove.stax-server.plist");
                codesign_binary(&dst, Some(&entitlements), &codesign_identity)?;
            } else {
                codesign_binary(&dst, None, &codesign_identity)?;
            }
        }
    }

    #[cfg(target_os = "macos")]
    install_server_launch_agent(&cargo_bin)?;

    println!();
    println!(":: Installed binaries to {}.", cargo_bin.display());
    println!();
    println!(":: Two install steps remain:");
    println!();
    println!("     sudo stax setup       # installs staxd as a LaunchDaemon (root)");
    println!();
    println!(":: stax-server (the unprivileged daemon agents talk to)");
    println!(":: was just bootstrapped under your user via launchctl.");
    println!(":: Logs:");
    println!("::   log stream --predicate 'subsystem == \"eu.bearcove.stax-server\"'");
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_server_launch_agent(cargo_bin: &Path) -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();
    let template = workspace_root
        .join(SERVER_BIN)
        .join("launchd")
        .join("eu.bearcove.stax-server.plist");
    let plist_text = fs::read_to_string(&template)?;

    let bin_path = cargo_bin.join(SERVER_BIN);
    let resolved = plist_text.replace("__BIN__", &bin_path.to_string_lossy());

    let agents_dir = home_dir().join("Library").join("LaunchAgents");
    fs::create_dir_all(&agents_dir)?;
    let plist_dst = agents_dir.join("eu.bearcove.stax-server.plist");
    println!(":: Writing LaunchAgent plist -> {}", plist_dst.display());
    fs::write(&plist_dst, resolved)?;

    let uid_str = unsafe { libc::getuid() }.to_string();
    let domain = format!("gui/{uid_str}");
    let label_target = format!("{domain}/eu.bearcove.stax-server");

    if is_loaded(&label_target) {
        println!(":: launchctl bootout {label_target}");
        let _ = Command::new("launchctl")
            .args(["bootout", &label_target])
            .status();

        // bootout returns before launchd actually tears the
        // service down — wait for it to really be gone before
        // calling bootstrap, otherwise we hit "Input/output
        // error" because the label is still in the domain.
        wait_until_unloaded(&label_target)?;
    }

    println!(":: launchctl bootstrap {domain} {}", plist_dst.display());
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist_dst)
        .status()?;
    if !status.success() {
        return Err(format!(
            "launchctl bootstrap exited with {status} (try `launchctl load {}` manually)",
            plist_dst.display()
        )
        .into());
    }
    println!(":: stax-server LaunchAgent loaded.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_loaded(label_target: &str) -> bool {
    Command::new("launchctl")
        .args(["print", label_target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn wait_until_unloaded(label_target: &str) -> Result<(), Box<dyn Error>> {
    use std::thread::sleep;
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_loaded(label_target) {
        if Instant::now() >= deadline {
            return Err(format!(
                "launchctl bootout: {label_target} still loaded after 10s; \
                 try `launchctl bootout {label_target}` manually"
            )
            .into());
        }
        sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn home_dir() -> PathBuf {
    PathBuf::from(env::var_os("HOME").expect("HOME is not set"))
}

#[cfg(target_os = "macos")]
fn resolve_codesign_identity() -> Result<String, Box<dyn Error>> {
    if let Ok(identity) = env::var("STAX_CODESIGN_IDENTITY") {
        if identity.trim().is_empty() {
            return Err("STAX_CODESIGN_IDENTITY is set but empty".into());
        }
        println!(":: Using codesign identity from STAX_CODESIGN_IDENTITY: {identity}");
        return Ok(identity);
    }

    for prefix in ["Developer ID Application", "Apple Development"] {
        if let Some(identity) = find_codesign_identity(prefix)? {
            println!(":: Using codesign identity: {identity}");
            return Ok(identity);
        }
    }

    Err(
        "no Developer ID Application or Apple Development codesign identity found; \
         set STAX_CODESIGN_IDENTITY=- only if you explicitly want ad-hoc signing"
            .into(),
    )
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn codesign_binary(
    binary: &Path,
    entitlements: Option<&Path>,
    identity: &str,
) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("codesign");
    command.arg("--force").arg("--sign").arg(identity);
    if identity != "-" {
        command.arg("--timestamp=none");
    }
    if let Some(entitlements) = entitlements {
        command
            .arg("--entitlements")
            .arg(entitlements)
            .arg("--generate-entitlement-der");
    }
    command.arg(binary);

    match entitlements {
        Some(entitlements) => println!(
            ":: Re-signing {} (identity={identity}, entitlements={})...",
            binary.display(),
            entitlements.display()
        ),
        None => println!(
            ":: Re-signing {} (identity={identity})...",
            binary.display()
        ),
    }

    let status = command.status()?;
    if !status.success() {
        return Err(format!("codesign exited with {status}").into());
    }
    Ok(())
}

fn build_daemon() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();

    // Workspace build (not `cargo build -p staxd`) so feature
    // unification stays identical to what `cargo xtask install`
    // produces — see the comment in `install()`.
    println!(":: Building workspace (release)...");
    cargo_build_release_workspace(&workspace_root)?;

    let binary = workspace_root
        .join("target")
        .join("release")
        .join(DAEMON_BIN);
    let plist = workspace_root
        .join(DAEMON_BIN)
        .join("launchd")
        .join("eu.bearcove.staxd.plist");
    println!();
    println!(":: Built {}", binary.display());
    println!();
    println!(":: To install (one-time, requires sudo):");
    println!("     sudo cp {} /usr/local/bin/", binary.display());
    println!("     sudo cp {} /Library/LaunchDaemons/", plist.display());
    println!("     sudo launchctl load /Library/LaunchDaemons/eu.bearcove.staxd.plist");
    println!();
    println!(":: After install, the daemon listens on /var/run/staxd.sock.");
    println!(":: Logs at /var/log/staxd.log.");
    Ok(())
}

fn cargo_build_release_workspace(workspace_root: &Path) -> Result<(), Box<dyn Error>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release", "--workspace"])
        .current_dir(workspace_root)
        .status()?;
    if !status.success() {
        return Err(format!("cargo build --workspace --release failed: {status}").into());
    }
    Ok(())
}

fn cargo_bin_dir() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(cargo_home).join("bin"));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cargo").join("bin"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate has a parent directory")
        .to_path_buf()
}
