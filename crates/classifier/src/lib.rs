//! Heuristic ranking of verified clone pairs.
//!
//! Takes a [`Verified`] pair and per-endpoint metadata, applies a small set of rules
//! (nwave-dedup-style — pure heuristics, no ML), and emits a [`Finding`] with tags plus a
//! final score that the report layer sorts by.
//!
//! Rules and weights (multiplicative on a base of `exact_jaccard`):
//!
//! | Tag             | When                                                  | Score factor |
//! |-----------------|-------------------------------------------------------|--------------|
//! | `TestOnly`      | both paths look like tests                            | ×0.4         |
//! | `GenericName`   | either function name in the curated generic set       | ×0.6         |
//! | `Tiny`          | both endpoints have <15 unique fingerprints           | ×0.5         |
//! | `CrossModule`   | top-level path components differ                      | ×1.4 (cap 1) |
//! | `Subset`        | smaller set ≥95% contained in larger                  | tag only     |
//!
//! All factors compose; the final score is clamped to `[0, 1]`. The order matters only
//! for stable ranking — tags themselves are independent flags.

use std::path::{Component, Path, PathBuf};
use tokei_dedup_verifier::Verified;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Tag {
    TestOnly,
    GenericName,
    Tiny,
    CrossModule,
    Subset,
}

impl Tag {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tag::TestOnly => "test-only",
            Tag::GenericName => "generic-name",
            Tag::Tiny => "tiny",
            Tag::CrossModule => "cross-module",
            Tag::Subset => "subset",
        }
    }
}

/// One side of a clone pair, with whatever metadata the upstream stages preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRef {
    pub path: PathBuf,
    pub lang: String,
    /// 1-based function line range and name; `None` for whole-file entries.
    pub granule: Option<GranuleRef>,
    /// Unique fingerprint count for the entry (denominator side of Jaccard).
    pub unique_fps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GranuleRef {
    pub fn_name: Option<String>,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub a: ItemRef,
    pub b: ItemRef,
    pub exact_jaccard: f32,
    pub estimated_jaccard: f32,
    pub shared: u32,
    pub tags: Vec<Tag>,
    /// Final ranking score in `[0, 1]`.
    pub score: f32,
}

pub fn classify(verified: &Verified, a: ItemRef, b: ItemRef) -> Finding {
    classify_with_root(verified, a, b, None)
}

/// Like [`classify`] but strips `scan_root` from each item's path before evaluating
/// path-shape heuristics (`TestOnly`, `CrossModule`). Without this, scanning
/// `~/projects/foo/tests/corpora/something/` would label every finding `TestOnly`
/// because of the host filesystem path, not the project's own layout.
pub fn classify_with_root(
    verified: &Verified,
    a: ItemRef,
    b: ItemRef,
    scan_root: Option<&Path>,
) -> Finding {
    let mut tags: Vec<Tag> = Vec::new();
    let mut score = verified.exact_jaccard;

    let a_rel = strip_root(&a.path, scan_root);
    let b_rel = strip_root(&b.path, scan_root);

    if is_test_path(a_rel) && is_test_path(b_rel) {
        tags.push(Tag::TestOnly);
        score *= 0.4;
    }

    if has_generic_fn_name(&a) || has_generic_fn_name(&b) {
        tags.push(Tag::GenericName);
        score *= 0.6;
    }

    if a.unique_fps < 15 && b.unique_fps < 15 {
        tags.push(Tag::Tiny);
        score *= 0.5;
    }

    if top_module(a_rel) != top_module(b_rel) {
        tags.push(Tag::CrossModule);
        score = (score * 1.4).min(1.0);
    }

    let small = a.unique_fps.min(b.unique_fps);
    let large = a.unique_fps.max(b.unique_fps);
    if small > 0 && large > small {
        let containment = verified.shared as f32 / small as f32;
        if containment > 0.95 {
            tags.push(Tag::Subset);
        }
    }

    Finding {
        a,
        b,
        exact_jaccard: verified.exact_jaccard,
        estimated_jaccard: verified.estimated_jaccard,
        shared: verified.shared,
        tags,
        score: score.clamp(0.0, 1.0),
    }
}

