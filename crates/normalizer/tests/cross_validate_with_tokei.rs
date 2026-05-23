//! Cross-validation: run our FSM and tokei's library on the same fixtures and assert that
//! the line counts (`code`, `comments`, `blanks`) match.
//!
//! Tokei is the oracle. If counts diverge, the FSM has a bug. Drift in either direction
//! over time will surface here.

use std::fs;
use std::path::{Path, PathBuf};
use tokei::{Config, LanguageType};
use tokei_dedup_core::LineCounts;
use tokei_dedup_normalizer::Normalizer;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("multi-lang")
}

fn tokei_counts(lang: LanguageType, content: &str) -> LineCounts {
    let stats = lang.parse_from_slice(content.as_bytes(), &Config::default());
    LineCounts {
        code: stats.code,
        comment: stats.comments,
        blank: stats.blanks,
    }
}

fn ours(lang_key: &str, content: &str) -> LineCounts {
    Normalizer::default().process(content, lang_key).counts
}

fn check(path: &Path, lang_key: &str, tokei_lang: LanguageType) {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let ours = ours(lang_key, &content);
    let theirs = tokei_counts(tokei_lang, &content);
    assert_eq!(
        ours, theirs,
        "\nLine-count mismatch on {}\n  ours:   {:?}\n  tokei:  {:?}\n",
        path.display(),
        ours,
        theirs,
    );
}

#[test]
fn rust_hello() {
    check(&fixtures_dir().join("hello.rs"), "Rust", LanguageType::Rust);
}

#[test]
fn rust_nested_block_comments() {
    check(&fixtures_dir().join("nested.rs"), "Rust", LanguageType::Rust);
}

#[test]
fn python_strings_and_docstrings() {
    check(
        &fixtures_dir().join("strings.py"),
        "Python",
        LanguageType::Python,
    );
}

#[test]
fn javascript_mixed() {
    check(
        &fixtures_dir().join("mixed.js"),
        "JavaScript",
        LanguageType::JavaScript,
    );
}

#[test]
fn go_simple() {
    check(&fixtures_dir().join("simple.go"), "Go", LanguageType::Go);
}

#[test]
fn c_simple() {
    check(&fixtures_dir().join("simple.c"), "C", LanguageType::C);
}

#[test]
fn markdown_doc() {
    check(
        &fixtures_dir().join("doc.md"),
        "Markdown",
        LanguageType::Markdown,
    );
}
