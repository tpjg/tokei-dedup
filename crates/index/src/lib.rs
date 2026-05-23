//! Index backends for candidate-pair retrieval.
//!
//! Two implementations are provided:
//!
//! - [`Index`] — naive inverted index over fingerprint hashes. Exact, simple, but pair
//!   enumeration is `O(sum bucket_size^2)`; needs the `max_bucket_size` cap to keep
//!   boilerplate hashes from exploding. Useful for small corpora and as an oracle.
//! - [`LshIndex`] — banded MinHash LSH. Each file contributes one bucket entry per band
//!   (default 32 bands × 4 rows over a 128-slot sketch). Candidate retrieval is
//!   sub-linear in `N` because most files never collide in any band.
//!
//! Both backends produce the same [`PairReport`] shape — `jaccard` is exact for `Index`
//! and an unbiased estimate (±~4 % stddev with 128 slots) for `LshIndex`. The estimated
//! `shared` count for LSH is derived from `jaccard * (a + b) / (1 + jaccard)`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokei_dedup_fingerprinter::{jaccard_from_sketches, Fingerprint, Sketch, SIGNATURE_SIZE};
use xxhash_rust::xxh3::Xxh3;

/// Sub-file region metadata. Carried alongside path/lang for function-level entries
/// (milestone 3+). `None` on a `FileMeta` means the entry is the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GranuleInfo {
    pub fn_name: Option<String>,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub id: u32,
    pub path: PathBuf,
    pub lang: String,
    /// Distinct fingerprint hash count for the entry (denominator of Jaccard).
    pub unique_fps: u32,
    /// Set on function-level entries; `None` for whole-file entries.
    pub granule: Option<GranuleInfo>,
}

/// Inverted index keyed by fingerprint hash.
#[derive(Default)]
pub struct Index {
    files: Vec<FileMeta>,
    /// `hash → Vec<file_id>`. Each file appears at most once per bucket (we dedupe at
    /// insertion via a HashSet over the inserted fingerprints).
    inverted: HashMap<u64, Vec<u32>>,
}

#[derive(Debug, Clone)]
pub struct PairReport {
    pub file_a: PathBuf,
    pub file_b: PathBuf,
    pub lang_a: String,
    pub lang_b: String,
    pub shared: u32,
    pub a_total: u32,
    pub b_total: u32,
    pub jaccard: f32,
    /// Set when the entry is a function-level granule.
    pub granule_a: Option<GranuleInfo>,
    pub granule_b: Option<GranuleInfo>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn bucket_count(&self) -> usize {
        self.inverted.len()
    }

    /// Add a whole-file entry. Returns the assigned id.
    pub fn add_file(&mut self, path: PathBuf, lang: &str, fps: &[Fingerprint]) -> u32 {
        self.add_internal(path, lang.into(), None, fps)
    }

    /// Add a function-level granule. The granule's fingerprints come from slicing the
    /// file's token stream by `granule.byte_range()` before fingerprinting.
    pub fn add_granule(
        &mut self,
        path: PathBuf,
        lang: &str,
        granule: GranuleInfo,
        fps: &[Fingerprint],
    ) -> u32 {
        self.add_internal(path, lang.into(), Some(granule), fps)
    }

    fn add_internal(
        &mut self,
        path: PathBuf,
        lang: String,
        granule: Option<GranuleInfo>,
        fps: &[Fingerprint],
    ) -> u32 {
        let id = self.files.len() as u32;
        let unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
        let unique_count = unique.len() as u32;
        for h in &unique {
            self.inverted.entry(*h).or_default().push(id);
        }
        self.files.push(FileMeta {
            id,
            path,
            lang,
            unique_fps: unique_count,
            granule,
        });
        id
    }

    /// Report all file pairs sharing at least `min_shared` distinct fingerprint hashes.
    /// Buckets larger than `max_bucket_size` are skipped (boilerplate suppression).
    /// Results unsorted — caller sorts by their preferred metric.
    pub fn pair_report(&self, min_shared: u32, max_bucket_size: usize) -> Vec<PairReport> {
        let mut pair_shared: HashMap<(u32, u32), u32> = HashMap::new();
        for files in self.inverted.values() {
            if files.len() < 2 || files.len() > max_bucket_size {
                continue;
            }
            for i in 0..files.len() {
                for j in (i + 1)..files.len() {
                    let (a, b) = if files[i] < files[j] {
                        (files[i], files[j])
                    } else {
                        (files[j], files[i])
                    };
                    *pair_shared.entry((a, b)).or_insert(0) += 1;
                }
            }
        }

        let mut out = Vec::with_capacity(pair_shared.len());
        for ((a, b), shared) in pair_shared {
            if shared < min_shared {
                continue;
            }
            let meta_a = &self.files[a as usize];
            let meta_b = &self.files[b as usize];
            let union = meta_a.unique_fps as f32 + meta_b.unique_fps as f32 - shared as f32;
            let jaccard = if union > 0.0 {
                shared as f32 / union
            } else {
                0.0
            };
            out.push(PairReport {
                file_a: meta_a.path.clone(),
                file_b: meta_b.path.clone(),
                lang_a: meta_a.lang.clone(),
                lang_b: meta_b.lang.clone(),
                shared,
                a_total: meta_a.unique_fps,
                b_total: meta_b.unique_fps,
                jaccard,
                granule_a: meta_a.granule.clone(),
                granule_b: meta_b.granule.clone(),
            });
        }
        out
    }

