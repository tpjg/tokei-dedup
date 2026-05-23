//! Function-bounded slicing of source files via tree-sitter.
//!
//! Given a file's bytes and its tokei language key, returns one [`Granule`] per
//! function-like construct found by a per-language tree-sitter query. A Granule is just
//! a labelled byte range — the caller (typically the fingerprinter) slices the file's
//! token stream by the granule's byte range to produce per-function fingerprints.
//!
//! Milestone-3 coverage: Rust, Python, JavaScript, Go. Adding a language is one new
//! `Query` and one dispatch arm.

use std::path::PathBuf;
use tree_sitter::{Language, Parser, Query, QueryCursor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granule {
    pub file: PathBuf,
    pub lang: String,
    /// Identifier captured by the tree-sitter query (function/method name). `None` when
    /// the construct is anonymous (e.g., an arrow function bound to a variable).
    pub name: Option<String>,
    pub byte_start: u32,
    pub byte_end: u32,
    /// 1-based line number of the first byte.
    pub line_start: u32,
    /// 1-based line number of the last byte (inclusive).
    pub line_end: u32,
}

impl Granule {
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        self.byte_start as usize..self.byte_end as usize
    }

    pub fn line_count(&self) -> u32 {
        self.line_end.saturating_sub(self.line_start) + 1
    }

    /// Display label for the granule — `path:line_start-line_end::name`.
    pub fn label(&self) -> String {
        let name = self.name.as_deref().unwrap_or("<anonymous>");
        format!(
            "{}:{}-{}::{}",
            self.file.display(),
            self.line_start,
            self.line_end,
            name
        )
    }
}

/// Holds compiled tree-sitter queries for every supported language. Construct once and
/// reuse across files (queries are `Send + Sync`); each call creates a fresh `Parser`
/// since `tree_sitter::Parser` is not `Sync`.
pub struct Slicer {
    rust: LangBundle,
    python: LangBundle,
    javascript: LangBundle,
    go: LangBundle,
}

struct LangBundle {
    language: Language,
    query: Query,
}

impl Default for Slicer {
    fn default() -> Self {
        Self::new()
    }
}

impl Slicer {
    pub fn new() -> Self {
        Self {
            rust: compile(
                tree_sitter_rust::language(),
                "(function_item name: (identifier) @name) @fn",
            ),
            python: compile(
                tree_sitter_python::language(),
                "(function_definition name: (identifier) @name) @fn",
            ),
            // For JS we accept both free function declarations and class methods. Arrow
            // functions assigned to vars are skipped — their "name" lives in the parent
            // variable declarator, which complicates the query. Future work.
            javascript: compile(
                tree_sitter_javascript::language(),
                r#"
                [
                  (function_declaration name: (identifier) @name) @fn
                  (method_definition name: (property_identifier) @name) @fn
                ]"#,
            ),
            go: compile(
                tree_sitter_go::language(),
                r#"
                [
                  (function_declaration name: (identifier) @name) @fn
                  (method_declaration name: (field_identifier) @name) @fn
                ]"#,
            ),
        }
    }

    /// Tokei language keys handled by this slicer.
    pub const SUPPORTED: &'static [&'static str] = &["Rust", "Python", "JavaScript", "Go"];

    pub fn supports(lang_key: &str) -> bool {
        Self::SUPPORTED.contains(&lang_key)
    }

    /// Extract function-bounded granules from a source file. Returns an empty Vec for
    /// unsupported languages or parse failures.
    pub fn slice(&self, lang_key: &str, file: PathBuf, source: &[u8]) -> Vec<Granule> {
        let bundle = match lang_key {
            "Rust" => &self.rust,
            "Python" => &self.python,
            "JavaScript" => &self.javascript,
            "Go" => &self.go,
            _ => return Vec::new(),
        };

        let mut parser = Parser::new();
        if parser.set_language(&bundle.language).is_err() {
            return Vec::new();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };

        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let capture_names = bundle.query.capture_names();
        for m in cursor.matches(&bundle.query, tree.root_node(), source) {
            let mut fn_node = None;
            let mut name = None;
            for c in m.captures {
                let cap_name = capture_names[c.index as usize];
                match cap_name {
                    "fn" => fn_node = Some(c.node),
                    "name" => {
                        name = std::str::from_utf8(&source[c.node.byte_range()])
                            .ok()
                            .map(str::to_string);
                    }
                    _ => {}
                }
            }
            let Some(fn_node) = fn_node else { continue };
            let range = fn_node.byte_range();
            let start_pos = fn_node.start_position();
            let end_pos = fn_node.end_position();
            out.push(Granule {
                file: file.clone(),
                lang: lang_key.into(),
                name,
                byte_start: range.start as u32,
                byte_end: range.end as u32,
                line_start: start_pos.row as u32 + 1,
                line_end: end_pos.row as u32 + 1,
            });
        }
        out
    }
}

