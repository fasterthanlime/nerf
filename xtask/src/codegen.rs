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

    for service in stax_live_proto::all_services() {
        let ts = vox_codegen::targets::typescript::generate_service(service);
        let ts_filename = format!("{}.generated.ts", service.service_name.to_lowercase());
        write_if_changed(&ts_dir.join(&ts_filename), ts)?;

        // Swift codegen is currently restricted to Profiler only. The
        // other services (RunControl/RunIngest/TerminalBroker) hit
        // upstream bugs in vox-codegen's Swift target:
        //   - shared named types (e.g. RunId) get re-declared in every
        //     file that references them, conflicting in a single Swift
        //     module
        //   - `Result<T, String>` is emitted as `Result<T, String>` but
        //     Swift's `String` doesn't conform to `Error`
        //   - some structs don't get Codable conformance
        // Re-enable once those are fixed upstream in vox-codegen.
        if service.service_name == "Profiler" {
            let swift = vox_codegen::targets::swift::generate_service_with_bindings(
                service,
                SwiftBindings::Client,
            );
            let swift_filename = format!("{}.generated.swift", service.service_name);
            write_if_changed(&swift_dir.join(&swift_filename), swift)?;
        }
    }

    // Theme CSS: catppuccin-mocha, scoped to anywhere we render
    // arborium-highlighted content (asm pane + interleaved source
    // snippets). `:where()` keeps specificity at zero so explicit
    // overrides in index.css still win.
    let theme = arborium_theme::theme::builtin::catppuccin_mocha();
    let css = theme.to_css(":where(.asm-line, .src-snip)");
    write_if_changed(&ts_dir.join("theme.css"), css)?;

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
