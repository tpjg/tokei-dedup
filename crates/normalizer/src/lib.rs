//! Single-pass byte-level FSM that normalizes source code into a token stream and produces
//! tokei-compatible line counts.
//!
//! The FSM is driven entirely by data from [`tokei_dedup_lang_config`] — we hold no
//! per-language code. The same loop handles ~330 languages by swapping the compiled
//! marker table per file.
//!
//! Milestone-0 scope (intentionally limited):
//! - Standard (non-literate, non-HTML) languages: full FSM with nested block comments,
//!   verbatim/doc strings, escape handling.
//! - Literate languages (Markdown, Djot, Mdx): fence-aware comment vs code classification.
//! - Embedded languages (HTML `<script>` / `<style>`, Jupyter cells, Markdown fenced code
//!   with a language tag) are **not yet** dispatched to a sub-FSM; they'll be milestone 0.5.
//! - Token emission is functional but minimal: identifiers, numbers, single-byte
//!   punctuation, and string literals. Multi-byte operator coalescing comes with the
//!   fingerprinter (n-grams subsume it).

use tokei_dedup_core::{BlindMode, LineCounts, NormalizedToken, TokenKind};
use tokei_dedup_lang_config::{self as lang_config, LanguageDef};

#[derive(Debug, Default, Clone)]
pub struct NormalizerOutput {
    pub tokens: Vec<NormalizedToken>,
    pub counts: LineCounts,
}

pub struct Normalizer {
    blind_mode: BlindMode,
}

impl Default for Normalizer {
    fn default() -> Self {
        Self { blind_mode: BlindMode::Mild }
    }
}

impl Normalizer {
    pub fn new(blind_mode: BlindMode) -> Self {
        Self { blind_mode }
    }

    pub fn blind_mode(&self) -> BlindMode {
        self.blind_mode
    }

    /// Look up the language by tokei key and process the source. Returns empty output if
    /// the key is unknown.
    pub fn process(&self, src: &str, lang_key: &str) -> NormalizerOutput {
        match lang_config::by_key(lang_key) {
            Some(lang) => self.process_with(src, lang),
            None => NormalizerOutput::default(),
        }
    }

    pub fn process_with(&self, src: &str, lang: &LanguageDef) -> NormalizerOutput {
        let compiled = CompiledLang::compile(lang);
        if compiled.literate {
            process_literate(src.as_bytes())
        } else {
            process_standard(src.as_bytes(), &compiled, self.blind_mode)
        }
    }
}

// --- Compiled per-language marker tables -------------------------------------------------

struct CompiledLang<'a> {
    line_comments: Vec<&'a [u8]>,
    /// `(start, end)` pairs, sorted longest-start-first.
    block_comments: Vec<(&'a [u8], &'a [u8])>,
    nested: bool,
    /// `(start, end, allows_escape)`. Verbatim quotes set `allows_escape = false`.
    quotes: Vec<(&'a [u8], &'a [u8], bool)>,
    literate: bool,
}

impl<'a> CompiledLang<'a> {
    fn compile(lang: &'a LanguageDef) -> Self {
        let mut line_comments: Vec<&[u8]> =
            lang.line_comment.iter().map(|s| s.as_bytes()).collect();
        line_comments.sort_by_key(|s| std::cmp::Reverse(s.len()));

        let mut block_comments: Vec<(&[u8], &[u8])> = lang
            .multi_line_comments
            .iter()
            .chain(lang.nested_comments.iter())
            .map(|p| (p[0].as_bytes(), p[1].as_bytes()))
            .collect();
        block_comments.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));

        let mut quotes: Vec<(&[u8], &[u8], bool)> = Vec::new();
        for q in &lang.quotes {
            quotes.push((q[0].as_bytes(), q[1].as_bytes(), true));
        }
        for q in &lang.verbatim_quotes {
            quotes.push((q[0].as_bytes(), q[1].as_bytes(), false));
        }
        for q in &lang.doc_quotes {
            quotes.push((q[0].as_bytes(), q[1].as_bytes(), true));
        }
        quotes.sort_by_key(|(s, _, _)| std::cmp::Reverse(s.len()));

        Self {
            line_comments,
            block_comments,
            nested: lang.nested,
            quotes,
            literate: lang.literate,
        }
    }
}

