//! Integration test against a known-dirty real-world corpus
//! (`TheAlgorithms/Python`). Gated `#[ignore]` because it needs ~30 MB of
//! source on disk and isn't free to run; opt in with:
//!
//! ```
//! scripts/fetch-corpora.sh the-algorithms-python
//! cargo test --release -p tokei-dedup-cli -- --ignored real_corpus
//! ```
//!
//! Rationale: hand-curated fixtures prove the detector works on textbook
//! Type-2 plants. They don't tell us whether it surfaces real organic
//! duplication on a 1k-file repo. This test points the engine at
//! TheAlgorithms/Python — a repo well-known to contain many genuine
//! intra-repo clones (same algorithm in multiple files, helper utilities
//! copy-pasted across solution sets) — and asserts that:
//!
//!   * the overall finding count clears a healthy floor,
//!   * a meaningful share of those findings are perfect-Jaccard pairs,
//!   * several specific clones that humans have flagged in the repo
//!     before do, in fact, appear in our results,
//!   * the cross-module classifier tag fires at least somewhere.
//!
//! All assertions are *floors* with comfortable headroom over the
//! numbers observed on commit `456d644c23` so that upstream drift
//! (TheAlgorithms is actively maintained) doesn't make this test flaky.
//! If the upstream repo ever evolves enough to break a specific clone
//! check, swap the expected pair, don't lower the floor.

use std::path::{Path, PathBuf};

use tokei_dedup_classifier::Tag;
use tokei_dedup_core::BlindMode;
use tokei_dedup_engine::{scan, Granularity, ScanOptions};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpora")
        .join("the-algorithms-python")
}

fn ensure_corpus(root: &Path) {
    if !root.join(".git").is_dir() {
        panic!(
            "corpus not found at {}. Fetch with:\n    \
             scripts/fetch-corpora.sh the-algorithms-python",
            root.display()
        );
    }
}

/// Returns true if a finding's two endpoints touch the given filenames
/// (in either order), regardless of directory or function name.
fn pair_touches(f: &tokei_dedup_classifier::Finding, fname_a: &str, fname_b: &str) -> bool {
    let a = f
        .a
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let b = f
        .b
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    (a == fname_a && b == fname_b) || (a == fname_b && b == fname_a)
}

fn endpoint_fn_name(f: &tokei_dedup_classifier::Finding, side: u8) -> Option<&str> {
    let r = if side == 0 { &f.a } else { &f.b };
    r.granule.as_ref().and_then(|g| g.fn_name.as_deref())
}

#[test]
#[ignore = "fetches ~30MB and scans ~1.4k files; opt in with --ignored"]
fn real_corpus_the_algorithms_python_function_mode() {
    let root = corpus_root();
    ensure_corpus(&root);

    let opts = ScanOptions {
        blind: BlindMode::Aggressive,
        granularity: Granularity::Function,
        min_jaccard: 0.7,
        ..Default::default()
    };

    let result = scan(&root, &opts);

    eprintln!(
        "scanned {} files, indexed {} entries, {} findings in {:.2}s (backend={})",
        result.files_walked,
        result.entries_indexed,
        result.findings.len(),
        result.elapsed_secs,
        result.backend
    );

    // --- Volume floors ------------------------------------------------
    // Observed on commit 456d644c23: 1485 files walked, 3915 indexed,
    // 2010 findings at j>=0.7. Floors set well below that so a couple
    // hundred deletions upstream don't flip the test red.
    assert!(
        result.files_walked >= 1000,
        "expected to walk >=1000 files; got {}",
        result.files_walked
    );
    assert!(
        result.findings.len() >= 500,
        "expected >=500 findings at j>=0.7; got {}",
        result.findings.len()
    );

    let perfect = result
        .findings
        .iter()
        .filter(|f| f.exact_jaccard >= 0.999)
        .count();
    assert!(
        perfect >= 50,
        "expected >=50 perfect-Jaccard findings; got {perfect}"
    );

    // --- Known organic clones ----------------------------------------
    // Each entry is a well-known intra-repo duplicate that has been
    // visible in TheAlgorithms/Python for years. We require at least
    // three of the four to still be present, so a single upstream
    // cleanup doesn't break this test.
    struct KnownClone {
        label: &'static str,
        matcher: fn(&tokei_dedup_classifier::Finding) -> bool,
    }
    let known: &[KnownClone] = &[
        KnownClone {
            label: "binary_search_by_recursion in binary_search.py and exponential_search.py",
            matcher: |f| {
                pair_touches(f, "binary_search.py", "exponential_search.py")
                    && (endpoint_fn_name(f, 0) == Some("binary_search_by_recursion")
                        || endpoint_fn_name(f, 1) == Some("binary_search_by_recursion"))
            },
        },
        KnownClone {
            label: "extended_gcd shared between modular_division.py and diophantine_equation.py",
            matcher: |f| {
                pair_touches(f, "modular_division.py", "diophantine_equation.py")
                    && (endpoint_fn_name(f, 0) == Some("extended_gcd")
                        || endpoint_fn_name(f, 1) == Some("extended_gcd"))
            },
        },
        KnownClone {
            label: "cache_decorator_inner shared between lfu_cache.py and lru_cache.py",
            matcher: |f| {
                pair_touches(f, "lfu_cache.py", "lru_cache.py")
                    && (endpoint_fn_name(f, 0) == Some("cache_decorator_inner")
                        || endpoint_fn_name(f, 1) == Some("cache_decorator_inner"))
            },
        },
        KnownClone {
            label: "cycle_nodes defined twice in directed_and_undirected_weighted_graph.py",
            matcher: |f| {
                pair_touches(
                    f,
                    "directed_and_undirected_weighted_graph.py",
                    "directed_and_undirected_weighted_graph.py",
                ) && (endpoint_fn_name(f, 0) == Some("cycle_nodes")
                    || endpoint_fn_name(f, 1) == Some("cycle_nodes"))
            },
        },
    ];

    let mut found = 0;
    let mut missing = Vec::new();
    for kc in known {
        if result.findings.iter().any(|f| (kc.matcher)(f)) {
            found += 1;
        } else {
            missing.push(kc.label);
        }
    }
    assert!(
        found >= 3,
        "expected at least 3 of 4 known clones; only found {found}. Missing:\n  - {}",
        missing.join("\n  - ")
    );

    // --- Classifier sanity check -------------------------------------
    let cross_module = result
        .findings
        .iter()
        .filter(|f| f.tags.iter().any(|t| matches!(t, Tag::CrossModule)))
        .count();
    assert!(
        cross_module >= 5,
        "expected the cross-module tag to fire on >=5 findings; got {cross_module}"
    );
}
