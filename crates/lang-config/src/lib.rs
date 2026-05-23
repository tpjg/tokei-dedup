//! Vendored copy of tokei's `languages.json`, parsed into typed Rust structs.
//!
//! The JSON itself lives at `vendor/tokei-languages.json` in the workspace root and is
//! embedded at compile time via `include_str!`. To track upstream tokei changes, re-copy
//! that file periodically (see `scripts/sync-tokei-languages.sh`).

use serde::Deserialize;
use std::collections::HashMap;

const TOKEI_LANGUAGES_JSON: &str =
    include_str!("../../../vendor/tokei-languages.json");

/// Top-level shape of `languages.json`.
#[derive(Debug, Deserialize)]
struct LanguagesFile {
    languages: HashMap<String, LanguageDef>,
}

/// A single language entry from tokei's `languages.json`.
///
/// Field names track tokei's JSON keys exactly — see tokei docs for semantics. Most fields
/// are optional because the JSON is sparse: a language with no comments at all (e.g. JSON
/// itself) carries only `extensions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LanguageDef {
    /// Display name, defaults to the key if absent.
    pub name: Option<String>,
    /// File extensions (without leading dot).
    pub extensions: Vec<String>,
    /// Special whole filenames (e.g. `Dockerfile`, `CMakeLists.txt`).
    pub filenames: Vec<String>,
    /// Path suffixes (e.g. Blade's `.blade.php`).
    pub path_suffixes: Vec<String>,
    /// Line-comment prefixes. `//` for C-likes, `#` for Python, `;` for Lisp, etc.
    pub line_comment: Vec<String>,
    /// `[start, end]` pairs for block comments.
    pub multi_line_comments: Vec<[String; 2]>,
    /// True if multi-line comments nest (Rust, Swift, D-style).
    pub nested: bool,
    /// Like `multi_line_comments` but explicitly nesting (D's `/+ +/`).
    pub nested_comments: Vec<[String; 2]>,
    /// `[start, end]` pairs for string literals.
    pub quotes: Vec<[String; 2]>,
    /// `[start, end]` for verbatim strings (no escape sequences) — Rust `r"..."`, C++ `R"(...)"`.
    pub verbatim_quotes: Vec<[String; 2]>,
    /// `[start, end]` for doc strings — Python `"""..."""`.
    pub doc_quotes: Vec<[String; 2]>,
    /// Substrings that mark important syntax — for embedded-language detection.
    /// HTML uses `<script` and `<style`; Markdown uses ` ``` `.
    pub important_syntax: Vec<String>,
    /// Special kind tag — currently only `"html"`.
    pub kind: Option<String>,
    /// True for literate/fence-block files (Markdown, Djot, MDX).
    pub literate: bool,
    /// True for files that should not be line-counted (FEN, Hex, IntelHex).
    pub blank: bool,
    /// Shebang interpreter names.
    pub shebangs: Vec<String>,
    /// MIME types — used by HTML `<script type=...>` to pick the embedded language.
    pub mime: Vec<String>,
    /// `env` shebangs (e.g. `#!/usr/bin/env python3`).
    pub env: Vec<String>,
}

impl LanguageDef {
    /// Returns true if this language has *any* tokei-recognizable syntax to count.
    /// Languages with no comments, strings, or fence rules are essentially "plaintext for tokei."
    pub fn is_trivial(&self) -> bool {
        self.line_comment.is_empty()
            && self.multi_line_comments.is_empty()
            && self.nested_comments.is_empty()
            && self.quotes.is_empty()
            && self.verbatim_quotes.is_empty()
            && self.doc_quotes.is_empty()
            && !self.literate
            && self.kind.is_none()
    }
}

static LANGUAGES: once_cell::sync::Lazy<HashMap<String, LanguageDef>> =
    once_cell::sync::Lazy::new(|| {
        let parsed: LanguagesFile = serde_json::from_str(TOKEI_LANGUAGES_JSON)
            .expect("vendored tokei-languages.json failed to parse");
        let mut out = parsed.languages;
        // Tokei's languages.json is double-escaped: each marker string is intended as a Rust
        // string literal (the JSON is fed through a Tera template that emits Rust source).
        // After JSON parsing, we still need to interpret Rust-style escapes to recover the
        // actual marker bytes (`\"` → `"`, `\\` → `\`, etc.). See tokei's build.rs.
        for def in out.values_mut() {
            normalize_def(def);
        }
        out
    });

