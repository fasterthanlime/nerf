use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::binary::BinaryData;
use crate::utils::HexString;

pub struct DebugInfoIndex {
    by_filename: HashMap<Vec<u8>, Vec<Arc<BinaryData>>>,
    by_build_id: HashMap<Vec<u8>, Vec<Arc<BinaryData>>>,
    auto_load: bool,
}

fn check_build_id(data: &Arc<BinaryData>, expected_build_id: Option<&[u8]>) -> bool {
    let build_id = data.build_id();
    expected_build_id.is_none() || build_id.is_none() || build_id == expected_build_id
}

impl Default for DebugInfoIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugInfoIndex {
    pub fn new() -> Self {
        DebugInfoIndex {
            by_filename: HashMap::new(),
            by_build_id: HashMap::new(),
            auto_load: false,
        }
    }

    pub fn enable_auto_load(&mut self) {
        self.auto_load = true;
    }

    pub fn add<P: AsRef<Path>>(&mut self, path: P) {
        let mut done = HashSet::new();
        self.add_impl(&mut done, path.as_ref(), true);
    }

    pub fn get(
        &mut self,
        path: &str,
        debuglink: Option<&[u8]>,
        build_id: Option<&[u8]>,
    ) -> Option<Arc<BinaryData>> {
        let (bin, dbg) = self.get_pair(path, debuglink, build_id);
        dbg.or(bin)
    }

    pub fn get_pair(
        &mut self,
        path: &str,
        debuglink: Option<&[u8]>,
        build_id: Option<&[u8]>,
    ) -> (Option<Arc<BinaryData>>, Option<Arc<BinaryData>>) {
        debug!(
            "Requested debug info for '{}'; debuglink = {:?}, build_id = {:?}",
            path,
            debuglink.map(String::from_utf8_lossy),
            build_id.map(HexString)
        );
        let basename = &path[path.rfind("/").map(|index| index + 1).unwrap_or(0)..];
        let basename: &[u8] = basename.as_ref();

        let mut candidates: Vec<Arc<BinaryData>> = Vec::new();
        if let Some(build_id) = build_id {
            if let Some(entries) = self.by_build_id.get(build_id) {
                candidates.extend(entries.iter().cloned());

                for entry in entries {
                    if let Some(debuglink) = entry.debuglink() {
                        if let Some(debug_entries) = self.by_filename.get(debuglink) {
                            candidates.extend(
                                debug_entries
                                    .iter()
                                    .filter(|data| check_build_id(data, Some(build_id)))
                                    .cloned(),
                            );
                        }
                    }
                }
            }
        }

        if let Some(entries) = self.by_filename.get(basename) {
            candidates.extend(
                entries
                    .iter()
                    .filter(|data| check_build_id(data, build_id))
                    .cloned(),
            );

            for entry in entries {
                if let Some(debuglink) = entry.debuglink() {
                    if let Some(debug_entries) = self.by_filename.get(debuglink) {
                        candidates.extend(
                            debug_entries
                                .iter()
                                .filter(|data| check_build_id(data, build_id))
                                .cloned(),
                        );
                    }
                }
            }
        }

        if let Some(debuglink) = debuglink {
            if let Some(entries) = self.by_filename.get(debuglink) {
                candidates.extend(
                    entries
                        .iter()
                        .filter(|data| check_build_id(data, build_id))
                        .cloned(),
                );
            }
        }

        if candidates.is_empty() {
            if let Some(build_id) = build_id {
                if let Some(binary) = self.try_auto_load(path, debuglink, build_id) {
                    candidates.push(binary);
                }
            }
        }

        candidates.sort_by_key(|entry| entry.as_ptr());
        candidates.dedup_by_key(|entry| entry.as_ptr());
        let matching: Vec<_> = candidates
            .iter()
            .filter(|entry| entry.build_id().is_some() && entry.build_id() == build_id)
            .cloned()
            .collect();
        if !matching.is_empty() {
            candidates = matching;
        }

        let (bin, dbg) = match candidates.len() {
            0 => (None, None),
            1 => (candidates.pop(), None),
            _ => {
                candidates.sort_by_key(|entry| entry.as_bytes().len());
                let dbg = candidates.pop();
                let bin = candidates.pop();
                (bin, dbg)
            }
        };

        debug!(
            "Debug info lookup result: bin = {:?}, dbg = {:?}",
            bin.as_ref().map(|data| data.name()),
            dbg.as_ref().map(|data| data.name())
        );
        (bin, dbg)
    }

