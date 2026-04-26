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
    inner: ContextKind,
}

enum ContextKind {
    /// DWARF lives directly in the binary (or its dSYM). One context
    /// covers the whole image.
    Direct(DirectContext),
    /// macOS split-debuginfo: the binary's symtab has N_OSO entries
    /// pointing at per-CU `.o` files (or archive members inside
    /// `.rlib`s). We lazily build a per-OSO context the first time we
    /// see an address that falls into that CU.
    Oso(OsoState),
}

struct DirectContext {
    _bytes: Arc<Vec<u8>>,
    /// SAFETY: borrows from `_bytes`. The Arc is Pin-ish (heap-stable)
    /// and the inner is dropped before _bytes is.
    inner: addr2line::Context<addr2line::gimli::EndianSlice<'static, addr2line::gimli::RunTimeEndian>>,
}

struct OsoState {
    /// Symbol→OSO entries, sorted by address. One entry per `N_FUN`
    /// (or `N_STSYM`) record we found in the binary.
    entries: Vec<OsoEntry>,
    objects: Vec<OsoObject>,
    /// Parallel to `objects[]`. `None` = not yet attempted; inner
    /// `None` = attempted and failed (cached so we don't keep retrying).
    contexts: Vec<Option<Option<Box<DirectContext>>>>,
}

struct OsoEntry {
    address: u64,
    size: u64,
    name: Vec<u8>,
    object_idx: usize,
}

struct OsoObject {
    /// Path to the `.o` file or to the `.rlib` archive containing it.
    path: std::path::PathBuf,
    /// Some(member_name) when `path` is an archive (`.rlib`); the
    /// actual `.o` lives inside it.
    member: Option<Vec<u8>>,
}

/// On macOS, `cargo build`'s default leaves DWARF in the per-CU `.o`
/// files; the binary itself ships only OSO references. `dsymutil <bin>`
/// consolidates everything into a `.dSYM` bundle. We probe that path
/// and only use it if it's at least as fresh as the binary — a stale
/// dSYM left over from a prior build would otherwise shadow the OSO
/// follower's correct data.
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
    let bin_mtime = std::fs::metadata(binary).and_then(|m| m.modified()).ok();
    for c in &candidates {
        let dsym_mtime = match std::fs::metadata(c).and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Some(bin_mt) = bin_mtime {
            if dsym_mtime < bin_mt {
                tracing::debug!(
                    "source: dSYM {} is older than binary, ignoring",
                    c.display()
                );
                continue;
            }
        }
        if let Ok(b) = std::fs::read(c) {
            tracing::debug!("source: using dSYM at {}", c.display());
            return Some(Arc::new(b));
        }
    }
    None
}

impl OwnedContext {
    fn build(binary_path: &str, image_bytes: Arc<Vec<u8>>) -> Option<Self> {
        // 1. Sibling .dSYM beats embedded DWARF (and is the macOS
        //    convention when the user has run dsymutil).
        if let Some(dsym_bytes) = dwarf_bytes_for(binary_path) {
            if let Some(direct) = DirectContext::build(dsym_bytes) {
                return Some(OwnedContext {
                    inner: ContextKind::Direct(direct),
                });
            }
        }
        // 2. DWARF embedded in the binary itself (ELF, or macOS with
        //    -C split-debuginfo=off).
        if let Some(direct) = DirectContext::try_build_with_dwarf(image_bytes.clone()) {
            return Some(OwnedContext {
                inner: ContextKind::Direct(direct),
            });
        }
        // 3. macOS split-debuginfo: walk OSO references and lazily
        //    open per-CU `.o` files for queries that hit them.
        if let Some(state) = OsoState::build(&image_bytes) {
            tracing::debug!(
                "source: {} → OSO mode ({} entries, {} objects)",
                binary_path,
                state.entries.len(),
                state.objects.len()
            );
            return Some(OwnedContext {
                inner: ContextKind::Oso(state),
            });
        }
        None
    }

    pub fn find_location(&mut self, probe: u64) -> Option<(String, u32)> {
        match &mut self.inner {
            ContextKind::Direct(d) => d.find_location(probe),
            ContextKind::Oso(o) => o.find_location(probe),
        }
    }
}

impl DirectContext {
    /// Build a Context, returning `None` if there isn't actually any
    /// DWARF (so the caller can try the next strategy). We probe by
    /// asking gimli for `.debug_info`; an empty section yields a
    /// usable but useless Context.
    fn try_build_with_dwarf(bytes: Arc<Vec<u8>>) -> Option<Self> {
        use object::{Object, ObjectSection};
        let file = object::File::parse(&bytes[..]).ok()?;
        let info = file
            .section_by_name(".debug_info")
            .or_else(|| file.section_by_name("__debug_info"));
        if info.as_ref().map(|s| s.size()).unwrap_or(0) == 0 {
            return None;
        }
        Self::build(bytes)
    }

    fn build(bytes: Arc<Vec<u8>>) -> Option<Self> {
        use addr2line::gimli;
        use object::Object;

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
            let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
            Ok(gimli::EndianSlice::new(leaked, endian))
        };
        let dwarf = gimli::Dwarf::load(load).ok()?;
        let inner = addr2line::Context::from_dwarf(dwarf).ok()?;
        Some(DirectContext {
            _bytes: bytes,
            inner,
        })
    }

    fn find_location(&self, probe: u64) -> Option<(String, u32)> {
        let loc = self.inner.find_location(probe).ok().flatten()?;
        let file = loc.file?.to_owned();
        let line = loc.line?;
        Some((file, line))
    }

    /// Find the `.o`-local address of a symbol named `name`. Used when
    /// translating a binary-side address into the `.o`'s DWARF space.
    fn symbol_address(&self, name: &[u8]) -> Option<u64> {
        use object::{Object, ObjectSymbol};
        let static_bytes: &'static [u8] = unsafe { std::mem::transmute(&self._bytes[..]) };
        let file = object::File::parse(static_bytes).ok()?;
        for sym in file.symbols() {
            if sym.name_bytes().ok() == Some(name) {
                return Some(sym.address());
            }
        }
        None
    }
}

