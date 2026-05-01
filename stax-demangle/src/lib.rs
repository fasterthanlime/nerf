//! Shared symbol demangler used by `nwind` (the offline report path)
//! and `stax-live` (the live RPC path).
//!
//! The toolchain feeds us mangled bytes coming from a few different
//! object-format conventions:
//!   * Mach-O usually adds a leading `_` to user-namespace symbols.
//!     ELF does not.
//!   * GCC IPA-SRA emits `<sym>.isra.<n>` suffixes that confuse
//!     `cpp_demangle`; we strip them.
//!   * `_GLOBAL__sub_I_<sym>` denotes a global initializer wrapping
//!     `<sym>` — we want the inner name + a "global init " prefix.
//!
//! `symbolic-demangle` handles the actual mangling schemes (Rust v0/legacy,
//! Itanium C++, Swift, MSVC). We just preprocess + post-format.
//!
//! Itanium-style `_Z…` symbols are ambiguous (legacy Rust mangling
//! reuses the Itanium framing). symbolic-demangle's autodetect tends
//! to pick Rust eagerly and yields truncated output for actual C++.
//! So we always run them through the C++ demangler first, then look
//! for the legacy Rust `::h<16hex>` hash suffix to upgrade the
//! classification when present.

use symbolic_common::{Language as SymLanguage, Name, NameMangling};
use symbolic_demangle::{Demangle, DemangleOptions};

/// Source language of a symbol. Re-exposed on top of
/// `symbolic_common::Language` so callers don't have to pull
/// `symbolic_common` themselves.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Cpp,
    Swift,
    ObjC,
    ObjCpp,
    C,
    #[default]
    Unknown,
}

impl Language {
    /// Stable lowercase token used by the wire protocol so the
    /// frontend can pick an icon directly (`"rust"`, `"swift"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Cpp => "cpp",
            Language::Swift => "swift",
            Language::ObjC => "objc",
            Language::ObjCpp => "objcpp",
            Language::C => "c",
            Language::Unknown => "unknown",
        }
    }
}

/// Result of demangling a single symbol.
#[derive(Debug, Clone)]
pub struct Demangled {
    /// Best-effort human-readable name. Falls back to the cleaned-up
    /// raw string when no demangler succeeds.
    pub name: String,
    pub language: Language,
}

fn strip_isra(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    let suffix = b".isra.";
    if end >= suffix.len() && end < bytes.len() && &bytes[end - suffix.len()..end] == suffix {
        return unsafe { std::str::from_utf8_unchecked(&bytes[..end - suffix.len()]) };
    }
    s
}

fn strip_global_init(s: &str) -> Option<&str> {
    s.strip_prefix("_GLOBAL__sub_I_")
}

/// `::h` followed by exactly 16 hex digits, anchored at the end —
/// the legacy Rust mangling hash that `rustc-demangle`'s alt
/// formatter (`{:#}`) hides. We use its presence to upgrade an
/// Itanium-shaped symbol's language from C++ to Rust.
fn rust_hash_stem(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let stem_end = bytes.len() - 19;
    if &bytes[stem_end..stem_end + 3] != b"::h" {
        return None;
    }
    if !bytes[stem_end + 3..].iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(&s[..stem_end])
}

/// Try one specific language. Returns `None` when the demangler
/// either rejected the symbol or returned the input unchanged.
fn try_lang(input: &str, lang: SymLanguage) -> Option<String> {
    let name = Name::new(input, NameMangling::Mangled, lang);
    let demangled = name.demangle(DemangleOptions::name_only())?;
    if demangled == input {
        None
    } else {
        Some(demangled)
    }
}

fn classify(input: &str) -> Demangled {
    // Unambiguous prefixes first.
    if input.starts_with("_R") {
        if let Some(d) = try_lang(input, SymLanguage::Rust) {
            return Demangled {
                name: d,
                language: Language::Rust,
            };
        }
    }
    if input.starts_with("_$s") || input.starts_with("$s") {
        if let Some(d) = try_lang(input, SymLanguage::Swift) {
            return Demangled {
                name: d,
                language: Language::Swift,
            };
        }
    }

    // Itanium / legacy Rust. Run C++ first so we get the full path,
    // then look for the legacy Rust hash suffix to reclassify; in
    // that case prefer the Rust demangler's output so punycode-style
    // escapes (`$LT$`, `$u7b$`, …) get translated back to `<`, `{`,
    // etc.
    if let Some(cpp_out) = try_lang(input, SymLanguage::Cpp) {
        if rust_hash_stem(&cpp_out).is_some() {
            if let Some(rust_out) = try_lang(input, SymLanguage::Rust) {
                return Demangled {
                    name: rust_out,
                    language: Language::Rust,
                };
            }
            // Rust demangler refused — fall back to stripping the hash
            // off the C++ output so we still hide it from users.
            if let Some(stem) = rust_hash_stem(&cpp_out) {
                return Demangled {
                    name: stem.to_owned(),
                    language: Language::Rust,
                };
            }
        }
        return Demangled {
            name: cpp_out,
            language: Language::Cpp,
        };
    }

    // Only try secondary languages when the symbol looks mangled.
    // Plain C names (main, pow, kevent, …) have no leading
    // underscore and can't possibly demangle as anything else;
    // guarding on the underscore saves three doomed attempts per
    // lookup for every C-library and syscall symbol.
    if input.as_bytes().first() == Some(&b'_') {
        // Some Rust v0 inputs aren't recognized by cpp_demangle.
        if let Some(d) = try_lang(input, SymLanguage::Rust) {
            return Demangled {
                name: d,
                language: Language::Rust,
            };
        }
        if let Some(d) = try_lang(input, SymLanguage::Swift) {
            return Demangled {
                name: d,
                language: Language::Swift,
            };
        }
        if let Some(d) = try_lang(input, SymLanguage::ObjC) {
            return Demangled {
                name: d,
                language: Language::ObjC,
            };
        }
    }

    Demangled {
        name: input.to_owned(),
        language: Language::Unknown,
    }
}

