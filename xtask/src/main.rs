//! `cargo xtask <subcommand>` — dev-only entrypoints.
//!
//! Available subcommands:
//!   - `install`  Build nperf in release mode, copy to ~/.cargo/bin/, and
//!                (on macOS) ad-hoc codesign with the
//!                `com.apple.security.cs.debugger` entitlement.

use std::env;
use std::error::Error;
use std::fs;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

mod codegen;
mod migrate_archive;

const BIN_NAME: &str = "nperf";

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let task = args.get(1).map(String::as_str).unwrap_or("");
    match task {
        "install" => install()?,
        "migrate-archives" => migrate_archive::run(&args[2..])?,
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
        "  migrate-archives     Rewrite v1 .nperf archives in place to v2 (one-shot fixture migration)"
    );
    eprintln!(
        "  codegen              Generate TypeScript bindings for nperf-live into frontend/src/generated/"
    );
}

fn install() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();

    println!(":: Building {BIN_NAME} (release)...");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release", "-p", BIN_NAME])
        .current_dir(&workspace_root)
        .status()?;
    if !status.success() {
        return Err(format!("cargo build failed: {status}").into());
    }

    let src = workspace_root
        .join("target")
        .join("release")
        .join(BIN_NAME);
    if !src.exists() {
        return Err(format!("expected built binary at {} but it wasn't there", src.display()).into());
    }

    let dst = cargo_bin_dir()?.join(BIN_NAME);
    fs::create_dir_all(dst.parent().unwrap())?;

    println!(":: Copying {} -> {}", src.display(), dst.display());
    fs::copy(&src, &dst)?;

    #[cfg(target_os = "macos")]
    codesign_macos(&dst)?;

    println!();
    println!(":: Installed. Try `{BIN_NAME} --help`.");
    #[cfg(target_os = "macos")]
    println!(
        ":: macOS: this binary is signed with the com.apple.security.cs.debugger \
        entitlement so `{BIN_NAME} record --pid` works without sudo on non-hardened targets."
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn codesign_macos(binary: &Path) -> Result<(), Box<dyn Error>> {
    // - cs.debugger: lets nperf attach to other processes
    // - get-task-allow: lets debuggers attach to nperf itself
    // - cs.allow-jit: required for any JIT mmap (kept for completeness)
    // - cs.allow-unsigned-executable-memory: cranelift-jit (used by vox)
    //   uses plain mprotect-PROT_EXEC rather than MAP_JIT, which the
    //   kernel rejects under hardened runtime + allow-jit alone. The
    //   broader entitlement accepts non-MAP_JIT executable pages.
    const ENTITLEMENTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
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

    let mut entitlements_path = env::temp_dir();
    entitlements_path.push(format!("nperf-xtask-entitlements-{}.xml", std::process::id()));
    fs::write(&entitlements_path, ENTITLEMENTS_XML)?;

    println!(
        ":: Codesigning {} with com.apple.security.cs.debugger entitlement...",
        binary.display()
    );
    let status = Command::new("codesign")
        .arg("--force")
        .arg("--options")
        .arg("runtime")
        .arg("--sign")
        .arg("-")
        .arg("--entitlements")
        .arg(&entitlements_path)
        .arg(binary)
        .status()?;

    let _ = fs::remove_file(&entitlements_path);

    if !status.success() {
        return Err(format!("codesign exited with {status}").into());
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
    // CARGO_MANIFEST_DIR is the xtask crate's directory; its parent is the
    // workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate has a parent directory")
        .to_path_buf()
}