fn normalize_def(def: &mut LanguageDef) {
    for s in def.line_comment.iter_mut() {
        *s = unescape_rust(s);
    }
    for pair in def.multi_line_comments.iter_mut() {
        pair[0] = unescape_rust(&pair[0]);
        pair[1] = unescape_rust(&pair[1]);
    }
    for pair in def.nested_comments.iter_mut() {
        pair[0] = unescape_rust(&pair[0]);
        pair[1] = unescape_rust(&pair[1]);
    }
    for pair in def.quotes.iter_mut() {
        pair[0] = unescape_rust(&pair[0]);
        pair[1] = unescape_rust(&pair[1]);
    }
    for pair in def.verbatim_quotes.iter_mut() {
        pair[0] = unescape_rust(&pair[0]);
        pair[1] = unescape_rust(&pair[1]);
    }
    for pair in def.doc_quotes.iter_mut() {
        pair[0] = unescape_rust(&pair[0]);
        pair[1] = unescape_rust(&pair[1]);
    }
    for s in def.important_syntax.iter_mut() {
        *s = unescape_rust(s);
    }
}

/// Interpret Rust-string-literal escapes within a marker. Handles the common cases —
/// `\"`, `\'`, `\\`, `\n`, `\t`, `\r`, `\0`. Unknown escapes are preserved verbatim.
fn unescape_rust(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Look up a language by tokei key (e.g. `"Rust"`, `"Python"`, `"Html"`).
pub fn by_key(key: &str) -> Option<&'static LanguageDef> {
    LANGUAGES.get(key)
}

/// Look up by file extension (no leading dot). Returns the first matching language key.
/// In tokei some extensions map to multiple languages (e.g. `.h` → C or C++) — disambiguation
/// is the caller's problem and will be a per-detector concern later.
pub fn by_extension(ext: &str) -> Option<(&'static str, &'static LanguageDef)> {
    LANGUAGES
        .iter()
        .find(|(_, def)| def.extensions.iter().any(|e| e == ext))
        .map(|(k, v)| (k.as_str(), v))
}

/// Iterate every language definition. Stable insertion order is not guaranteed.
pub fn all() -> impl Iterator<Item = (&'static str, &'static LanguageDef)> {
    LANGUAGES.iter().map(|(k, v)| (k.as_str(), v))
}

/// Total number of languages parsed from the vendored JSON.
pub fn count() -> usize {
    LANGUAGES.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vendored_json() {
        assert!(count() > 300, "expected hundreds of languages, got {}", count());
    }

    #[test]
    fn rust_is_complete() {
        let rust = by_key("Rust").expect("Rust should be present");
        assert_eq!(rust.line_comment, vec!["//"]);
        assert_eq!(rust.multi_line_comments, vec![["/*".to_string(), "*/".to_string()]]);
        assert!(rust.nested, "Rust block comments nest");
        assert!(!rust.verbatim_quotes.is_empty(), "Rust has r\"...\" verbatim strings");
    }

    #[test]
    fn python_has_doc_quotes() {
        let py = by_key("Python").expect("Python should be present");
        assert!(py.doc_quotes.iter().any(|p| p[0] == "\"\"\""));
    }

    #[test]
    fn markdown_is_literate() {
        let md = by_key("Markdown").expect("Markdown should be present");
        assert!(md.literate);
        assert!(md.important_syntax.contains(&"```".to_string()));
    }

    #[test]
    fn html_is_html_kind() {
        let html = by_key("Html").expect("Html should be present");
        assert_eq!(html.kind.as_deref(), Some("html"));
    }

    #[test]
    fn lookup_by_extension() {
        let (key, _) = by_extension("rs").expect(".rs should resolve");
        assert_eq!(key, "Rust");
        let (key, _) = by_extension("py").expect(".py should resolve");
        assert_eq!(key, "Python");
    }
}