// --- State machine -----------------------------------------------------------------------

#[derive(Clone, Copy)]
enum State {
    Code,
    LineComment,
    BlockComment { depth: u32, pair_idx: usize },
    Str { quote_idx: usize, token_start: u32 },
}

fn process_standard(src: &[u8], lang: &CompiledLang, blind: BlindMode) -> NormalizerOutput {
    let mut state = State::Code;
    let mut tokens: Vec<NormalizedToken> = Vec::new();
    let mut counts = LineCounts::default();
    let mut i: usize = 0;
    let mut line: u32 = 1;
    let mut line_start: u32 = 0;
    let mut line_has_code = false;
    let mut line_has_comment = false;
    let mut saw_any_byte_on_line = false;

    while i < src.len() {
        let b = src[i];
        if b != b'\n' {
            saw_any_byte_on_line = true;
        }

        if b == b'\n' {
            commit(&mut counts, line_has_code, line_has_comment);
            line += 1;
            line_start = (i + 1) as u32;
            line_has_code = false;
            line_has_comment = false;
            saw_any_byte_on_line = false;
            // A line that *opens* inside a non-Code state inherits classification:
            // - still in a string → that line has code (string content)
            // - still in a block comment → that line has comment content
            // - line comment terminates at the newline
            match state {
                State::Str { .. } => line_has_code = true,
                State::BlockComment { .. } => line_has_comment = true,
                State::LineComment => state = State::Code,
                State::Code => {}
            }
            i += 1;
            continue;
        }

        match state {
            State::Code => {
                if let Some(len) = match_first_of(&lang.line_comments, src, i) {
                    line_has_comment = true;
                    state = State::LineComment;
                    i += len;
                    continue;
                }
                if let Some((idx, len)) = match_first_pair(&lang.block_comments, src, i) {
                    line_has_comment = true;
                    state = State::BlockComment { depth: 1, pair_idx: idx };
                    i += len;
                    continue;
                }
                if let Some((idx, len)) = match_first_quote(&lang.quotes, src, i) {
                    line_has_code = true;
                    state = State::Str { quote_idx: idx, token_start: i as u32 };
                    i += len;
                    continue;
                }
                if b.is_ascii_whitespace() {
                    i += 1;
                    continue;
                }
                // Code byte — emit a token (identifier/number run, or single-byte punctuation).
                line_has_code = true;
                let start = i;
                if is_word_byte(b) {
                    while i < src.len() && is_word_byte(src[i]) {
                        i += 1;
                    }
                    let kind = if b.is_ascii_digit() {
                        TokenKind::LitNumber
                    } else {
                        TokenKind::Ident
                    };
                    push_token(
                        &mut tokens,
                        kind,
                        keep_text(src, start, i, kind, blind),
                        start,
                        i,
                        line,
                        line_start,
                    );
                } else {
                    let text = std::str::from_utf8(&src[start..start + 1])
                        .map(str::to_string)
                        .ok();
                    push_token(
                        &mut tokens,
                        TokenKind::Punctuation,
                        text,
                        start,
                        start + 1,
                        line,
                        line_start,
                    );
                    i += 1;
                }
            }
            State::LineComment => {
                i += 1;
            }
            State::BlockComment { depth, pair_idx } => {
                line_has_comment = true;
                let (start_m, end_m) = lang.block_comments[pair_idx];
                if starts_with(src, i, end_m) {
                    let new_depth = depth - 1;
                    i += end_m.len();
                    state = if new_depth == 0 {
                        State::Code
                    } else {
                        State::BlockComment { depth: new_depth, pair_idx }
                    };
                } else if lang.nested && starts_with(src, i, start_m) {
                    state = State::BlockComment { depth: depth + 1, pair_idx };
                    i += start_m.len();
                } else {
                    i += 1;
                }
            }
            State::Str { quote_idx, token_start } => {
                line_has_code = true;
                let (_start_m, end_m, allows_escape) = lang.quotes[quote_idx];
                if allows_escape && b == b'\\' && i + 1 < src.len() {
                    // Skip the escaped byte (but not a newline — backslash-newline is fine
                    // either way: it still terminates the "logical" line for our counter).
                    i += 2;
                } else if starts_with(src, i, end_m) {
                    let tok_end = i + end_m.len();
                    let text = match blind {
                        BlindMode::Strict => std::str::from_utf8(&src[token_start as usize..tok_end])
                            .map(str::to_string)
                            .ok(),
                        _ => None,
                    };
                    let col = token_start.saturating_sub(line_start) + 1;
                    tokens.push(NormalizedToken {
                        kind: TokenKind::LitString,
                        text,
                        byte_start: token_start,
                        byte_end: tok_end as u32,
                        line,
                        col,
                        lang_stack: vec![],
                    });
                    state = State::Code;
                    i = tok_end;
                } else {
                    i += 1;
                }
            }
        }
    }

    if saw_any_byte_on_line || line_has_code || line_has_comment {
        commit(&mut counts, line_has_code, line_has_comment);
    }

    NormalizerOutput { tokens, counts }
}