    /// Distribution diagnostic — useful for tuning `max_bucket_size`. Returns counts of
    /// buckets at each size, sorted descending.
    pub fn bucket_size_histogram(&self) -> Vec<(usize, usize)> {
        let mut sizes: HashMap<usize, usize> = HashMap::new();
        for files in self.inverted.values() {
            *sizes.entry(files.len()).or_insert(0) += 1;
        }
        let mut v: Vec<_> = sizes.into_iter().collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v
    }
}

// --- LSH backend ----------------------------------------------------------------------

/// Banded LSH over MinHash sketches.
///
/// Two sketches that agree on at least one band become a candidate pair. With `bands=b`
/// and `rows=r`, the probability that a pair with true Jaccard `s` is caught is
/// `1 - (1 - s^r)^b`. Defaults `b=32, r=4` give an S-curve centered around `j ≈ 0.42`:
/// pairs with `j ≥ 0.6` are essentially always retrieved; pairs with `j ≤ 0.2` are rarely
/// retrieved.
pub struct LshIndex {
    bands: usize,
    rows: usize,
    files: Vec<FileMeta>,
    sketches: Vec<Sketch>,
    /// `xxh3(band_idx || band_slice) → file_ids`. A file appears at most once per band
    /// here (we don't add the same file twice).
    buckets: HashMap<u64, Vec<u32>>,
}

pub const DEFAULT_LSH_BANDS: usize = 32;
pub const DEFAULT_LSH_ROWS: usize = 4;

