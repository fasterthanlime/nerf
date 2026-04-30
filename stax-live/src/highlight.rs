//! Syntax highlighting via arborium, returning classified token runs.
//!
//! `TokenHighlighter` is not `Send`/`Sync` so we keep one per task;
//! constructing it is cheap (the grammar is statically linked through
//! the `arborium-asm` and other `arborium-*` crates).
//!
//! We deliberately don't ship arborium's HTML output across the wire.
//! Frontends each have their own styling primitive (CSS classes,
//! SwiftUI `Color`, ANSI, …) — letting them translate the canonical
//! class enum themselves keeps the server out of the rendering loop.

use arborium::Highlighter;
use arborium::advanced::Span;
use arborium_theme::highlights::{ThemeSlot, capture_to_slot};
use stax_live_proto::{Token, TokenClass};

pub struct TokenHighlighter {
    inner: Highlighter,
}

impl TokenHighlighter {
    pub fn new() -> Self {
        Self {
            inner: Highlighter::new(),
        }
    }

    /// Construct a highlighter aimed at arbitrary languages. Same arborium
    /// instance, just a different default-language convention.
    pub fn new_for_source() -> Self {
        Self::new()
    }

    /// Tokenize one line of assembly.
    pub fn highlight_line(&mut self, asm: &str) -> Vec<Token> {
        self.highlight_in("asm", asm)
    }

    /// Tokenize `text` in `lang` (an arborium language id like "rust",
    /// "c", "cpp"). Falls back to a single `Plain` token on parse error
    /// so the UI always has something to render.
    pub fn highlight_in(&mut self, lang: &str, text: &str) -> Vec<Token> {
        match self.inner.highlight_spans(lang, text) {
            Ok(spans) => spans_to_tokens(text, spans),
            Err(_) => plain(text),
        }
    }
}

impl Default for TokenHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

fn plain(text: &str) -> Vec<Token> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Token {
            text: text.to_owned(),
            kind: TokenClass::Plain,
        }]
    }
}

/// Convert arborium spans into a flat token sequence covering every
/// byte of `text`. Overlapping spans pick the one with the highest
/// `pattern_index` (matches tree-sitter's "later patterns win"); gaps
/// are emitted as `Plain` tokens; adjacent same-class runs are
/// coalesced.
fn spans_to_tokens(text: &str, mut spans: Vec<Span>) -> Vec<Token> {
    if text.is_empty() {
        return Vec::new();
    }
    if spans.is_empty() {
        return plain(text);
    }

    // Sort by start, then by pattern_index descending so the higher-
    // priority span comes first when ranges overlap.
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.pattern_index.cmp(&a.pattern_index)));

    // Walk left-to-right, keeping a cursor into `text`. For each span:
    // emit a Plain run for any uncovered bytes before it; emit the
    // span's classified text; advance the cursor past it. Spans that
    // start before the cursor (already covered by a higher-priority
    // span) are skipped.
    let bytes = text.as_bytes();
    let mut out: Vec<Token> = Vec::with_capacity(spans.len() * 2 + 1);
    let mut cursor: usize = 0;

    for span in spans {
        let start = span.start as usize;
        let end = span.end as usize;
        if end <= cursor || start >= bytes.len() {
            continue;
        }
        let start = start.max(cursor);
        let end = end.min(bytes.len());
        if start > cursor {
            push_run(&mut out, &text[cursor..start], TokenClass::Plain);
        }
        let kind = slot_to_class(capture_to_slot(&span.capture));
        push_run(&mut out, &text[start..end], kind);
        cursor = end;
    }
    if cursor < bytes.len() {
        push_run(&mut out, &text[cursor..], TokenClass::Plain);
    }
    out
}

fn push_run(out: &mut Vec<Token>, text: &str, kind: TokenClass) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.kind == kind
    {
        last.text.push_str(text);
        return;
    }
    out.push(Token {
        text: text.to_owned(),
        kind,
    });
}

fn slot_to_class(slot: ThemeSlot) -> TokenClass {
    match slot {
        ThemeSlot::Keyword => TokenClass::Keyword,
        ThemeSlot::Function => TokenClass::Function,
        ThemeSlot::String => TokenClass::String,
        ThemeSlot::Comment => TokenClass::Comment,
        ThemeSlot::Type => TokenClass::Type,
        ThemeSlot::Variable => TokenClass::Variable,
        ThemeSlot::Constant => TokenClass::Constant,
        ThemeSlot::Number => TokenClass::Number,
        ThemeSlot::Operator => TokenClass::Operator,
        ThemeSlot::Punctuation => TokenClass::Punctuation,
        ThemeSlot::Property => TokenClass::Property,
        ThemeSlot::Attribute => TokenClass::Attribute,
        ThemeSlot::Tag => TokenClass::Tag,
        ThemeSlot::Macro => TokenClass::Macro,
        ThemeSlot::Label => TokenClass::Label,
        ThemeSlot::Namespace => TokenClass::Namespace,
        ThemeSlot::Constructor => TokenClass::Constructor,
        ThemeSlot::Title => TokenClass::Title,
        ThemeSlot::Strong => TokenClass::Strong,
        ThemeSlot::Emphasis => TokenClass::Emphasis,
        ThemeSlot::Link => TokenClass::Link,
        ThemeSlot::Literal => TokenClass::Literal,
        ThemeSlot::Strikethrough => TokenClass::Strikethrough,
        ThemeSlot::DiffAdd => TokenClass::DiffAdd,
        ThemeSlot::DiffDelete => TokenClass::DiffDelete,
        ThemeSlot::Embedded => TokenClass::Embedded,
        ThemeSlot::Error => TokenClass::Error,
        ThemeSlot::None => TokenClass::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_simple_mov() {
        let mut hl = TokenHighlighter::new();
        let tokens = hl.highlight_line("mov rax, 0x42");
        // Concatenated text round-trips losslessly.
        let joined: String = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "mov rax, 0x42");
        // Some token must be classified — exact class depends on the
        // grammar but at minimum it isn't all Plain.
        assert!(
            tokens.iter().any(|t| t.kind != TokenClass::Plain),
            "expected some classified tokens, got {tokens:?}"
        );
    }

    #[test]
    fn falls_back_to_plain_for_unknown_lang() {
        let mut hl = TokenHighlighter::new();
        let tokens = hl.highlight_in("nonsense-lang", "hello world");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].text, "hello world");
        assert_eq!(tokens[0].kind, TokenClass::Plain);
    }

    #[test]
    fn empty_input_yields_no_tokens() {
        let mut hl = TokenHighlighter::new();
        assert!(hl.highlight_line("").is_empty());
    }
}
