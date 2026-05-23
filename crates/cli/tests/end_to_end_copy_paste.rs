//! End-to-end: walk the deliberate copy-paste fixture, build the index, assert that the
//! original/renamed-clone pair is the top match by Jaccard and that the unrelated file
//! ranks far below.

use std::path::PathBuf;
use tokei_dedup_core::BlindMode;
use tokei_dedup_fingerprinter::{fingerprint_tokens, DEFAULT_K, DEFAULT_WINDOW};
use tokei_dedup_index::Index;
use tokei_dedup_lang_config as lang_config;
use tokei_dedup_normalizer::Normalizer;
use walkdir::WalkDir;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("copy-paste")
}

fn scan(blind: BlindMode) -> Vec<tokei_dedup_index::PairReport> {
    let normalizer = Normalizer::new(blind);
    let mut idx = Index::new();
    for entry in WalkDir::new(fixtures_root())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((lang, _)) = lang_config::by_extension(ext) else {
            continue;
        };
        let content = std::fs::read_to_string(path).expect("read fixture");
        let out = normalizer.process(&content, lang);
        let fps = fingerprint_tokens(&out.tokens, DEFAULT_K, DEFAULT_WINDOW);
        idx.add_file(path.to_owned(), lang, &fps);
    }
    let mut pairs = idx.pair_report(1, 1000);
    pairs.sort_by(|a, b| {
        b.jaccard
            .partial_cmp(&a.jaccard)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

#[test]
fn aggressive_mode_detects_renamed_clone() {
    let pairs = scan(BlindMode::Aggressive);
    assert!(!pairs.is_empty(), "expected at least one pair");
    let top = &pairs[0];
    // Top pair should be the (original, clone) pair, in either order.
    let a = top.file_a.file_name().unwrap().to_string_lossy().to_string();
    let b = top.file_b.file_name().unwrap().to_string_lossy().to_string();
    let names: std::collections::HashSet<&str> = [a.as_str(), b.as_str()].into_iter().collect();
    assert!(
        names.contains("original.py") && names.contains("clone_with_renames.py"),
        "expected top pair to be original.py / clone_with_renames.py, got {a} <-> {b}"
    );
    assert!(
        top.jaccard > 0.4,
        "expected strong Jaccard for the clone pair, got {}",
        top.jaccard
    );
    // Any other pair (involving unrelated.py) should rank well below.
    if pairs.len() > 1 {
        assert!(
            pairs[1].jaccard < top.jaccard / 2.0,
            "unrelated pair too close to clone pair: top={:.3}, next={:.3}",
            top.jaccard,
            pairs[1].jaccard
        );
    }
}

#[test]
fn mild_mode_misses_renamed_clone() {
    // Documents the limitation: with identifiers kept verbatim, renamed clones drop out.
    // Demonstrates *why* Aggressive mode exists; protects against accidentally weakening
    // the blinding semantics.
    let pairs = scan(BlindMode::Mild);
    let clone_pairs: Vec<_> = pairs
        .iter()
        .filter(|p| {
            let a = p.file_a.file_name().unwrap().to_string_lossy();
            let b = p.file_b.file_name().unwrap().to_string_lossy();
            (a == "original.py" && b == "clone_with_renames.py")
                || (a == "clone_with_renames.py" && b == "original.py")
        })
        .collect();
    // Either no match found at all, or a very weak one — both confirm Mild can't see it.
    if let Some(p) = clone_pairs.first() {
        assert!(
            p.jaccard < 0.15,
            "Mild mode unexpectedly strong on renamed clone (j={:.3})",
            p.jaccard
        );
    }
}