impl OsoState {
    fn build(image_bytes: &[u8]) -> Option<Self> {
        use object::{Object, ObjectMapEntry, ObjectMapFile};
        let file = object::File::parse(image_bytes).ok()?;
        let map = file.object_map();
        let raw_symbols: &[ObjectMapEntry<'_>] = map.symbols();
        if raw_symbols.is_empty() {
            return None;
        }
        let mut entries: Vec<OsoEntry> = raw_symbols
            .iter()
            .map(|e: &ObjectMapEntry<'_>| OsoEntry {
                address: e.address(),
                size: e.size(),
                name: e.name().to_vec(),
                object_idx: e.object_index(),
            })
            .collect();
        entries.sort_by_key(|e| e.address);
        let raw_objects: &[ObjectMapFile<'_>] = map.objects();
        let objects: Vec<OsoObject> = raw_objects
            .iter()
            .map(|f: &ObjectMapFile<'_>| OsoObject {
                path: std::path::PathBuf::from(std::str::from_utf8(f.path()).unwrap_or("")),
                member: f.member().map(|m| m.to_vec()),
            })
            .collect();
        let contexts = std::iter::repeat_with(|| None).take(objects.len()).collect();
        Some(OsoState {
            entries,
            objects,
            contexts,
        })
    }

    fn find_location(&mut self, probe: u64) -> Option<(String, u32)> {
        // Binary search for the function range containing `probe`.
        let idx = match self
            .entries
            .binary_search_by_key(&probe, |e| e.address)
        {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let entry = self.entries.get(idx)?;
        if probe < entry.address || probe >= entry.address.wrapping_add(entry.size) {
            return None;
        }

        let offset_in_fn = probe - entry.address;
        let oso_idx = entry.object_idx;
        let oso_name = entry.name.clone();
        let ctx = self.context_for(oso_idx)?;
        let base_in_o = ctx.symbol_address(&oso_name)?;
        let probe_in_o = base_in_o.wrapping_add(offset_in_fn);
        ctx.find_location(probe_in_o)
    }

    fn context_for(&mut self, idx: usize) -> Option<&DirectContext> {
        if self.contexts[idx].is_none() {
            let obj = &self.objects[idx];
            let built = load_oso_bytes(&obj.path, obj.member.as_deref())
                .and_then(|bytes| DirectContext::build(bytes))
                .map(Box::new);
            self.contexts[idx] = Some(built);
        }
        self.contexts[idx].as_ref()?.as_deref()
    }
}

/// Read a `.o` file off disk; if `member` is set, treat `path` as an
/// archive (`.rlib` / `.a`) and extract the named member.
fn load_oso_bytes(path: &std::path::Path, member: Option<&[u8]>) -> Option<Arc<Vec<u8>>> {
    let bytes = std::fs::read(path).ok()?;
    match member {
        None => Some(Arc::new(bytes)),
        Some(member_name) => {
            let archive = object::read::archive::ArchiveFile::parse(&bytes[..]).ok()?;
            for m in archive.members() {
                let m = m.ok()?;
                if m.name() == member_name {
                    let data = m.data(&bytes[..]).ok()?;
                    return Some(Arc::new(data.to_vec()));
                }
            }
            tracing::debug!(
                "source: archive {} has no member {:?}",
                path.display(),
                String::from_utf8_lossy(member_name)
            );
            None
        }
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
                    "source: no DWARF for {} (no .dSYM, no embedded line info, no OSO refs)",
                    binary_path
                );
            }
            self.contexts.insert(binary_path.to_owned(), ctx);
        }
        let ctx = self.contexts.get_mut(binary_path)?.as_deref_mut()?;
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
        let loaded = read_source(file)
            .map(|s| Arc::new(s.lines().map(str::to_owned).collect()));
        self.sources.insert(file.to_owned(), loaded.clone());
        loaded
    }
}

/// Read a source file, with rust-src remapping. The rustc compiler stamps
/// std/core/alloc paths into DWARF as `/rustc/<commit>/library/...` —
/// those don't exist on the user's box, but the rust-src component does
/// (under `<sysroot>/lib/rustlib/src/rust/library/...`), so we try that
/// translation as a fallback.
fn read_source(file: &str) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(file) {
        return Some(s);
    }
    if let Some(rs_path) = rust_src_translate(file) {
        if let Ok(s) = std::fs::read_to_string(&rs_path) {
            tracing::debug!(
                "source: {} translated to rust-src path {}",
                file,
                rs_path.display()
            );
            return Some(s);
        }
    }
    None
}

fn rust_src_translate(file: &str) -> Option<std::path::PathBuf> {
    let rest = file.strip_prefix("/rustc/")?;
    // rest = "<commit>/library/std/src/sys/unix.rs"
    let (_commit, rel) = rest.split_once('/')?;
    let sysroot = rust_sysroot()?;
    Some(sysroot.join("lib/rustlib/src/rust").join(rel))
}

fn rust_sysroot() -> Option<&'static std::path::Path> {
    use std::sync::OnceLock;
    static SYSROOT: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    SYSROOT
        .get_or_init(|| {
            std::process::Command::new("rustc")
                .arg("--print")
                .arg("sysroot")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| std::path::PathBuf::from(s.trim()))
        })
        .as_deref()
}

impl Default for SourceResolver {
    fn default() -> Self {
        Self::new()
    }
}