fn compile(language: Language, query_src: &str) -> LangBundle {
    let query = Query::new(&language, query_src).expect("slicer query compiles");
    LangBundle { language, query }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn names(g: &[Granule]) -> Vec<String> {
        g.iter()
            .map(|g| g.name.clone().unwrap_or_else(|| "?".into()))
            .collect()
    }

    #[test]
    fn supported_predicate() {
        assert!(Slicer::supports("Rust"));
        assert!(Slicer::supports("Python"));
        assert!(!Slicer::supports("CHeader"));
        assert!(!Slicer::supports("Markdown"));
    }

    #[test]
    fn rust_extracts_functions() {
        let s = Slicer::new();
        let src = br#"
fn foo() -> i32 { 1 }

fn bar(x: i32) -> i32 {
    x * 2
}

impl Thing {
    fn method(&self) -> i32 { 3 }
}
"#;
        let g = s.slice("Rust", PathBuf::from("t.rs"), src);
        let mut got = names(&g);
        got.sort();
        assert_eq!(got, vec!["bar", "foo", "method"]);
    }

    #[test]
    fn rust_granule_byte_ranges_are_sane() {
        let s = Slicer::new();
        let src = b"fn first() { 1 }\nfn second() { 2 }\n";
        let g = s.slice("Rust", PathBuf::from("t.rs"), src);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name.as_deref(), Some("first"));
        assert_eq!(g[1].name.as_deref(), Some("second"));
        assert_eq!(&src[g[0].byte_range()], b"fn first() { 1 }");
        assert_eq!(&src[g[1].byte_range()], b"fn second() { 2 }");
        assert_eq!(g[0].line_start, 1);
        assert_eq!(g[1].line_start, 2);
    }

    #[test]
    fn python_extracts_module_and_method_functions() {
        let s = Slicer::new();
        let src = br#"
def free_fn(x):
    return x + 1

class K:
    def method(self, y):
        return y * 2
    def other(self):
        return 0
"#;
        let g = s.slice("Python", PathBuf::from("t.py"), src);
        let mut got = names(&g);
        got.sort();
        assert_eq!(got, vec!["free_fn", "method", "other"]);
    }

    #[test]
    fn javascript_extracts_function_decl_and_methods() {
        let s = Slicer::new();
        let src = br#"
function add(a, b) { return a + b; }

class X {
    foo() { return 1; }
    bar(z) { return z * 2; }
}
"#;
        let g = s.slice("JavaScript", PathBuf::from("t.js"), src);
        let mut got = names(&g);
        got.sort();
        assert_eq!(got, vec!["add", "bar", "foo"]);
    }

    #[test]
    fn go_extracts_functions_and_methods() {
        let s = Slicer::new();
        let src = br#"
package main

func Add(a, b int) int { return a + b }

type T struct{}

func (t T) Mul(x int) int { return x * 2 }
"#;
        let g = s.slice("Go", PathBuf::from("t.go"), src);
        let mut got = names(&g);
        got.sort();
        assert_eq!(got, vec!["Add", "Mul"]);
    }

    #[test]
    fn unsupported_lang_returns_empty() {
        let s = Slicer::new();
        let g = s.slice("Haskell", PathBuf::from("t.hs"), b"foo = 1");
        assert!(g.is_empty());
    }

    #[test]
    fn syntax_error_returns_partial_or_empty() {
        // tree-sitter is tolerant — even on broken input it builds a tree. We tolerate
        // either: 0 granules (if the parse drops the function) or N granules (if
        // tree-sitter recovers). The contract is "doesn't panic".
        let s = Slicer::new();
        let src = b"fn broken( {\n";
        let _ = s.slice("Rust", PathBuf::from("t.rs"), src);
    }
}
