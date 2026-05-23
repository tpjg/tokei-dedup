//! End-to-end: tree-sitter slice the copy-paste-functions fixture into per-function
//! granules and assert that the (read_cli_args, parse_args) clone surfaces as the top
//! pair — and that function-mode's top jaccard is materially higher than file-mode's,
//! the whole point of the granularity refinement.

use std::collections::HashSet;
use std::path::PathBuf;
use tokei_dedup_core::{BlindMode, NormalizedToken};
use tokei_dedup_fingerprinter::{
    fingerprint_tokens, MinHasher, DEFAULT_K, DEFAULT_MINHASH_SEED, DEFAULT_WINDOW,
};
use tokei_dedup_index::{GranuleInfo, LshIndex, PairReport};
use tokei_dedup_lang_config as lang_config;
use tokei_dedup_normalizer::Normalizer;
use tokei_dedup_slicer::Slicer;
use walkdir::WalkDir;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("copy-paste-functions")
}

fn tokens_in_byte_range(
    tokens: &[NormalizedToken],
    start: u32,
    end: u32,
) -> &[NormalizedToken] {
    let lo = tokens.partition_point(|t| t.byte_start < start);
    let hi = tokens.partition_point(|t| t.byte_start < end);
    &tokens[lo..hi]
}

fn scan_file_mode() -> Vec<PairReport> {
    let normalizer = Normalizer::new(BlindMode::Aggressive);
    let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
    let mut idx = LshIndex::with_defaults();
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
        let content = std::fs::read_to_string(path).unwrap();
        let out = normalizer.process(&content, lang);
        let fps = fingerprint_tokens(&out.tokens, DEFAULT_K, DEFAULT_WINDOW);
        if fps.is_empty() {
            continue;
        }
        let unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
        let unique_count = unique.len() as u32;
        let unique_vec: Vec<u64> = unique.into_iter().collect();
        idx.add_file(path.to_owned(), lang, minhasher.sketch(&unique_vec), unique_count);
    }
    let mut pairs = idx.pair_report(0.05);
    pairs.sort_by(|a, b| b.jaccard.partial_cmp(&a.jaccard).unwrap_or(std::cmp::Ordering::Equal));
    pairs
}

fn scan_function_mode() -> Vec<PairReport> {
    let normalizer = Normalizer::new(BlindMode::Aggressive);
    let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
    let slicer = Slicer::new();
    let mut idx = LshIndex::with_defaults();
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
        if !Slicer::supports(lang) {
            continue;
        }
        let content = std::fs::read_to_string(path).unwrap();
        let out = normalizer.process(&content, lang);
        for g in slicer.slice(lang, path.to_owned(), content.as_bytes()) {
            let toks = tokens_in_byte_range(&out.tokens, g.byte_start, g.byte_end);
            let fps = fingerprint_tokens(toks, DEFAULT_K, DEFAULT_WINDOW);
            if fps.is_empty() {
                continue;
            }
            let unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
            let unique_count = unique.len() as u32;
            let unique_vec: Vec<u64> = unique.into_iter().collect();
            let info = GranuleInfo {
                fn_name: g.name,
                line_start: g.line_start,
                line_end: g.line_end,
            };
            idx.add_granule(
                g.file,
                lang,
                info,
                minhasher.sketch(&unique_vec),
                unique_count,
            );
        }
    }
    let mut pairs = idx.pair_report(0.05);
    pairs.sort_by(|a, b| b.jaccard.partial_cmp(&a.jaccard).unwrap_or(std::cmp::Ordering::Equal));
    pairs
}

#[test]
fn function_mode_pinpoints_the_clone() {
    let pairs = scan_function_mode();
    assert!(!pairs.is_empty(), "expected at least one function-level pair");
    let top = &pairs[0];
    let a_info = top.granule_a.as_ref().expect("granule_a set in function mode");
    let b_info = top.granule_b.as_ref().expect("granule_b set in function mode");
    let mut names = vec![
        a_info.fn_name.clone().unwrap_or_default(),
        b_info.fn_name.clone().unwrap_or_default(),
    ];
    names.sort();
    assert_eq!(
        names,
        vec!["parse_args".to_string(), "read_cli_args".to_string()],
        "top function pair should be (read_cli_args, parse_args), got {names:?}"
    );
    assert!(
        top.jaccard > 0.7,
        "function-mode clone should match strongly; got j={:.3}",
        top.jaccard,
    );
}

#[test]
fn function_mode_beats_file_mode_on_function_clones() {
    let file_top = &scan_file_mode()[0];
    let fn_top = &scan_function_mode()[0];
    // The whole point of granularity: the function-level Jaccard should be substantially
    // higher than the file-level Jaccard because the surrounding unrelated functions
    // dilute the file-level signal.
    assert!(
        fn_top.jaccard > file_top.jaccard + 0.2,
        "expected function mode to beat file mode by >0.2 jaccard; file={:.3}, fn={:.3}",
        file_top.jaccard,
        fn_top.jaccard,
    );
}
