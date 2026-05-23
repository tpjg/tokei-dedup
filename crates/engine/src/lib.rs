//! The tokei-dedup scan pipeline as a single library entry point.
//!
//! Both `dupe` (the CLI) and `dupe-lsp` (the LSP server) call [`scan`]. The pipeline:
//!
//! 1. Walk `workspace` collecting file paths (skipping unknown extensions).
//! 2. In parallel: normalize each file, optionally tree-sitter-slice into per-function
//!    granules, fingerprint with winnowing, and compute a MinHash sketch.
//! 3. Build an index (LSH by default; naive on `use_naive`).
//! 4. Retrieve candidate pairs, verify exact Jaccard, classify with five-tag heuristics
//!    relative to `workspace`.
//! 5. Return a [`ScanResult`] with the findings plus diagnostic stats.

use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokei_dedup_classifier::{classify_with_root, Finding, GranuleRef, ItemRef};
use tokei_dedup_core::{BlindMode, NormalizedToken};
use tokei_dedup_fingerprinter::{
    fingerprint_tokens, Fingerprint, MinHasher, Sketch, DEFAULT_MINHASH_SEED,
};
use tokei_dedup_index::{GranuleInfo, Index, LshIndex};
use tokei_dedup_lang_config as lang_config;
use tokei_dedup_normalizer::Normalizer;
use tokei_dedup_slicer::Slicer;
use tokei_dedup_verifier::{verify, Verified};
use walkdir::WalkDir;

pub use tokei_dedup_classifier::{Finding as ClassifiedFinding, ItemRef as FindingEndpoint, Tag};
pub use tokei_dedup_core::BlindMode as BlindModeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    File,
    Function,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub blind: BlindMode,
    pub granularity: Granularity,
    pub k: usize,
    pub window: usize,
    pub use_naive: bool,
    /// LSH mode: minimum exact Jaccard to retain.
    pub min_jaccard: f32,
    /// Naive mode: minimum shared distinct fingerprints.
    pub min_shared: u32,
    /// Naive mode: drop fingerprint buckets above this size.
    pub max_bucket: usize,
    /// Restrict to a single tokei language key, e.g. `"Rust"`.
    pub only_lang: Option<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            blind: BlindMode::Mild,
            granularity: Granularity::File,
            k: tokei_dedup_fingerprinter::DEFAULT_K,
            window: tokei_dedup_fingerprinter::DEFAULT_WINDOW,
            use_naive: false,
            min_jaccard: 0.5,
            min_shared: 10,
            max_bucket: 50,
            only_lang: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub findings: Vec<Finding>,
    pub files_walked: usize,
    pub entries_indexed: usize,
    pub candidate_pairs: usize,
    pub elapsed_secs: f32,
    pub backend: &'static str,
}

/// Run the full pipeline on a workspace directory.
pub fn scan(workspace: &Path, opts: &ScanOptions) -> ScanResult {
    let normalizer = Normalizer::new(opts.blind);
    let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
    let slicer = Slicer::new();
    let start = Instant::now();

    let paths: Vec<PathBuf> = WalkDir::new(workspace)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();
    let files_walked = paths.len();

    let items: Vec<Item> = paths
        .par_iter()
        .flat_map(|p| {
            process_path(
                &normalizer,
                &slicer,
                &minhasher,
                opts.granularity,
                p,
                opts.k,
                opts.window,
                opts.only_lang.as_deref(),
            )
        })
        .collect();

    let (mut findings, candidate_pairs, backend) = if opts.use_naive {
        run_naive(&items, opts.min_shared, opts.max_bucket, workspace)
    } else {
        run_lsh(&items, opts.min_jaccard, workspace)
    };

    if opts.granularity == Granularity::Function {
        findings.retain(|f| !same_endpoint(&f.a, &f.b));
    }

    findings.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.shared.cmp(&x.shared))
    });

    ScanResult {
        findings,
        files_walked,
        entries_indexed: items.len(),
        candidate_pairs,
        elapsed_secs: start.elapsed().as_secs_f32(),
        backend,
    }
}

struct Item {
    path: PathBuf,
    lang: &'static str,
    granule: Option<GranuleInfo>,
    fps: Vec<Fingerprint>,
    sketch: Sketch,
    unique_fps: u32,
    unique_set: HashSet<u64>,
}

