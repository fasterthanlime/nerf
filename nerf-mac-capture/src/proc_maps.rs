//! Minimal Mach-O symbol type. The full samply-derived dyld walker
//! lived here previously; the only piece other crates depend on now
//! is `MachOSymbol`, kept here so `nerf-mac-kperf-parse` and
//! `nperf-mac-shared-cache` don't need to relocate their imports.
//
// SVMAs throughout: addresses as the linker laid them out in the
// on-disk binary (no ASLR slide applied).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachOSymbol {
    /// Symbol VMA as it appears in the binary's `nlist_64::n_value`
    /// (i.e. before applying any ASLR slide).
    pub start_svma: u64,
    /// Synthesized end SVMA: the start of the next symbol within the
    /// same section, or `start_svma + 4` as a fallback.
    pub end_svma: u64,
    pub name: Vec<u8>,
}
