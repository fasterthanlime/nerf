//! `cargo xtask codegen` — generate TypeScript bindings for the
//! stax-live RPC service into `frontend/src/generated/`.
//!
//! Mirrors the layout vox itself uses for its bindings: one
//! `<service>.generated.ts` per service.

use std::error::Error;
use std::path::PathBuf;

pub fn run() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();
    let out_dir = workspace_root
        .join("frontend")
        .join("src")
        .join("generated");
    std::fs::create_dir_all(&out_dir)?;

    for service in stax_live_proto::all_services() {
        let ts = vox_codegen::targets::typescript::generate_service(service);
        let filename = format!("{}.generated.ts", service.service_name.to_lowercase());
        let out_path = out_dir.join(&filename);
        write_if_changed(&out_path, ts)?;
    }

    // Theme CSS: catppuccin-mocha, scoped to anywhere we render
    // arborium-highlighted content (asm pane + interleaved source
    // snippets). `:where()` keeps specificity at zero so explicit
    // overrides in index.css still win.
    let theme = arborium_theme::theme::builtin::catppuccin_mocha();
    let css = theme.to_css(":where(.asm-line, .src-snip)");
    write_if_changed(&out_dir.join("theme.css"), css)?;

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
