//! Tiny helper kept for downstream consumers (`nerf-mac-kperf-parse`).
//! The samply-derived suspend-and-walk recorder that used to live here
//! has been removed: the daemon backend (`nperfd-client`) is the only
//! sampling path now.

/// Tracks which (tid, name) pairs we've already reported so a recorder
/// only emits a `ThreadNameEvent` when the binding actually changes
/// (or appears for the first time).
pub struct ThreadNameCache {
    seen: std::collections::HashMap<u32, String>,
}

impl ThreadNameCache {
    pub fn new() -> Self {
        Self {
            seen: std::collections::HashMap::new(),
        }
    }

    /// Returns true iff the thread name was newly seen or has changed.
    pub fn note_thread(&mut self, tid: u32, name: &str) -> bool {
        match self.seen.get(&tid) {
            Some(existing) if existing == name => false,
            _ => {
                self.seen.insert(tid, name.to_owned());
                true
            }
        }
    }
}

impl Default for ThreadNameCache {
    fn default() -> Self {
        Self::new()
    }
}