// --- Literate (Markdown-class) mode ------------------------------------------------------
//
// Tokei's literate semantics (reverse-engineered empirically):
//
// - Lines outside fences: blank if empty/whitespace, else `comment` (prose).
// - Fence delimiter lines (``` / ~~~): `comment`.
// - Lines inside a fence **with a language tag** (e.g. ```` ```rust ````): excluded from
//   outer counts entirely — tokei extracts these into a sub-language `CodeStats` reachable
//   via `stats.blobs`. To match the outer-view counts we simply drop them.
// - Lines inside a fence **without a language tag**: counted as `comment`.
//
// Milestone 0 collapses sub-language extraction to "drop". Milestone 0.5 will emit
// `LangPush`/`LangPop` markers around the inner stream so a downstream consumer can
// re-tokenize the inner content in the named language.

fn process_literate(src: &[u8]) -> NormalizerOutput {
    let mut counts = LineCounts::default();
    let mut in_fence = false;
    let mut fence_has_lang = false;

    for raw_line in split_lines(src) {
        let trimmed = trim_ws(raw_line);
        if trimmed.is_empty() {
            counts.blank += 1;
            continue;
        }
        let fence_marker = (trimmed.starts_with(b"```") || trimmed.starts_with(b"~~~"))
            .then_some(3usize);
        if let Some(marker_len) = fence_marker {
            counts.comment += 1;
            if !in_fence {
                fence_has_lang = !trim_ws(&trimmed[marker_len..]).is_empty();
                in_fence = true;
            } else {
                in_fence = false;
                fence_has_lang = false;
            }
            continue;
        }
        if in_fence {
            if fence_has_lang {
                // Excluded from outer counts — tokei would attribute this line to the
                // tagged sub-language's stats. We drop it for milestone 0.
            } else {
                counts.comment += 1;
            }
        } else {
            counts.comment += 1;
        }
    }

    NormalizerOutput { tokens: vec![], counts }
}

// --- Helpers -----------------------------------------------------------------------------

