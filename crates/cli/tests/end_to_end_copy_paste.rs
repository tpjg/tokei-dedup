//! End-to-end: walk the deliberate copy-paste fixture, build the index, assert that the
//! original/renamed-clone pair is the top match by Jaccard and that the unrelated file
//! ranks far below.

use std::collections::HashSet;
use std::path::PathBuf;
use tokei_dedup_core::BlindMode;
use tokei_dedup_fingerprinter::{
    fingerprint_tokens, MinHasher, DEFAULT_K, DEFAULT_MINHASH_SEED, DEFAULT_WINDOW,
};
use tokei_dedup_index::{Index, LshIndex};
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

enum Backend {
    Naive,
    Lsh,
}

fn scan(blind: BlindMode, backend: Backend) -> Vec<tokei_dedup_index::PairReport> {
    let normalizer = Normalizer::new(blind);
    let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
    let mut naive = Index::new();
    let mut lsh = LshIndex::with_defaults();
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
        match backend {
            Backend::Naive => {
                naive.add_file(path.to_owned(), lang, &fps);
            }
            Backend::Lsh => {
                let unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
                let unique_count = unique.len() as u32;
                let unique_vec: Vec<u64> = unique.into_iter().collect();
                let sketch = minhasher.sketch(&unique_vec);
                lsh.add_file(path.to_owned(), lang, sketch, unique_count);
            }
        }
    }
    let mut pairs = match backend {
        Backend::Naive => naive.pair_report(1, 1000),
        Backend::Lsh => lsh.pair_report(0.1),
    };
    pairs.sort_by(|a, b| {
        b.jaccard
            .partial_cmp(&a.jaccard)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

fn assert_top_pair_is_clone(pairs: &[tokei_dedup_index::PairReport], min_jaccard: f32) {
    assert!(!pairs.is_empty(), "expected at least one pair");
    let top = &pairs[0];
    let a = top.file_a.file_name().unwrap().to_string_lossy().to_string();
    let b = top.file_b.file_name().unwrap().to_string_lossy().to_string();
    let names: HashSet<&str> = [a.as_str(), b.as_str()].into_iter().collect();
    assert!(
        names.contains("original.py") && names.contains("clone_with_renames.py"),
        "expected top pair to be original.py / clone_with_renames.py, got {a} <-> {b}"
    );
    assert!(
        top.jaccard > min_jaccard,
        "expected Jaccard > {min_jaccard}, got {}",
        top.jaccard
    );
}

#[test]
fn naive_aggressive_detects_renamed_clone() {
    let pairs = scan(BlindMode::Aggressive, Backend::Naive);
    assert_top_pair_is_clone(&pairs, 0.4);
    if pairs.len() > 1 {
        assert!(
            pairs[1].jaccard < pairs[0].jaccard / 2.0,
            "unrelated pair too close to clone pair"
        );
    }
}

#[test]
fn lsh_aggressive_detects_renamed_clone() {
    // LSH must surface the same top pair. With ~30 fingerprints per file the MinHash
    // estimate is noisy (the underlying Jaccard is around 0.6), so we lower the bar to
    // 0.3 — the test is "did LSH find this pair at all", not "exact Jaccard match".
    let pairs = scan(BlindMode::Aggressive, Backend::Lsh);
    assert_top_pair_is_clone(&pairs, 0.3);
}

#[test]
fn both_backends_agree_on_top_clone() {
    // Cross-validate: top pair under both backends must reference the same two files.
    let naive = scan(BlindMode::Aggressive, Backend::Naive);
    let lsh = scan(BlindMode::Aggressive, Backend::Lsh);
    let top_pair = |p: &tokei_dedup_index::PairReport| {
        let mut names = [
            p.file_a.file_name().unwrap().to_string_lossy().to_string(),
            p.file_b.file_name().unwrap().to_string_lossy().to_string(),
        ];
        names.sort();
        names
    };
    assert_eq!(top_pair(&naive[0]), top_pair(&lsh[0]));
}

#[test]
fn mild_mode_misses_renamed_clone() {
    // Documents the limitation: with identifiers kept verbatim, renamed clones drop out.
    // Demonstrates *why* Aggressive mode exists.
    let pairs = scan(BlindMode::Mild, Backend::Naive);
    let clone_pairs: Vec<_> = pairs
        .iter()
        .filter(|p| {
            let a = p.file_a.file_name().unwrap().to_string_lossy();
            let b = p.file_b.file_name().unwrap().to_string_lossy();
            (a == "original.py" && b == "clone_with_renames.py")
                || (a == "clone_with_renames.py" && b == "original.py")
        })
        .collect();
    if let Some(p) = clone_pairs.first() {
        assert!(
            p.jaccard < 0.15,
            "Mild mode unexpectedly strong on renamed clone (j={:.3})",
            p.jaccard
        );
    }
}
