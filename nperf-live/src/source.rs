//! Source-line resolution + on-disk source caching for live annotate.
//!
//! Given the SVMA of an instruction inside a `CodeImage`, returns the
//! best-effort `(file, line)` via DWARF (`addr2line::Context`) plus a
//! highlighted snippet of the source line if we can find it on disk.
//!
//! Both the addr2line context (per binary path) and source-file
//! contents (per absolute path) are cached so repeated annotate ticks
//! don't re-parse DWARF or re-read files. Caches are owned by the live
//! server's `BinaryRegistry` lock and live for the recording session.

use std::collections::HashMap;
use std::sync::Arc;

use crate::binaries::CodeImage;
use crate::highlight::AsmHighlighter;

pub struct SourceResolver {
    /// addr2line contexts, keyed by binary path. `addr2line::Context`
    /// has interior `LazyCell`s that aren't `Sync`, so we keep each
    /// owned (boxed) and access them only while holding the resolver's
    /// outer Mutex. Failed builds are cached as `None`.
    contexts: HashMap<String, Option<Box<OwnedContext>>>,
    /// Source-file contents (lines) keyed by absolute path. `None` is
    /// "tried and failed" (don't keep stat'ing).
    sources: HashMap<String, Option<Arc<Vec<String>>>>,
    /// Highlighter reused across resolves; arborium grammars are
    /// expensive to instantiate per-call.
    hl: AsmHighlighter,
}

/// `addr2line::Context<R>` borrows from an `object::File<'a>` which
/// borrows from the bytes. To cache it we need to own all three. This
/// uses a manual self-referential pattern: we keep the bytes Arc alive,
/// and we trust that the `Context`'s borrowed lifetime really is tied
/// to those bytes (which are immutable).
pub struct OwnedContext {
    _bytes: Arc<Vec<u8>>,
    /// SAFETY: `inner` borrows from `_bytes`. We never expose `inner`
    /// past the OwnedContext's drop, and `_bytes` is `Arc<Vec<u8>>` so
    /// it can't be moved.
    inner: addr2line::Context<addr2line::gimli::EndianSlice<'static, addr2line::gimli::RunTimeEndian>>,
}

/// On macOS, `cargo build`'s default leaves DWARF in the per-CU `.o`
/// files; the binary itself ships only OSO references. `dsymutil <bin>`
/// consolidates everything into a `.dSYM` bundle. We probe that path
/// and fall back to the embedded bytes.
fn dwarf_bytes_for(binary_path: &str) -> Option<Arc<Vec<u8>>> {
    let binary = std::path::Path::new(binary_path);
    let base = binary.file_name()?.to_string_lossy().into_owned();
    let candidates = [
        std::path::PathBuf::from(format!("{}.dSYM", binary_path))
            .join("Contents/Resources/DWARF")
            .join(&base),
        binary
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .join(format!("{}.dSYM", base))
            .join("Contents/Resources/DWARF")
            .join(&base),
    ];
    for c in &candidates {
        if let Ok(b) = std::fs::read(c) {
            tracing::debug!("source: using dSYM at {}", c.display());
            return Some(Arc::new(b));
        }
    }
    None
}

impl OwnedContext {
    fn build(binary_path: &str, fallback_bytes: Arc<Vec<u8>>) -> Option<Self> {
        // Try dSYM first; fall back to the binary's own bytes (works
        // for ELF / for macOS targets that linked DWARF into the
        // executable via `-C split-debuginfo=off`).
        let bytes = dwarf_bytes_for(binary_path).unwrap_or(fallback_bytes);
        Self::build_from(bytes)
    }

    fn build_from(bytes: Arc<Vec<u8>>) -> Option<Self> {
        use addr2line::gimli;
        use object::Object;

        // Safety: we cast the bytes slice to 'static. We promise to
        // never let the Context outlive `_bytes` (we hold the Arc as a
        // sibling field).
        let static_bytes: &'static [u8] = unsafe { std::mem::transmute(&bytes[..]) };
        let file = object::File::parse(static_bytes).ok()?;
        let endian = if file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };
        let load = |id: gimli::SectionId| -> Result<gimli::EndianSlice<'static, gimli::RunTimeEndian>, ()> {
            use object::ObjectSection;
            let data = file
                .section_by_name(id.name())
                .and_then(|s| s.uncompressed_data().ok())
                .map(|d| d.into_owned())
                .unwrap_or_default();
            // We need 'static; box-leak the data so the slice survives.
            let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
            Ok(gimli::EndianSlice::new(leaked, endian))
        };
        let dwarf = gimli::Dwarf::load(load).ok()?;
        let inner = addr2line::Context::from_dwarf(dwarf).ok()?;
        Some(OwnedContext {
            _bytes: bytes,
            inner,
        })
    }

    pub fn find_location(&self, probe: u64) -> Option<(String, u32)> {
        let loc = self.inner.find_location(probe).ok().flatten()?;
        let file = loc.file?.to_owned();
        let line = loc.line?;
        Some((file, line))
    }
}

impl SourceResolver {
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            sources: HashMap::new(),
            hl: AsmHighlighter::new_for_source(),
        }
    }

    /// Look up the (file, line) for `svma` inside `image`. Returns
    /// `None` if the binary has no DWARF, or DWARF has no entry for
    /// this address.
    pub fn locate(
        &mut self,
        binary_path: &str,
        image: &Arc<CodeImage>,
        svma: u64,
    ) -> Option<(String, u32)> {
        // Build (or reuse) the context, then immediately query it. We
        // can't return a borrow from `&mut self` here because the
        // borrow checker doesn't know `self.contexts` won't be touched
        // again during the call.
        if !self.contexts.contains_key(binary_path) {
            let ctx = OwnedContext::build(binary_path, image.bytes.clone()).map(Box::new);
            if ctx.is_none() {
                tracing::debug!(
                    "source: no DWARF for {} (no .dSYM and no embedded line info)",
                    binary_path
                );
            }
            self.contexts.insert(binary_path.to_owned(), ctx);
        }
        let ctx = self.contexts.get(binary_path)?.as_deref()?;
        ctx.find_location(svma)
    }

    /// Highlighted snippet for `(file, line_1based)`. Returns an empty
    /// string when the file isn't loadable from disk.
    pub fn snippet(&mut self, file: &str, line: u32) -> String {
        let lines = self.source_lines(file);
        let raw = match lines.as_ref().and_then(|v| v.get(line.saturating_sub(1) as usize)) {
            Some(s) => s.trim().to_owned(),
            None => return String::new(),
        };
        let lang = arborium::detect_language(file).unwrap_or("rust");
        self.hl.highlight_in(lang, &raw)
    }

    fn source_lines(&mut self, file: &str) -> Option<Arc<Vec<String>>> {
        if let Some(entry) = self.sources.get(file) {
            return entry.clone();
        }
        let loaded = std::fs::read_to_string(file)
            .ok()
            .map(|s| Arc::new(s.lines().map(str::to_owned).collect()));
        self.sources.insert(file.to_owned(), loaded.clone());
        loaded
    }
}

impl Default for SourceResolver {
    fn default() -> Self {
        Self::new()
    }
}
