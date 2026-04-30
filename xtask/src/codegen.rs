//! `cargo xtask codegen` — generate language bindings for the
//! stax-live RPC services. TypeScript output lands in
//! `frontend/src/generated/`, Swift output in
//! `stax-mac-app/stax/Generated/`.
//!
//! Mirrors the layout vox itself uses for its bindings: one
//! `<service>.generated.{ts,swift}` per service.

use std::error::Error;
use std::path::PathBuf;

use vox_codegen::targets::swift::SwiftBindings;

pub fn run() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();

    // TypeScript: full client+server bindings for the web frontend.
    let ts_dir = workspace_root
        .join("frontend")
        .join("src")
        .join("generated");
    std::fs::create_dir_all(&ts_dir)?;

    // Swift: client-only bindings for the macOS app.
    let swift_dir = workspace_root
        .join("stax-mac-app")
        .join("stax")
        .join("Generated");
    std::fs::create_dir_all(&swift_dir)?;

    let services: Vec<_> = stax_live_proto::all_services();

    for service in &services {
        let ts = vox_codegen::targets::typescript::generate_service(service);
        let ts_filename = format!("{}.generated.ts", service.service_name.to_lowercase());
        write_if_changed(&ts_dir.join(&ts_filename), ts)?;

        // Per-service Swift file: method IDs + client only. Named
        // types live in Common.generated.swift below so the Mac app
        // (a single Swift module) doesn't see duplicate declarations
        // for shared types like RunId / TerminalSize.
        let swift = vox_codegen::targets::swift::generate_service_without_types(
            service,
            SwiftBindings::Client,
        );
        let swift_filename = format!("{}.generated.swift", service.service_name);
        write_if_changed(&swift_dir.join(&swift_filename), swift)?;
    }

    // Single Common.generated.swift with deduplicated named types +
    // their encoders, referenced by every per-service file.
    let common = vox_codegen::targets::swift::generate_common_types(&services);
    write_if_changed(&swift_dir.join("Common.generated.swift"), common)?;

    Ok(())
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is xtask/, so the workspace root is its parent.
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap())
        .parent()
        .unwrap()
        .to_path_buf()
}

fn write_if_changed(
    path: &std::path::Path,
    contents: impl AsRef<[u8]>,
) -> Result<(), Box<dyn Error>> {
    let contents = contents.as_ref();
    if std::fs::read(path).ok().as_deref() == Some(contents) {
        println!("Unchanged {}", path.display());
        return Ok(());
    }
    std::fs::write(path, contents)?;
    println!("Wrote {}", path.display());
    Ok(())
}