/// Demangle one mangled symbol. Accepts bytes; non-UTF8 input is
/// passed through as a lossy string.
pub fn demangle_bytes(raw: &[u8]) -> Demangled {
    match std::str::from_utf8(raw) {
        Ok(s) => demangle_str(s),
        Err(_) => Demangled {
            name: String::from_utf8_lossy(raw).into_owned(),
            language: Language::Unknown,
        },
    }
}

/// Demangle one mangled symbol. Tries the input as-is, then with the
/// Mach-O leading underscore stripped — keeps whichever variant's
/// language was actually detected.
pub fn demangle_str(raw: &str) -> Demangled {
    let cleaned = strip_isra(raw);
    let (target, is_global_init) = match strip_global_init(cleaned) {
        Some(inner) => (inner, true),
        None => (cleaned, false),
    };

    let direct = classify(target);

    // The Mach-O leading underscore convention means a Rust v0 (`_R`)
    // or Swift (`_$s`) symbol is recognizable only when we keep the
    // underscore. But Itanium C++ mangling (`_Z…`) and legacy Rust
    // (`_Z…`) both want it kept too. So `direct` is usually the right
    // answer; we only fall through to the underscore-stripped
    // attempt if the direct one came back Unknown.
    let chosen = if direct.language != Language::Unknown {
        direct
    } else if let Some(stripped_input) = target.strip_prefix('_') {
        let stripped = classify(stripped_input);
        if stripped.language != Language::Unknown {
            stripped
        } else {
            // Neither classified — return the cleanest raw string.
            Demangled {
                name: stripped_input.to_owned(),
                language: Language::Unknown,
            }
        }
    } else {
        direct
    };

    if is_global_init {
        Demangled {
            name: format!("global init {}", chosen.name),
            language: chosen.language,
        }
    } else {
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_legacy() {
        let d = demangle_str("_ZN4core3ptr18real_drop_in_place17h12ad72ac936a11ecE");
        assert_eq!(d.name, "core::ptr::real_drop_in_place");
        assert_eq!(d.language, Language::Rust);
    }

    #[test]
    fn rust_legacy_punycode_translation() {
        // `$LT$T$GT$` → `<T>` and `$u7b$$u7b$closure$u7d$$u7d$` →
        // `{{closure}}`. The C++ demangler leaves these literal; we
        // need to defer to the Rust demangler when the legacy hash
        // suffix is present.
        let d =
            demangle_str("_ZN5alloc7raw_vec15RawVec$LT$T$GT$14from_raw_parts17h2c9379b27997b67cE");
        assert_eq!(d.name, "alloc::raw_vec::RawVec<T>::from_raw_parts");
        assert_eq!(d.language, Language::Rust);

        let d = demangle_str("_ZN12panic_unwind3imp14find_eh_action28_$u7b$$u7b$closure$u7d$$u7d$17hd5299eb0542f59b0E");
        assert_eq!(d.name, "panic_unwind::imp::find_eh_action::{{closure}}");
        assert_eq!(d.language, Language::Rust);
    }

    #[test]
    fn rust_v0() {
        let d = demangle_str("_RNvNtNtCsgEmfK2I1SDS_4core3str8converts9from_utf8");
        assert_eq!(d.name, "core::str::converts::from_utf8");
        assert_eq!(d.language, Language::Rust);
    }

    #[test]
    fn cpp_namespace() {
        let d = demangle_str("_ZN9nsGkAtoms4headE");
        assert_eq!(d.name, "nsGkAtoms::head");
        assert_eq!(d.language, Language::Cpp);
    }

    #[test]
    fn cpp_isra_suffix() {
        let d = demangle_str(
            "_ZNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEE12_M_constructIPcEEvT_S7_St20forward_iterator_tag.isra.90",
        );
        assert!(d.name.contains("std::__cxx11::basic_string"), "got: {d:?}");
        assert_eq!(d.language, Language::Cpp);
    }

    #[test]
    fn swift_v5() {
        let d =
            demangle_str("_$s7SwiftUI4ViewPAAE7overlay_9alignmentQrqd___AA9AlignmentVtAaBRd__lF");
        assert_eq!(d.language, Language::Swift, "got: {d:?}");
        assert!(d.name.contains("View"), "got: {d:?}");
    }

    #[test]
    fn global_init_wrapping() {
        let d = demangle_str("_GLOBAL__sub_I__ZN9nsGkAtoms4headE");
        assert!(d.name.starts_with("global init "), "got: {d:?}");
        assert!(d.name.contains("nsGkAtoms::head"), "got: {d:?}");
    }

    #[test]
    fn unknown_passthrough() {
        let d = demangle_str("_main");
        assert_eq!(d.name, "main");
    }

    #[test]
    fn rust_v0_in_global_init() {
        // exercise: GLOBAL wrapper around a v0 Rust name
        let d = demangle_str("_GLOBAL__sub_I__RNvNtNtCsgEmfK2I1SDS_4core3str8converts9from_utf8");
        assert!(d.name.starts_with("global init "), "got: {d:?}");
        assert!(d.name.contains("from_utf8"), "got: {d:?}");
    }
}
