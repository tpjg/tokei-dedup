//! Function-bounded slicing of source files via tree-sitter.
//!
//! Given a file's bytes and its tokei language key, returns one [`Granule`] per
//! function-like construct found by a per-language tree-sitter query. A Granule is just
//! a labelled byte range — the caller (typically the fingerprinter) slices the file's
//! token stream by the granule's byte range to produce per-function fingerprints.
//!
//! Coverage as of milestone 5.5:
//!
//! | Tokei key | Slicer family | Notes |
//! |---|---|---|
//! | `Rust` | rust | `function_item` covers both freestanding and `impl` methods |
//! | `Python` | python | `function_definition` covers module-level and class methods |
//! | `JavaScript` | javascript | `function_declaration` + `method_definition`; arrows skipped |
//! | `Go` | go | `function_declaration` + `method_declaration` |
//! | `TypeScript` | typescript | same shape as JavaScript; uses `language_typescript()` |
//! | `Java` | java | `method_declaration` + `constructor_declaration` |
//! | `C`, `CHeader` | c | `function_definition` (declarator: function_declarator -> identifier) |
//! | `Cpp`, `CppHeader`, `CppModule`, `ObjectiveCpp` | cpp | free functions + class methods + `Foo::bar` |
//! | `Ruby` | ruby | `method` + `singleton_method` |
//! | `CSharp` | c_sharp | `method_declaration` + `constructor_declaration` + `destructor_declaration` |
//! | `Gleam` | gleam | top-level `function` declarations |
//!
//! Adding a language is one new query string and one `bundles.insert(...)` line.

use std::collections::HashMap;
use std::path::PathBuf;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granule {
    pub file: PathBuf,
    pub lang: String,
    pub name: Option<String>,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line_start: u32,
    pub line_end: u32,
}

impl Granule {
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        self.byte_start as usize..self.byte_end as usize
    }

    pub fn line_count(&self) -> u32 {
        self.line_end.saturating_sub(self.line_start) + 1
    }

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

struct LangBundle {
    language: Language,
    query: Query,
}

pub struct Slicer {
    /// Canonical key → bundle. Header keys (CHeader, CppHeader, …) are normalized to
    /// the implementation language's key before lookup; see [`canonical_key`].
    bundles: HashMap<&'static str, LangBundle>,
}

impl Default for Slicer {
    fn default() -> Self {
        Self::new()
    }
}