#[allow(clippy::too_many_arguments)]
fn process_path(
    normalizer: &Normalizer,
    slicer: &Slicer,
    minhasher: &MinHasher,
    granularity: Granularity,
    path: &Path,
    k: usize,
    window: usize,
    only_lang: Option<&str>,
) -> Vec<Item> {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let Some((lang_key, _def)) = lang_config::by_extension(ext) else {
        return Vec::new();
    };
    if let Some(want) = only_lang {
        if !lang_key.eq_ignore_ascii_case(want) {
            return Vec::new();
        }
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let out = normalizer.process(&content, lang_key);
    if out.tokens.is_empty() {
        return Vec::new();
    }

    match granularity {
        Granularity::File => {
            build_item(minhasher, path.to_owned(), lang_key, None, &out.tokens, k, window)
                .into_iter()
                .collect()
        }
        Granularity::Function => {
            if !Slicer::supports(lang_key) {
                return Vec::new();
            }
            slicer
                .slice(lang_key, path.to_owned(), content.as_bytes())
                .into_iter()
                .filter_map(|g| {
                    let toks = tokens_in_byte_range(&out.tokens, g.byte_start, g.byte_end);
                    let info = GranuleInfo {
                        fn_name: g.name,
                        line_start: g.line_start,
                        line_end: g.line_end,
                    };
                    build_item(minhasher, g.file, lang_key, Some(info), toks, k, window)
                })
                .collect()
        }
    }
}

fn build_item(
    minhasher: &MinHasher,
    path: PathBuf,
    lang: &'static str,
    granule: Option<GranuleInfo>,
    tokens: &[NormalizedToken],
    k: usize,
    window: usize,
) -> Option<Item> {
    let fps = fingerprint_tokens(tokens, k, window);
    if fps.is_empty() {
        return None;
    }
    let unique_set: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
    let unique_count = unique_set.len() as u32;
    let unique_vec: Vec<u64> = unique_set.iter().copied().collect();
    let sketch = minhasher.sketch(&unique_vec);
    Some(Item {
        path,
        lang,
        granule,
        fps,
        sketch,
        unique_fps: unique_count,
        unique_set,
    })
}

fn tokens_in_byte_range(tokens: &[NormalizedToken], start: u32, end: u32) -> &[NormalizedToken] {
    let lo = tokens.partition_point(|t| t.byte_start < start);
    let hi = tokens.partition_point(|t| t.byte_start < end);
    &tokens[lo..hi]
}

fn run_lsh(
    items: &[Item],
    min_jaccard: f32,
    scan_root: &Path,
) -> (Vec<Finding>, usize, &'static str) {
    let mut idx = LshIndex::with_defaults();
    for e in items {
        if let Some(g) = &e.granule {
            idx.add_granule(e.path.clone(), e.lang, g.clone(), e.sketch, e.unique_fps);
        } else {
            idx.add_file(e.path.clone(), e.lang, e.sketch, e.unique_fps);
        }
    }
    let candidates = idx.candidate_pairs();
    let cand_count = candidates.len();
    let findings: Vec<Finding> = candidates
        .into_iter()
        .filter(|(_, _, est)| *est >= min_jaccard)
        .filter_map(|(a_id, b_id, est)| {
            let v = verify(
                a_id,
                b_id,
                est,
                &items[a_id as usize].unique_set,
                &items[b_id as usize].unique_set,
            );
            if v.exact_jaccard < min_jaccard {
                return None;
            }
            let meta_a = idx.meta(a_id);
            let meta_b = idx.meta(b_id);
            Some(classify_with_root(
                &v,
                meta_to_item_ref(meta_a),
                meta_to_item_ref(meta_b),
                Some(scan_root),
            ))
        })
        .collect();
    (findings, cand_count, "lsh")
}

fn run_naive(
    items: &[Item],
    min_shared: u32,
    max_bucket: usize,
    scan_root: &Path,
) -> (Vec<Finding>, usize, &'static str) {
    let mut idx = Index::new();
    for e in items {
        if let Some(g) = &e.granule {
            idx.add_granule(e.path.clone(), e.lang, g.clone(), &e.fps);
        } else {
            idx.add_file(e.path.clone(), e.lang, &e.fps);
        }
    }
    let pairs = idx.pair_report(min_shared, max_bucket);
    let cand = pairs.len();
    let findings: Vec<Finding> = pairs
        .into_iter()
        .map(|p| {
            let union = (p.a_total + p.b_total).saturating_sub(p.shared);
            let v = Verified {
                a_id: 0,
                b_id: 0,
                exact_jaccard: p.jaccard,
                estimated_jaccard: p.jaccard,
                shared: p.shared,
                union,
            };
            let a = ItemRef {
                path: p.file_a,
                lang: p.lang_a,
                granule: p.granule_a.as_ref().map(to_classifier_granule),
                unique_fps: p.a_total,
            };
            let b = ItemRef {
                path: p.file_b,
                lang: p.lang_b,
                granule: p.granule_b.as_ref().map(to_classifier_granule),
                unique_fps: p.b_total,
            };
            classify_with_root(&v, a, b, Some(scan_root))
        })
        .collect();
    (findings, cand, "naive")
}

fn meta_to_item_ref(meta: &tokei_dedup_index::FileMeta) -> ItemRef {
    ItemRef {
        path: meta.path.clone(),
        lang: meta.lang.clone(),
        granule: meta.granule.as_ref().map(to_classifier_granule),
        unique_fps: meta.unique_fps,
    }
}

fn to_classifier_granule(g: &GranuleInfo) -> GranuleRef {
    GranuleRef {
        fn_name: g.fn_name.clone(),
        line_start: g.line_start,
        line_end: g.line_end,
    }
}

fn same_endpoint(a: &ItemRef, b: &ItemRef) -> bool {
    a.path == b.path
        && match (&a.granule, &b.granule) {
            (Some(ga), Some(gb)) => ga == gb,
            (None, None) => true,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_options_default_is_sensible() {
        let o = ScanOptions::default();
        assert_eq!(o.granularity, Granularity::File);
        assert!(!o.use_naive);
        assert!(o.min_jaccard > 0.0 && o.min_jaccard < 1.0);
    }
}
