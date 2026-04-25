//! Runtime accessor for the bundled `nerf-mac-preload` dylib.
//!
//! The build script (`build.rs`) compiles `nerf-mac-preload` for the host
//! target and stages the resulting bytes at
//! `$OUT_DIR/libnerf_mac_preload.dylib.gz`. We `include_bytes!` the blob,
//! drop it to a tempfile on demand, and hand the path to the child via
//! `DYLD_INSERT_LIBRARIES`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;

/// The embedded preload-dylib bytes, as produced by `build.rs`.
pub static PRELOAD_DYLIB_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/libnerf_mac_preload.dylib.gz"));

/// Write the bundled preload dylib to a fresh tempfile and return its path.
/// The returned `TempPreload` owns the temp directory; its `Drop` impl
/// removes it.
pub fn stage_preload_dylib() -> io::Result<TempPreload> {
    let mut tempdir = std::env::temp_dir();
    tempdir.push(format!(
        "nerf-mac-preload-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&tempdir)?;

    let dylib_path = tempdir.join("libnerf_mac_preload.dylib");
    fs::write(&dylib_path, PRELOAD_DYLIB_BYTES)?;

    Ok(TempPreload {
        dir: tempdir,
        dylib_path,
    })
}

pub struct TempPreload {
    dir: PathBuf,
    dylib_path: PathBuf,
}

impl TempPreload {
    pub fn dylib_path(&self) -> &std::path::Path {
        &self.dylib_path
    }

    pub fn dylib_path_os(&self) -> OsString {
        self.dylib_path.clone().into()
    }
}

impl Drop for TempPreload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.dylib_path);
        let _ = fs::remove_dir(&self.dir);
    }
}