fn strip_root<'a>(path: &'a Path, scan_root: Option<&Path>) -> &'a Path {
    match scan_root {
        Some(root) => path.strip_prefix(root).unwrap_or(path),
        None => path,
    }
}

/// Curated list of function names that carry low signal — same name across files almost
/// always means "same convention," not "same code." Lowercase comparison so the rule
/// catches `Init`, `INIT`, etc.
const GENERIC_NAMES: &[&str] = &[
    "init",
    "__init__",
    "new",
    "setup",
    "set_up",
    "teardown",
    "tear_down",
    "main",
    "run",
    "handle",
    "start",
    "stop",
    "close",
    "open",
    "read",
    "write",
    "get",
    "set",
    "build",
    "create",
    "destroy",
    "reset",
    "clear",
    "default",
    "from",
    "into",
    "as_ref",
    "as_mut",
    "to_string",
    "fmt",
    "drop",
    "clone",
    "eq",
    "hash",
    "deserialize",
    "serialize",
    "tostring",
    "equals",
    "hashcode",
];

fn has_generic_fn_name(item: &ItemRef) -> bool {
    let Some(g) = &item.granule else {
        return false;
    };
    let Some(name) = &g.fn_name else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    GENERIC_NAMES.iter().any(|g| *g == lower)
}

fn is_test_path(path: &Path) -> bool {
    for c in path.components() {
        if let Component::Normal(s) = c {
            let s = s.to_string_lossy().to_ascii_lowercase();
            if s == "tests"
                || s == "test"
                || s == "__tests__"
                || s == "spec"
                || s == "specs"
            {
                return true;
            }
            if s.starts_with("test_")
                || s.ends_with("_test")
                || s.ends_with("_test.py")
                || s.ends_with("_test.go")
                || s.ends_with("_test.rs")
                || s.ends_with(".test.js")
                || s.ends_with(".test.ts")
                || s.ends_with(".spec.js")
                || s.ends_with(".spec.ts")
            {
                return true;
            }
        }
    }
    false
}