impl LshIndex {
    /// `bands * rows` must equal [`tokei_dedup_fingerprinter::SIGNATURE_SIZE`].
    pub fn new(bands: usize, rows: usize) -> Self {
        assert_eq!(
            bands * rows,
            SIGNATURE_SIZE,
            "bands*rows must equal SIGNATURE_SIZE ({SIGNATURE_SIZE})"
        );
        Self {
            bands,
            rows,
            files: Vec::new(),
            sketches: Vec::new(),
            buckets: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_LSH_BANDS, DEFAULT_LSH_ROWS)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Register a whole-file entry with its precomputed MinHash sketch.
    pub fn add_file(
        &mut self,
        path: PathBuf,
        lang: &str,
        sketch: Sketch,
        unique_fps: u32,
    ) -> u32 {
        self.add_internal(path, lang.into(), None, sketch, unique_fps)
    }

    /// Register a function-level granule with its precomputed MinHash sketch.
    pub fn add_granule(
        &mut self,
        path: PathBuf,
        lang: &str,
        granule: GranuleInfo,
        sketch: Sketch,
        unique_fps: u32,
    ) -> u32 {
        self.add_internal(path, lang.into(), Some(granule), sketch, unique_fps)
    }

    fn add_internal(
        &mut self,
        path: PathBuf,
        lang: String,
        granule: Option<GranuleInfo>,
        sketch: Sketch,
        unique_fps: u32,
    ) -> u32 {
        let id = self.files.len() as u32;
        for band in 0..self.bands {
            let key = band_hash(&sketch, band, self.rows);
            self.buckets.entry(key).or_default().push(id);
        }
        self.sketches.push(sketch);
        self.files.push(FileMeta {
            id,
            path,
            lang,
            unique_fps,
            granule,
        });
        id
    }

    /// Walk buckets, collect candidate pairs, then refine via full-sketch Jaccard
    /// estimate. Returns pairs with estimated Jaccard `>= min_jaccard`.
    pub fn pair_report(&self, min_jaccard: f32) -> Vec<PairReport> {
        let mut candidates: HashSet<(u32, u32)> = HashSet::new();
        for files in self.buckets.values() {
            if files.len() < 2 {
                continue;
            }
            // Sort + dedup defensively in case the same file landed twice in a bucket
            // (cross-band collision — exceedingly rare with xxh3).
            let mut sorted = files.clone();
            sorted.sort_unstable();
            sorted.dedup();
            for i in 0..sorted.len() {
                for j in (i + 1)..sorted.len() {
                    candidates.insert((sorted[i], sorted[j]));
                }
            }
        }

        let mut out = Vec::with_capacity(candidates.len());
        for (a, b) in candidates {
            let est = jaccard_from_sketches(&self.sketches[a as usize], &self.sketches[b as usize]);
            if est < min_jaccard {
                continue;
            }
            let meta_a = &self.files[a as usize];
            let meta_b = &self.files[b as usize];
            let sum = meta_a.unique_fps as f32 + meta_b.unique_fps as f32;
            // Invert jaccard = shared / (a + b - shared) → shared = j(a+b) / (1 + j).
            let shared_est = if est > 0.0 {
                (est * sum / (1.0 + est)).round() as u32
            } else {
                0
            };
            out.push(PairReport {
                file_a: meta_a.path.clone(),
                file_b: meta_b.path.clone(),
                lang_a: meta_a.lang.clone(),
                lang_b: meta_b.lang.clone(),
                shared: shared_est,
                a_total: meta_a.unique_fps,
                b_total: meta_b.unique_fps,
                jaccard: est,
                granule_a: meta_a.granule.clone(),
                granule_b: meta_b.granule.clone(),
            });
        }
        out
    }

    pub fn candidate_pair_count(&self) -> usize {
        self.candidate_pairs_raw().len()
    }

    /// Distinct candidate pairs from the band buckets, each paired with the estimated
    /// Jaccard derived from the full sketch. The downstream verifier replaces the
    /// estimate with the exact value using the original fingerprint sets.
    pub fn candidate_pairs(&self) -> Vec<(u32, u32, f32)> {
        self.candidate_pairs_raw()
            .into_iter()
            .map(|(a, b)| {
                let est = jaccard_from_sketches(
                    &self.sketches[a as usize],
                    &self.sketches[b as usize],
                );
                (a, b, est)
            })
            .collect()
    }

    /// Lookup the metadata that was recorded at `add_file` / `add_granule` time.
    pub fn meta(&self, id: u32) -> &FileMeta {
        &self.files[id as usize]
    }

    fn candidate_pairs_raw(&self) -> Vec<(u32, u32)> {
        let mut candidates: HashSet<(u32, u32)> = HashSet::new();
        for files in self.buckets.values() {
            if files.len() < 2 {
                continue;
            }
            let mut sorted = files.clone();
            sorted.sort_unstable();
            sorted.dedup();
            for i in 0..sorted.len() {
                for j in (i + 1)..sorted.len() {
                    candidates.insert((sorted[i], sorted[j]));
                }
            }
        }
        candidates.into_iter().collect()
    }
}

fn band_hash(sketch: &Sketch, band: usize, rows: usize) -> u64 {
    let mut h = Xxh3::new();
    h.update(&(band as u64).to_le_bytes());
    let start = band * rows;
    for slot in &sketch[start..start + rows] {
        h.update(&slot.to_le_bytes());
    }
    h.digest()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokei_dedup_fingerprinter::Fingerprint;

    fn fp(h: u64) -> Fingerprint {
        Fingerprint { hash: h, pos: 0 }
    }

    #[test]
    fn empty_index_no_pairs() {
        let idx = Index::new();
        assert!(idx.pair_report(1, 1000).is_empty());
    }

    #[test]
    fn pair_with_full_overlap_reports_jaccard_one() {
        let mut idx = Index::new();
        let fps = vec![fp(1), fp(2), fp(3)];
        idx.add_file("a.rs".into(), "Rust", &fps);
        idx.add_file("b.rs".into(), "Rust", &fps);
        let pairs = idx.pair_report(1, 1000);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].shared, 3);
        assert!((pairs[0].jaccard - 1.0).abs() < 1e-6);
    }

    #[test]
    fn disjoint_files_no_pair() {
        let mut idx = Index::new();
        idx.add_file("a.rs".into(), "Rust", &[fp(1), fp(2)]);
        idx.add_file("b.rs".into(), "Rust", &[fp(10), fp(20)]);
        assert!(idx.pair_report(1, 1000).is_empty());
    }

    #[test]
    fn min_shared_filters() {
        let mut idx = Index::new();
        idx.add_file("a.rs".into(), "Rust", &[fp(1), fp(2)]);
        idx.add_file("b.rs".into(), "Rust", &[fp(2), fp(3)]);
        // 1 shared; threshold 2 excludes.
        assert!(idx.pair_report(2, 1000).is_empty());
        // threshold 1 includes.
        assert_eq!(idx.pair_report(1, 1000).len(), 1);
    }

    #[test]
    fn max_bucket_filters_boilerplate() {
        let mut idx = Index::new();
        // Three files all sharing the same single fingerprint — looks like boilerplate.
        idx.add_file("a.rs".into(), "Rust", &[fp(99)]);
        idx.add_file("b.rs".into(), "Rust", &[fp(99)]);
        idx.add_file("c.rs".into(), "Rust", &[fp(99)]);
        // With cap=2, the 3-way bucket is dropped entirely.
        assert!(idx.pair_report(1, 2).is_empty());
        // With cap=10 we get all 3 pairs.
        assert_eq!(idx.pair_report(1, 10).len(), 3);
    }

    // --- LSH tests ----------------------------------------------------------------

    use tokei_dedup_fingerprinter::{MinHasher, DEFAULT_MINHASH_SEED};

    fn sketch_of(set: &[u64]) -> Sketch {
        let mh = MinHasher::new(DEFAULT_MINHASH_SEED);
        mh.sketch(set)
    }

    #[test]
    fn lsh_panics_on_bad_band_row_product() {
        let result = std::panic::catch_unwind(|| LshIndex::new(7, 7));
        assert!(result.is_err());
    }

    #[test]
    fn lsh_identical_sketches_pair() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        let s = sketch_of(&set);
        idx.add_file("a.rs".into(), "Rust", s, 200);
        idx.add_file("b.rs".into(), "Rust", s, 200);
        let pairs = idx.pair_report(0.5);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].jaccard > 0.99);
    }

    #[test]
    fn lsh_disjoint_sketches_do_not_pair() {
        let mut idx = LshIndex::with_defaults();
        let a: Vec<u64> = (0..200).collect();
        let b: Vec<u64> = (10_000..10_200).collect();
        idx.add_file("a.rs".into(), "Rust", sketch_of(&a), 200);
        idx.add_file("b.rs".into(), "Rust", sketch_of(&b), 200);
        let pairs = idx.pair_report(0.3);
        // Either no candidate generated at all, or candidate filtered by jaccard threshold.
        assert!(pairs.is_empty(), "disjoint sets should not pair, got {pairs:?}");
    }

    #[test]
    fn lsh_high_overlap_pairs_correctly() {
        // True Jaccard ≈ 100 / (200 + 200 - 100) = 0.33 — below default LSH curve
        // midpoint, may or may not be retrieved. Build a clearer case: 90% overlap.
        let mut idx = LshIndex::with_defaults();
        let common: Vec<u64> = (0..180).collect();
        let mut a = common.clone();
        a.extend(180..200);
        let mut b = common.clone();
        b.extend(200..220);
        idx.add_file("a.rs".into(), "Rust", sketch_of(&a), a.len() as u32);
        idx.add_file("b.rs".into(), "Rust", sketch_of(&b), b.len() as u32);
        let pairs = idx.pair_report(0.5);
        assert_eq!(pairs.len(), 1);
        // True Jaccard = 180 / 220 ≈ 0.818. Estimate should be in the ballpark.
        assert!(
            pairs[0].jaccard > 0.7,
            "expected high-overlap pair, got {:.3}",
            pairs[0].jaccard
        );
    }

    #[test]
    fn lsh_scales_to_many_files() {
        // 1000 unrelated files plus 1 cloned pair — pair must surface, candidate count
        // must stay far below O(N^2/2) = 500K.
        let mut idx = LshIndex::with_defaults();
        for i in 0..1000 {
            let set: Vec<u64> = (i * 1000..i * 1000 + 200).collect();
            idx.add_file(format!("f{i}.rs").into(), "Rust", sketch_of(&set), 200);
        }
        let clone_set: Vec<u64> = (0..200).collect(); // identical to file 0
        idx.add_file("clone.rs".into(), "Rust", sketch_of(&clone_set), 200);
        let candidates = idx.candidate_pair_count();
        let pairs = idx.pair_report(0.5);
        assert!(
            candidates < 5_000,
            "expected sub-linear candidate set, got {candidates}"
        );
        assert!(
            pairs.iter().any(|p| {
                let a = p.file_a.to_string_lossy();
                let b = p.file_b.to_string_lossy();
                (a == "f0.rs" && b == "clone.rs") || (a == "clone.rs" && b == "f0.rs")
            }),
            "expected the clone-of-f0 pair to surface; got {pairs:?}"
        );
    }
}