fn commit(counts: &mut LineCounts, has_code: bool, has_comment: bool) {
    if has_code {
        counts.code += 1;
    } else if has_comment {
        counts.comment += 1;
    } else {
        counts.blank += 1;
    }
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn starts_with(src: &[u8], at: usize, needle: &[u8]) -> bool {
    src.len() >= at + needle.len() && &src[at..at + needle.len()] == needle
}

fn match_first_of(needles: &[&[u8]], src: &[u8], at: usize) -> Option<usize> {
    needles.iter().find(|n| starts_with(src, at, n)).map(|n| n.len())
}

fn match_first_pair(pairs: &[(&[u8], &[u8])], src: &[u8], at: usize) -> Option<(usize, usize)> {
    pairs
        .iter()
        .enumerate()
        .find_map(|(idx, (s, _))| starts_with(src, at, s).then_some((idx, s.len())))
}

fn match_first_quote(
    quotes: &[(&[u8], &[u8], bool)],
    src: &[u8],
    at: usize,
) -> Option<(usize, usize)> {
    quotes
        .iter()
        .enumerate()
        .find_map(|(idx, (s, _, _))| starts_with(src, at, s).then_some((idx, s.len())))
}

fn trim_ws(b: &[u8]) -> &[u8] {
    let mut s = 0;
    while s < b.len() && b[s].is_ascii_whitespace() {
        s += 1;
    }
    let mut e = b.len();
    while e > s && b[e - 1].is_ascii_whitespace() {
        e -= 1;
    }
    &b[s..e]
}

fn split_lines(src: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Treat both LF-terminated and final-line-without-LF correctly.
    let mut start = 0;
    let mut out = Vec::new();
    for (i, &b) in src.iter().enumerate() {
        if b == b'\n' {
            out.push(&src[start..i]);
            start = i + 1;
        }
    }
    if start < src.len() {
        out.push(&src[start..]);
    }
    out.into_iter()
}

fn keep_text(
    src: &[u8],
    start: usize,
    end: usize,
    kind: TokenKind,
    blind: BlindMode,
) -> Option<String> {
    match (kind, blind) {
        (TokenKind::Ident, BlindMode::Aggressive) => None,
        (TokenKind::LitNumber, BlindMode::Mild | BlindMode::Aggressive) => None,
        _ => std::str::from_utf8(&src[start..end])
            .map(str::to_string)
            .ok(),
    }
}

fn push_token(
    tokens: &mut Vec<NormalizedToken>,
    kind: TokenKind,
    text: Option<String>,
    start: usize,
    end: usize,
    line: u32,
    line_start: u32,
) {
    tokens.push(NormalizedToken {
        kind,
        text,
        byte_start: start as u32,
        byte_end: end as u32,
        line,
        col: (start as u32).saturating_sub(line_start) + 1,
        lang_stack: vec![],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_lang_returns_empty() {
        let out = Normalizer::default().process("anything", "DoesNotExist");
        assert_eq!(out.counts, LineCounts::default());
    }

    #[test]
    fn rust_counts_basic() {
        let src = "// hi\nfn main() {}\n";
        let out = Normalizer::default().process(src, "Rust");
        assert_eq!(out.counts.comment, 1);
        assert_eq!(out.counts.code, 1);
        assert_eq!(out.counts.blank, 0);
    }

    #[test]
    fn rust_nested_block_comment() {
        let src = "/* outer /* inner */ still outer */\nfn x() {}\n";
        let out = Normalizer::default().process(src, "Rust");
        assert_eq!(out.counts.comment, 1);
        assert_eq!(out.counts.code, 1);
    }

    #[test]
    fn multiline_string_is_code() {
        let src = "let s = \"hello\nworld\";\n";
        let out = Normalizer::default().process(src, "Rust");
        // Both lines of the string are code.
        assert_eq!(out.counts.code, 2);
        assert_eq!(out.counts.blank, 0);
        assert_eq!(out.counts.comment, 0);
    }

    #[test]
    fn markdown_fence_with_lang_excludes_inner() {
        let src = "# Title\n\nProse line.\n\n```rust\nfn x() {}\n```\nMore prose.\n";
        let out = Normalizer::default().process(src, "Markdown");
        // Matches tokei's outer view: tagged-fence inner lines are excluded entirely
        // (would be in the Rust sub-stats). Delimiter lines themselves count as comment.
        assert_eq!(out.counts.code, 0);
        assert_eq!(out.counts.comment, 5);
        assert_eq!(out.counts.blank, 2);
    }

    #[test]
    fn markdown_plain_fence_keeps_inner_as_comment() {
        let src = "```\nfoo\nbar\n```\n";
        let out = Normalizer::default().process(src, "Markdown");
        assert_eq!(out.counts.code, 0);
        assert_eq!(out.counts.comment, 4);
        assert_eq!(out.counts.blank, 0);
    }
}