/// First non-trivial path component, used as a proxy for "top-level module / package".
/// Empty-leading-slash and `.` are skipped.
fn top_module(path: &Path) -> Option<String> {
    for c in path.components() {
        match c {
            Component::Normal(s) => return Some(s.to_string_lossy().into_owned()),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, fn_name: Option<&str>, ufps: u32) -> ItemRef {
        ItemRef {
            path: PathBuf::from(path),
            lang: "Python".into(),
            granule: fn_name.map(|n| GranuleRef {
                fn_name: Some(n.into()),
                line_start: 1,
                line_end: 10,
            }),
            unique_fps: ufps,
        }
    }

    fn verified(j: f32, shared: u32) -> Verified {
        Verified {
            a_id: 0,
            b_id: 1,
            exact_jaccard: j,
            estimated_jaccard: j,
            shared,
            union: 100,
        }
    }

    #[test]
    fn cross_module_boosts() {
        let f = classify(
            &verified(0.5, 25),
            item("src/foo.py", Some("compute"), 40),
            item("lib/bar.py", Some("compute"), 40),
        );
        assert!(f.tags.contains(&Tag::CrossModule));
        // base 0.5 * 1.4 = 0.7 (no other modifiers since "compute" isn't generic and ufps>=15)
        assert!((f.score - 0.7).abs() < 1e-4);
    }

    #[test]
    fn test_only_demotes() {
        let f = classify(
            &verified(0.9, 50),
            item("tests/foo_test.py", Some("test_one"), 40),
            item("tests/bar_test.py", Some("test_two"), 40),
        );
        assert!(f.tags.contains(&Tag::TestOnly));
        // Same top-level "tests", so no CrossModule. 0.9 * 0.4 = 0.36.
        assert!((f.score - 0.36).abs() < 1e-4);
    }

    #[test]
    fn generic_name_demotes() {
        let f = classify(
            &verified(0.8, 40),
            item("src/foo.py", Some("__init__"), 40),
            item("src/bar.py", Some("__init__"), 40),
        );
        assert!(f.tags.contains(&Tag::GenericName));
        // Same top-level "src", so no boost. 0.8 * 0.6 = 0.48.
        assert!((f.score - 0.48).abs() < 1e-4);
    }

    #[test]
    fn tiny_demotes() {
        let f = classify(
            &verified(0.9, 5),
            item("a/x.py", Some("foo"), 8),
            item("a/y.py", Some("bar"), 10),
        );
        assert!(f.tags.contains(&Tag::Tiny));
        assert!((f.score - 0.45).abs() < 1e-4);
    }

    #[test]
    fn subset_tags_without_score_change() {
        // 18 of 20 (smaller) shared = 90% containment — not quite Subset.
        let mid = classify(
            &verified(0.5, 18),
            item("a/x.py", Some("foo"), 20),
            item("a/y.py", Some("bar"), 100),
        );
        assert!(!mid.tags.contains(&Tag::Subset));

        // 19 of 20 (smaller) shared = 95% — still strict-> needs > 0.95.
        let boundary = classify(
            &verified(0.5, 19),
            item("a/x.py", Some("foo"), 20),
            item("a/y.py", Some("bar"), 100),
        );
        assert!(!boundary.tags.contains(&Tag::Subset));

        // 20 of 20 shared and the other is larger → 100% containment.
        let subset = classify(
            &verified(0.18, 20),
            item("a/x.py", Some("foo"), 20),
            item("a/y.py", Some("bar"), 100),
        );
        assert!(subset.tags.contains(&Tag::Subset));
    }

    #[test]
    fn compound_tags_compose() {
        // test-only + tiny + cross-module on a 0.9 base:
        // 0.9 * 0.4 (test) * 0.5 (tiny) * 1.4 (cross-module, capped at 1) = 0.252
        let f = classify(
            &verified(0.9, 7),
            item("src/tests/a_test.py", Some("test_a"), 10),
            item("lib/tests/b_test.py", Some("test_b"), 9),
        );
        let expected = (0.9_f32 * 0.4 * 0.5 * 1.4).clamp(0.0, 1.0);
        assert!((f.score - expected).abs() < 1e-4, "got {}", f.score);
        assert!(f.tags.contains(&Tag::TestOnly));
        assert!(f.tags.contains(&Tag::Tiny));
        assert!(f.tags.contains(&Tag::CrossModule));
    }

    #[test]
    fn generic_name_detection_is_case_insensitive() {
        let it = item("src/foo.java", Some("HashCode"), 40);
        assert!(has_generic_fn_name(&it));
    }

    #[test]
    fn is_test_path_recognizes_common_patterns() {
        assert!(is_test_path(Path::new("src/tests/foo.py")));
        assert!(is_test_path(Path::new("project/__tests__/widget.js")));
        assert!(is_test_path(Path::new("pkg/foo_test.go")));
        assert!(is_test_path(Path::new("src/a/b/test_handler.py")));
        assert!(is_test_path(Path::new("src/x.spec.ts")));
        assert!(!is_test_path(Path::new("src/handler.py")));
    }

    #[test]
    fn classify_with_root_strips_host_prefix() {
        // Without scan_root, the host path's `tests/` triggers the TestOnly tag and
        // demotes the score even though, from the project's own layout, these are
        // ordinary source files.
        let pair = verified(0.8, 30);
        let a = item("/host/tests/corpora/proj/src/foo.py", Some("compute"), 40);
        let b = item("/host/tests/corpora/proj/src/bar.py", Some("compute"), 40);
        let without = classify(&pair, a.clone(), b.clone());
        assert!(without.tags.contains(&Tag::TestOnly));

        let with = classify_with_root(
            &pair,
            a,
            b,
            Some(Path::new("/host/tests/corpora/proj")),
        );
        assert!(
            !with.tags.contains(&Tag::TestOnly),
            "scan_root strip should remove the false TestOnly tag"
        );
    }
}