    fn try_auto_load(
        &mut self,
        path: &str,
        debuglink: Option<&[u8]>,
        build_id: &[u8],
    ) -> Option<Arc<BinaryData>> {
        trace!(
            "try_auto_load(path={:?}, build_id={:?}): auto_load={}",
            path,
            HexString(build_id),
            self.auto_load
        );
        if !self.auto_load {
            trace!("try_auto_load({:?}): skipped — auto_load disabled", path);
            return None;
        }
        if !path.starts_with("/") {
            trace!("try_auto_load({:?}): skipped — path is not absolute", path);
            return None;
        }

        let original_path = Path::new(path);

        // Try to load the original (possibly stripped) binary so it gets
        // indexed by build_id.  This also serves as the fallback if no
        // separate debug file is found.
        let mut result: Option<Arc<BinaryData>> = None;
        if original_path.exists() {
            match BinaryData::load_from_fs(original_path) {
                Ok(binary) => {
                    let actual = binary.build_id();
                    if actual == Some(build_id) {
                        trace!(
                            "try_auto_load({:?}): loaded original binary, build_id matches",
                            path
                        );
                        let binary = Arc::new(binary);
                        self.by_build_id
                            .entry(build_id.to_vec())
                            .or_default()
                            .push(binary.clone());
                        result = Some(binary);
                    } else {
                        trace!(
                            "try_auto_load({:?}): build_id mismatch — got {:?}, expected {:?}",
                            path,
                            actual.map(HexString),
                            HexString(build_id)
                        );
                    }
                }
                Err(error) => {
                    trace!(
                        "try_auto_load({:?}): BinaryData::load_from_fs failed: {}",
                        path,
                        error
                    );
                }
            }
        } else {
            trace!("try_auto_load({:?}): original path does not exist", path);
        }

        // Try the standard .build-id/ debug-file path used by Debian /
        // Ubuntu (and other distros).  Layout:
        //   /usr/lib/debug/.build-id/<2-char-prefix>/<rest>.debug
        let debug_path = Self::build_id_debug_path(build_id);
        if debug_path.exists() && debug_path != original_path {
            match BinaryData::load_from_fs(&debug_path) {
                Ok(debug_binary) => {
                    if debug_binary.build_id() == Some(build_id) {
                        let debug_binary = Arc::new(debug_binary);
                        debug!("Loaded debug symbols from .build-id path: {:?}", debug_path);
                        self.by_build_id
                            .entry(build_id.to_vec())
                            .or_default()
                            .push(debug_binary.clone());
                        // Prefer the debug file (it has DWARF sections)
                        result = Some(debug_binary);
                    } else {
                        trace!(
                            "try_auto_load({:?}): {:?} build_id mismatch",
                            path,
                            debug_path
                        );
                    }
                }
                Err(error) => {
                    trace!(
                        "try_auto_load({:?}): failed to load {:?}: {}",
                        path,
                        debug_path,
                        error
                    );
                }
            }
        } else {
            trace!(
                "try_auto_load({:?}): {:?} does not exist on disk",
                path,
                debug_path
            );
        }

        // Also try the debuglink path under /usr/lib/debug if we have one.
        if let Some(debuglink) = debuglink {
            if let Ok(debuglink_str) = std::str::from_utf8(debuglink) {
                let debuglink_path = Path::new("/usr/lib/debug").join(debuglink_str);
                if debuglink_path.exists()
                    && debuglink_path != original_path
                    && debuglink_path != debug_path
                {
                    match BinaryData::load_from_fs(&debuglink_path) {
                        Ok(debug_binary) => {
                            if debug_binary.build_id() == Some(build_id) {
                                let debug_binary = Arc::new(debug_binary);
                                debug!(
                                    "Loaded debug symbols from debuglink path: {:?}",
                                    debuglink_path
                                );
                                self.by_build_id
                                    .entry(build_id.to_vec())
                                    .or_default()
                                    .push(debug_binary.clone());
                                result = Some(debug_binary);
                            } else {
                                trace!(
                                    "try_auto_load({:?}): {:?} build_id mismatch",
                                    path,
                                    debuglink_path
                                );
                            }
                        }
                        Err(error) => {
                            trace!(
                                "try_auto_load({:?}): failed to load {:?}: {}",
                                path,
                                debuglink_path,
                                error
                            );
                        }
                    }
                }
            } else {
                trace!(
                    "try_auto_load({:?}): debuglink is not valid UTF-8: {:?}",
                    path,
                    debuglink
                );
            }
        }

        trace!(
            "try_auto_load({:?}): result = {}",
            path,
            if result.is_some() {
                "loaded"
            } else {
                "no debug info found"
            }
        );
        result
    }

