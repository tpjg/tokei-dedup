//! Shared types used across tokei-dedup crates.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Identifies a language using its tokei key (e.g. `"Rust"`, `"Python"`, `"Html"`).
///
/// Borrowed against the static `lang-config` data — interned strings keep this `Copy`-cheap.
pub type LangId = &'static str;

/// A normalized token emitted by the FSM.
///
/// `text` is `None` when the token has been blinded (literals/identifiers replaced by their
/// kind under `BlindMode::Mild` or `BlindMode::Aggressive`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NormalizedToken {
    pub kind: TokenKind,
    pub text: Option<String>,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line: u32,
    pub col: u32,
    /// Stack of embedded-language transitions active at this token, outermost first.
    /// `[]` means top-level in the file's own language.
    pub lang_stack: Vec<LangId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum TokenKind {
    Ident,
    LitString,
    LitNumber,
    LitChar,
    Operator,
    Punctuation,
    /// Whitespace token — emitted only in `strict` mode for fidelity; usually dropped.
    Whitespace,
    /// Marks a transition into an embedded language. Carries the language being pushed.
    LangPush(LangId),
    /// Marks the exit from an embedded language.
    LangPop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum BlindMode {
    /// Keep all token text verbatim. Type-1 clones only.
    Strict,
    /// Replace literal text with their kind. Type-1 and literal-renamed Type-2.
    #[default]
    Mild,
    /// Replace identifiers too. Full Type-2.
    Aggressive,
}

/// Counts that match tokei's reported semantics (per file).
///
/// Used by the milestone-0 cross-validation harness to confirm our FSM tracks the same
/// states as tokei.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LineCounts {
    pub code: usize,
    pub comment: usize,
    pub blank: usize,
}

impl LineCounts {
    pub fn total(&self) -> usize {
        self.code + self.comment + self.blank
    }
}
