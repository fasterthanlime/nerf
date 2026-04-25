//! Build the `nerf-mac-preload` cdylib on macOS hosts, gzip it, and stage
//! the bytes at `$OUT_DIR/libnerf_mac_preload.dylib.gz` so the runtime can
//! `include_bytes!` it. The runtime then writes the blob to a tempfile and
//! sets `DYLD_INSERT_LIBRARIES` on the launched target.
//!
//! Single-arch for now: we build for the host triple only. Multi-arch
//! (lipo of x86_64-apple-darwin + aarch64-apple-darwin + arm64e) is a
//! follow-up.

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let stub_path = out_dir.join("libnerf_mac_preload.dylib.gz");

    if target_os != "macos" {
        // Empty placeholder so include_bytes! compiles on non-mac hosts.
        fs::write(&stub_path, b"").expect("writing empty preload stub");
        return;
    }

    // The preload crate lives next to us in the repo. CARGO_MANIFEST_DIR
    // points at our crate dir; the preload is one level up + sibling.
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"),
    );
    let preload_dir = manifest_dir
        .parent()
        .expect("nerf-mac-capture has a parent")
        .join("nerf-mac-preload");

    // Re-run if the preload's sources change.
    println!(
        "cargo:rerun-if-changed={}",
        preload_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        preload_dir.join("Cargo.toml").display()
    );
    println!("cargo:rerun-if-changed=build.rs");

    // Build the preload as release. It's its own standalone workspace, so
    // we have to invoke cargo with --manifest-path; the `--target-dir` is
    // pinned to our build OUT_DIR so we don't pollute the user's workspace
    // target.
    let preload_target_dir = out_dir.join("preload-target");
    fs::create_dir_all(&preload_target_dir).expect("creating preload target dir");

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(preload_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&preload_target_dir)
        // Avoid recursive cargo picking up the parent's RUSTFLAGS, which
        // can include things the no_std preload can't tolerate.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // Same for target overrides.
        .env_remove("CARGO_BUILD_TARGET")
        .status()
        .expect("invoking cargo build for nerf-mac-preload");
    if !status.success() {
        panic!("nerf-mac-preload build failed: {status}");
    }

    let dylib_path = preload_target_dir
        .join("release")
        .join("libnerf_mac_preload.dylib");
    let dylib_bytes = fs::read(&dylib_path).unwrap_or_else(|err| {
        panic!(
            "reading {} after build: {}",
            dylib_path.display(),
            err
        )
    });

    // Gzip the dylib. We open-code a minimal flate2 dependency at build
    // time -- but that would need flate2 as a build-dep. Instead, just
    // store the dylib uncompressed (we can revisit gzip later; it's only
    // an install-size optimisation).
    fs::write(&stub_path, &dylib_bytes).expect("writing preload blob to OUT_DIR");
    let _ = stub_path; // silence unused-warning on non-mac

    // Write a sibling file describing whether the blob is gzipped, so the
    // runtime side knows whether to gunzip. For now: never gzipped.
    let mut marker = fs::File::create(out_dir.join("preload-format.txt"))
        .expect("creating preload-format marker");
    writeln!(marker, "raw-dylib").expect("writing preload-format marker");
}