    /// Build the standard Debian/Ubuntu debug-file path from a build-id.
    /// Layout: `/usr/lib/debug/.build-id/<2-char-prefix>/<rest>.debug`
    fn build_id_debug_path(build_id: &[u8]) -> PathBuf {
        let mut hex = String::with_capacity(build_id.len() * 2);
        for byte in build_id {
            use std::fmt::Write;
            write!(&mut hex, "{:02x}", byte).unwrap();
        }
        let prefix = &hex[..2];
        let rest = &hex[2..];
        Path::new("/usr/lib/debug")
            .join(".build-id")
            .join(prefix)
            .join(format!("{}.debug", rest))
    }

    fn add_impl(&mut self, done: &mut HashSet<PathBuf>, path: &Path, is_toplevel: bool) {
        if !path.exists() {
            warn!("Failed to load {:?}: file not found", path);
            return;
        }

        let owned_path;
        let mut path: &Path = path;

        if let Ok(target) = path.read_link() {
            // Resolve relative symlink targets against the symlink's
            // parent directory, not the CWD.
            if target.is_relative() {
                if let Some(parent) = path.parent() {
                    owned_path = parent.join(target);
                    path = &owned_path;
                }
            } else {
                owned_path = target;
                path = &owned_path;
            }
        }

        if done.contains(path) {
            return;
        }

        done.insert(path.into());

        if path.is_dir() {
            let dir = match path.read_dir() {
                Ok(dir) => dir,
                Err(error) => {
                    warn!("Cannot read the contents of {:?}: {}", path, error);
                    return;
                }
            };

            for entry in dir.flatten() {
                let path = entry.path();
                self.add_impl(done, &path, false);
            }
        } else if path.is_file() {
            match path.metadata() {
                Ok(metadata) => {
                    if metadata.len() == 0 {
                        return;
                    }
                }
                Err(error) => {
                    warn!("Cannot get the metadata of {:?}: {}", path, error);
                    return;
                }
            };

            let is_elf = File::open(path)
                .and_then(|mut fp| {
                    let mut buffer = [0; 4];
                    fp.read_exact(&mut buffer)?;
                    Ok(buffer)
                })
                .map(|buffer| &buffer == b"\x7FELF");
            match is_elf {
                Ok(false) => (),
                Ok(true) => self.add_file(path),
                Err(error) => {
                    if is_toplevel {
                        warn!("Cannot read the first four bytes of {:?}: {}", path, error);
                    }
                }
            }
        }
    }

    fn add_file(&mut self, path: &Path) {
        match BinaryData::load_from_fs(path) {
            Ok(binary) => {
                let filename = path.file_name().unwrap();
                let binary = Arc::new(binary);
                let filename_key = filename.to_string_lossy().into_owned();
                debug!("Adding a new binary by filename: \"{}\"", filename_key);

                self.by_filename
                    .entry(filename_key.into_bytes())
                    .or_default()
                    .push(binary.clone());
                if let Some(build_id) = binary.build_id() {
                    debug!("Adding a new binary by build_id: {:?}", HexString(build_id));
                    self.by_build_id
                        .entry(build_id.to_owned())
                        .or_default()
                        .push(binary.clone());
                }
            }
            Err(error) => {
                warn!("Cannot read debug symbols from {:?}: {}", path, error);
            }
        }
    }
}