impl Slicer {
    /// Canonical tokei keys that the slicer can produce granules for. Keys aliased via
    /// [`canonical_key`] (e.g. `CHeader`) are also accepted by [`supports`] / [`slice`].
    pub const SUPPORTED: &'static [&'static str] = &[
        "Rust",
        "Python",
        "JavaScript",
        "Go",
        "TypeScript",
        "Java",
        "C",
        "Cpp",
        "Ruby",
        "CSharp",
        "Gleam",
    ];

    pub fn new() -> Self {
        let mut bundles = HashMap::with_capacity(Self::SUPPORTED.len());
        // tree-sitter 0.23+ exposes each grammar as `LANGUAGE: LanguageFn` which
        // converts into `Language` via `.into()`. TypeScript ships two parsers; pick
        // the .ts one (TSX would be a separate entry).
        bundles.insert("Rust", compile(tree_sitter_rust::LANGUAGE.into(), RUST_QUERY));
        bundles.insert("Python", compile(tree_sitter_python::LANGUAGE.into(), PYTHON_QUERY));
        bundles.insert("JavaScript", compile(tree_sitter_javascript::LANGUAGE.into(), JS_QUERY));
        bundles.insert("Go", compile(tree_sitter_go::LANGUAGE.into(), GO_QUERY));
        bundles.insert(
            "TypeScript",
            compile(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), TS_QUERY),
        );
        bundles.insert("Java", compile(tree_sitter_java::LANGUAGE.into(), JAVA_QUERY));
        bundles.insert("C", compile(tree_sitter_c::LANGUAGE.into(), C_QUERY));
        bundles.insert("Cpp", compile(tree_sitter_cpp::LANGUAGE.into(), CPP_QUERY));
        bundles.insert("Ruby", compile(tree_sitter_ruby::LANGUAGE.into(), RUBY_QUERY));
        bundles.insert("CSharp", compile(tree_sitter_c_sharp::LANGUAGE.into(), CSHARP_QUERY));
        bundles.insert("Gleam", compile(tree_sitter_gleam::LANGUAGE.into(), GLEAM_QUERY));
        Self { bundles }
    }

    /// True if the slicer can handle this tokei key (after header-key normalization).
    pub fn supports(lang_key: &str) -> bool {
        Self::SUPPORTED.contains(&canonical_key(lang_key))
    }

    /// Extract function-bounded granules from a source file. Empty Vec for unsupported
    /// languages or parse failures.
    pub fn slice(&self, lang_key: &str, file: PathBuf, source: &[u8]) -> Vec<Granule> {
        let canonical = canonical_key(lang_key);
        let Some(bundle) = self.bundles.get(canonical) else {
            return Vec::new();
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
        // tree-sitter 0.23+ returns a StreamingIterator from `cursor.matches`, not a
        // plain Iterator — so drive it with `while let`.
        let mut matches = cursor.matches(&bundle.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
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

/// Header / module variants share the implementation language's tree-sitter parser.
fn canonical_key(tokei_key: &str) -> &str {
    match tokei_key {
        "CHeader" => "C",
        "CppHeader" | "CppModule" | "ObjectiveCpp" => "Cpp",
        other => other,
    }
}

fn compile(language: Language, query_src: &str) -> LangBundle {
    let query = Query::new(&language, query_src).expect("slicer query compiles");
    LangBundle { language, query }
}


const RUST_QUERY: &str = "(function_item name: (identifier) @name) @fn";

const PYTHON_QUERY: &str = "(function_definition name: (identifier) @name) @fn";

// Arrow functions assigned to vars are skipped — their "name" lives on the parent
// variable_declarator, which complicates the query without much payoff for now.
const JS_QUERY: &str = r#"
[
  (function_declaration name: (identifier) @name) @fn
  (method_definition name: (property_identifier) @name) @fn
]"#;

const GO_QUERY: &str = r#"
[
  (function_declaration name: (identifier) @name) @fn
  (method_declaration name: (field_identifier) @name) @fn
]"#;

// TypeScript's grammar mirrors JS for our purposes. function_signature (abstract decls)
// is intentionally NOT captured — those are interfaces, not implementations.
const TS_QUERY: &str = r#"
[
  (function_declaration name: (identifier) @name) @fn
  (method_definition name: (property_identifier) @name) @fn
]"#;

const JAVA_QUERY: &str = r#"
[
  (method_declaration name: (identifier) @name) @fn
  (constructor_declaration name: (identifier) @name) @fn
]"#;

const C_QUERY: &str = r#"
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @fn
"#;

// C++ functions appear three ways:
//   1. Free function: `int foo() { ... }` — declarator is a plain identifier.
//   2. Method definition outside class: `int Foo::bar() { ... }` — qualified_identifier.
//   3. Inline method inside class: declarator is a field_identifier.
// All three are function_definition nodes; only the inner declarator differs.
const CPP_QUERY: &str = r#"
[
  (function_definition declarator: (function_declarator declarator: (identifier) @name)) @fn
  (function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @fn
  (function_definition declarator: (function_declarator declarator: (qualified_identifier) @name)) @fn
]
"#;

const RUBY_QUERY: &str = r#"
[
  (method name: (identifier) @name) @fn
  (singleton_method name: (identifier) @name) @fn
]"#;

const CSHARP_QUERY: &str = r#"
[
  (method_declaration name: (identifier) @name) @fn
  (constructor_declaration name: (identifier) @name) @fn
  (destructor_declaration name: (identifier) @name) @fn
]"#;

// Gleam's grammar uses `function` for top-level function declarations. Identifier
// captured via the `name:` field.
const GLEAM_QUERY: &str = "(function name: (identifier) @name) @fn";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn names(g: &[Granule]) -> Vec<String> {
        let mut v: Vec<String> = g
            .iter()
            .map(|g| g.name.clone().unwrap_or_else(|| "?".into()))
            .collect();
        v.sort();
        v
    }

    fn check(lang: &str, src: &[u8], want: &[&str]) {
        let s = Slicer::new();
        let got = names(&s.slice(lang, PathBuf::from("t"), src));
        let want_sorted = {
            let mut w: Vec<String> = want.iter().map(|s| (*s).into()).collect();
            w.sort();
            w
        };
        assert_eq!(got, want_sorted, "mismatch for {lang}");
    }

    #[test]
    fn supported_set() {
        assert!(Slicer::supports("Rust"));
        assert!(Slicer::supports("Python"));
        assert!(Slicer::supports("TypeScript"));
        assert!(Slicer::supports("Cpp"));
        assert!(Slicer::supports("CHeader")); // header alias
        assert!(Slicer::supports("CppHeader")); // alias
        assert!(Slicer::supports("Gleam"));
        assert!(!Slicer::supports("Markdown"));
        assert!(!Slicer::supports("CHeader2"));
    }

    #[test]
    fn rust_functions() {
        let src = br#"
fn foo() -> i32 { 1 }
fn bar(x: i32) -> i32 { x * 2 }
impl T { fn method(&self) -> i32 { 3 } }
"#;
        check("Rust", src, &["foo", "bar", "method"]);
    }

    #[test]
    fn rust_byte_ranges_are_sane() {
        let s = Slicer::new();
        let src = b"fn first() { 1 }\nfn second() { 2 }\n";
        let g = s.slice("Rust", PathBuf::from("t.rs"), src);
        assert_eq!(g.len(), 2);
        assert_eq!(&src[g[0].byte_range()], b"fn first() { 1 }");
        assert_eq!(g[0].line_start, 1);
        assert_eq!(g[1].line_start, 2);
    }

    #[test]
    fn python_functions() {
        let src = br#"
def free_fn(x):
    return x + 1

class K:
    def method(self, y):
        return y * 2
    def other(self):
        return 0
"#;
        check("Python", src, &["free_fn", "method", "other"]);
    }

    #[test]
    fn javascript_functions() {
        let src = br#"
function add(a, b) { return a + b; }
class X {
    foo() { return 1; }
    bar(z) { return z * 2; }
}
"#;
        check("JavaScript", src, &["add", "foo", "bar"]);
    }

    #[test]
    fn go_functions() {
        let src = br#"
package main
func Add(a, b int) int { return a + b }
type T struct{}
func (t T) Mul(x int) int { return x * 2 }
"#;
        check("Go", src, &["Add", "Mul"]);
    }

    #[test]
    fn typescript_functions() {
        let src = br#"
function add(a: number, b: number): number { return a + b; }
class X {
    foo(): number { return 1; }
    bar(z: number): number { return z * 2; }
}
"#;
        check("TypeScript", src, &["add", "foo", "bar"]);
    }

    #[test]
    fn java_functions() {
        let src = br#"
class X {
    public X() {}
    int add(int a, int b) { return a + b; }
    static int mul(int a, int b) { return a * b; }
}
"#;
        check("Java", src, &["X", "add", "mul"]);
    }

    #[test]
    fn c_functions() {
        let src = br#"
int add(int a, int b) { return a + b; }
static void noop(void) { }
"#;
        check("C", src, &["add", "noop"]);
    }

    #[test]
    fn c_header_via_alias() {
        let src = br#"
int add(int a, int b) { return a + b; }
"#;
        check("CHeader", src, &["add"]);
    }

    #[test]
    fn cpp_free_and_method() {
        let src = br#"
int free_fn(int x) { return x; }
class Foo {
public:
    int inline_method() { return 1; }
};
int Foo::out_of_line() { return 2; }
"#;
        let s = Slicer::new();
        let got = names(&s.slice("Cpp", PathBuf::from("t.cpp"), src));
        // We expect free_fn, inline_method, and the out_of_line method captured via
        // the qualified_identifier variant. The qualified-identifier capture yields
        // "Foo::out_of_line" as the name; assert it appears somewhere.
        assert!(got.contains(&"free_fn".to_string()));
        assert!(got.contains(&"inline_method".to_string()));
        assert!(
            got.iter().any(|n| n.contains("out_of_line")),
            "expected an out_of_line entry, got {got:?}"
        );
    }

    #[test]
    fn ruby_methods() {
        let src = br#"
def add(a, b)
  a + b
end
class Foo
  def bar(x)
    x * 2
  end
end
"#;
        check("Ruby", src, &["add", "bar"]);
    }

    #[test]
    fn csharp_methods() {
        let src = br#"
class X {
    public X() {}
    ~X() {}
    public int Add(int a, int b) { return a + b; }
}
"#;
        check("CSharp", src, &["X", "X", "Add"]);
    }

    #[test]
    fn gleam_functions() {
        let src = br#"
pub fn add(a: Int, b: Int) -> Int {
  a + b
}

fn private_helper(x: Int) -> Int {
  x * 2
}
"#;
        check("Gleam", src, &["add", "private_helper"]);
    }

    #[test]
    fn unsupported_lang_returns_empty() {
        let s = Slicer::new();
        let g = s.slice("Haskell", PathBuf::from("t.hs"), b"foo = 1");
        assert!(g.is_empty());
    }

    #[test]
    fn syntax_error_does_not_panic() {
        let s = Slicer::new();
        let _ = s.slice("Rust", PathBuf::from("t.rs"), b"fn broken( {\n");
        let _ = s.slice("Cpp", PathBuf::from("t.cpp"), b"int broken( {\n");
    }
}
