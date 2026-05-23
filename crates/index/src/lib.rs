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
use std::path::{Path, PathBuf};
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
    /// Tombstone flag for incremental updates ([`LshIndex::remove_by_path`]).
    /// The naive [`Index`] never flips this and treats every entry as alive.
    pub alive: bool,
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
            alive: true,
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
    /// here (we don't add the same file twice). Entries for tombstoned files stay in
    /// the buckets until [`compact`](Self::compact) runs — readers skip dead IDs.
    buckets: HashMap<u64, Vec<u32>>,
    /// `path → currently-live IDs at that path`. Lets [`remove_by_path`](Self::remove_by_path)
    /// tombstone all granules of a file in O(granule_count). Cleared for a path on
    /// removal so a re-add registers fresh IDs without colliding with the old set.
    path_to_ids: HashMap<PathBuf, Vec<u32>>,
    /// Count of entries with `alive == false`. Drives the lazy-compaction trigger.
    tombstoned: usize,
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
            path_to_ids: HashMap::new(),
            tombstoned: 0,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_LSH_BANDS, DEFAULT_LSH_ROWS)
    }

    /// Number of currently-alive entries. Tombstoned entries do not count.
    pub fn file_count(&self) -> usize {
        self.files.len() - self.tombstoned
    }

    /// Total entries including tombstoned ones (the raw vector length). Useful for
    /// diagnostics around compaction; user-facing counts should prefer
    /// [`file_count`](Self::file_count).
    pub fn total_entries(&self) -> usize {
        self.files.len()
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Ratio of tombstoned to total entries. Used to decide when to
    /// [`compact`](Self::compact); 0.0 immediately after a fresh build or compaction.
    pub fn tombstone_ratio(&self) -> f32 {
        if self.files.is_empty() {
            0.0
        } else {
            self.tombstoned as f32 / self.files.len() as f32
        }
    }

    /// Whether `id` is still alive (i.e. not tombstoned). Out-of-range IDs are
    /// considered dead.
    pub fn is_alive(&self, id: u32) -> bool {
        self.files
            .get(id as usize)
            .map(|m| m.alive)
            .unwrap_or(false)
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
        self.path_to_ids.entry(path.clone()).or_default().push(id);
        self.files.push(FileMeta {
            id,
            path,
            lang,
            unique_fps,
            granule,
            alive: true,
        });
        id
    }

    /// Tombstone every entry whose path equals `path` and return the affected IDs.
    ///
    /// IDs and band-bucket entries stay in place — readers (`pair_report`,
    /// `candidate_pairs`, `partners_of`) skip dead IDs. This is O(granule_count)
    /// for the path. Call [`compact`](Self::compact) when [`tombstone_ratio`](Self::tombstone_ratio)
    /// grows too large.
    ///
    /// Returned IDs remain queryable via [`partners_of`](Self::partners_of) until
    /// the next compaction — this is intentional, so the engine layer can enumerate
    /// "what pairs would this (now-removed) entry have been in" after the call.
    pub fn remove_by_path(&mut self, path: &Path) -> Vec<u32> {
        let ids = self.path_to_ids.remove(path).unwrap_or_default();
        for &id in &ids {
            if let Some(meta) = self.files.get_mut(id as usize) {
                if meta.alive {
                    meta.alive = false;
                    self.tombstoned += 1;
                }
            }
        }
        ids
    }

    /// Live partners of `id` together with the sketch-Jaccard estimate.
    ///
    /// Walks the band buckets `id` participates in, collects every other ID sharing
    /// any band, filters to live entries, and computes the sketch-Jaccard estimate
    /// against `id`'s own sketch. The result is deduplicated across bands.
    ///
    /// Works on tombstoned `id`s too: their bucket entries are still in place, so
    /// you can ask "before I committed this removal, which live partners would this
    /// have paired with?". Dead-dead pairs are not returned because the partner
    /// must be alive.
    pub fn partners_of(&self, id: u32) -> Vec<(u32, f32)> {
        let Some(my_sketch) = self.sketches.get(id as usize) else {
            return Vec::new();
        };
        let mut seen: HashSet<u32> = HashSet::new();
        for band in 0..self.bands {
            let key = band_hash(my_sketch, band, self.rows);
            let Some(bucket) = self.buckets.get(&key) else {
                continue;
            };
            for &other in bucket {
                if other == id {
                    continue;
                }
                if !self.is_alive(other) {
                    continue;
                }
                seen.insert(other);
            }
        }
        seen.into_iter()
            .map(|other| {
                let est = jaccard_from_sketches(my_sketch, &self.sketches[other as usize]);
                (other, est)
            })
            .collect()
    }

    /// Drop tombstoned entries from the underlying vectors, reassign IDs to be
    /// contiguous, and rebuild buckets / path map. After this returns,
    /// [`tombstone_ratio`](Self::tombstone_ratio) is zero and every live entry has a
    /// fresh (typically smaller) ID.
    ///
    /// IDs are not stable across this call — callers that hold IDs must capture them
    /// before compaction or rebuild their own mapping afterwards. The engine layer
    /// drives compaction on its own cadence (after publishing an update) so the LSP
    /// caller never sees an ID change mid-request.
    pub fn compact(&mut self) {
        if self.tombstoned == 0 {
            return;
        }
        let mut new_files: Vec<FileMeta> = Vec::with_capacity(self.file_count());
        let mut new_sketches: Vec<Sketch> = Vec::with_capacity(self.file_count());
        let mut new_path_to_ids: HashMap<PathBuf, Vec<u32>> = HashMap::new();
        let mut new_buckets: HashMap<u64, Vec<u32>> = HashMap::new();

        for (old_idx, meta) in self.files.iter().enumerate() {
            if !meta.alive {
                continue;
            }
            let new_id = new_files.len() as u32;
            let sketch = self.sketches[old_idx];
            for band in 0..self.bands {
                let key = band_hash(&sketch, band, self.rows);
                new_buckets.entry(key).or_default().push(new_id);
            }
            new_sketches.push(sketch);
            new_path_to_ids
                .entry(meta.path.clone())
                .or_default()
                .push(new_id);
            let mut renumbered = meta.clone();
            renumbered.id = new_id;
            new_files.push(renumbered);
        }

        self.files = new_files;
        self.sketches = new_sketches;
        self.path_to_ids = new_path_to_ids;
        self.buckets = new_buckets;
        self.tombstoned = 0;
    }

    /// Walk buckets, collect candidate pairs, then refine via full-sketch Jaccard
    /// estimate. Returns pairs with estimated Jaccard `>= min_jaccard`. Dead IDs
    /// (tombstoned via [`remove_by_path`](Self::remove_by_path)) are skipped.
    pub fn pair_report(&self, min_jaccard: f32) -> Vec<PairReport> {
        let mut candidates: HashSet<(u32, u32)> = HashSet::new();
        for files in self.buckets.values() {
            // Filter dead IDs out before counting; a bucket with one live entry
            // and several tombstones still has no pairs.
            let mut sorted: Vec<u32> =
                files.iter().copied().filter(|id| self.is_alive(*id)).collect();
            if sorted.len() < 2 {
                continue;
            }
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
            let mut sorted: Vec<u32> =
                files.iter().copied().filter(|id| self.is_alive(*id)).collect();
            if sorted.len() < 2 {
                continue;
            }
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

    // --- LSH tombstoning / incremental-update primitives -------------------------

    fn ids_for_path<'a>(idx: &'a LshIndex, path: &str) -> Vec<u32> {
        idx.path_to_ids
            .get(&PathBuf::from(path))
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn lsh_remove_by_path_drops_pair() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        idx.add_file("b.rs".into(), "Rust", sketch_of(&set), 200);
        assert_eq!(idx.pair_report(0.5).len(), 1);
        let removed = idx.remove_by_path(&PathBuf::from("a.rs"));
        assert_eq!(removed.len(), 1);
        assert!(idx.pair_report(0.5).is_empty(), "pair should disappear after removal");
        assert_eq!(idx.file_count(), 1);
        assert_eq!(idx.total_entries(), 2);
    }

    #[test]
    fn lsh_partners_of_excludes_dead_ids() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        let mid_id = idx.add_file("b.rs".into(), "Rust", sketch_of(&set), 200);
        idx.add_file("c.rs".into(), "Rust", sketch_of(&set), 200);
        // All three are partners initially.
        let a_partners = idx.partners_of(0);
        assert_eq!(a_partners.len(), 2);
        idx.remove_by_path(&PathBuf::from("b.rs"));
        let after = idx.partners_of(0);
        assert_eq!(after.len(), 1, "after removing b, a should only see c");
        assert!(after.iter().all(|(id, _)| *id != mid_id));
    }

    #[test]
    fn lsh_partners_of_works_on_dead_id() {
        // Pre-removal capture pattern: we call partners_of on an ID that's about
        // to be (or just was) tombstoned, to learn which pairs need invalidation.
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        let a_id = idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        idx.add_file("b.rs".into(), "Rust", sketch_of(&set), 200);
        idx.remove_by_path(&PathBuf::from("a.rs"));
        assert!(!idx.is_alive(a_id));
        let partners = idx.partners_of(a_id);
        assert_eq!(
            partners.len(),
            1,
            "dead a should still report b as a (live) partner so caller can invalidate the pair"
        );
    }

    #[test]
    fn lsh_compact_preserves_alive_queries_and_renumbers() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
            idx.add_file(name.into(), "Rust", sketch_of(&set), 200);
        }
        // 5 files, all pairwise matching → 10 pairs.
        assert_eq!(idx.pair_report(0.5).len(), 10);
        idx.remove_by_path(&PathBuf::from("b.rs"));
        idx.remove_by_path(&PathBuf::from("d.rs"));
        // 3 live × 2 pairs each / 2 = 3 pairs.
        assert_eq!(idx.pair_report(0.5).len(), 3);
        assert!(idx.tombstone_ratio() > 0.3);
        let total_before = idx.total_entries();
        idx.compact();
        // Compaction reassigns IDs but keeps the live result set identical.
        assert_eq!(idx.pair_report(0.5).len(), 3);
        assert_eq!(idx.tombstone_ratio(), 0.0);
        assert!(idx.total_entries() < total_before);
        // path_to_ids reflects the new IDs.
        let new_a_ids = ids_for_path(&idx, "a.rs");
        assert_eq!(new_a_ids.len(), 1);
        assert!(idx.is_alive(new_a_ids[0]));
    }

    #[test]
    fn lsh_readd_same_path_after_removal_works() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        idx.add_file("b.rs".into(), "Rust", sketch_of(&set), 200);
        idx.remove_by_path(&PathBuf::from("a.rs"));
        // Re-add with same path (e.g. file was edited but still resembles the original).
        let new_id = idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        let live_a = ids_for_path(&idx, "a.rs");
        assert_eq!(live_a, vec![new_id], "path_to_ids should only track the live entry");
        assert_eq!(idx.pair_report(0.5).len(), 1);
        assert_eq!(idx.file_count(), 2);
        assert_eq!(idx.total_entries(), 3); // the dead entry still occupies a slot
    }

    #[test]
    fn lsh_remove_then_compact_then_add_round_trip() {
        // Build A, build B = (rebuild from scratch after a remove+compact+add cycle).
        // Both should produce identical pair_report sets.
        let set_one: Vec<u64> = (0..200).collect();
        let set_two: Vec<u64> = (300..500).collect();

        let mut a = LshIndex::with_defaults();
        a.add_file("x.rs".into(), "Rust", sketch_of(&set_one), 200);
        a.add_file("y.rs".into(), "Rust", sketch_of(&set_one), 200);
        a.add_file("z.rs".into(), "Rust", sketch_of(&set_two), 200);
        let a_pairs = sorted_pair_paths(&a.pair_report(0.5));

        let mut b = LshIndex::with_defaults();
        b.add_file("x.rs".into(), "Rust", sketch_of(&set_one), 200);
        b.add_file("y.rs".into(), "Rust", sketch_of(&set_two), 200); // wrong content
        b.add_file("z.rs".into(), "Rust", sketch_of(&set_two), 200);
        // Fix it: remove y, add y with the right content. Compact.
        b.remove_by_path(&PathBuf::from("y.rs"));
        b.add_file("y.rs".into(), "Rust", sketch_of(&set_one), 200);
        b.compact();
        let b_pairs = sorted_pair_paths(&b.pair_report(0.5));

        assert_eq!(a_pairs, b_pairs, "incremental path should match full-rebuild result");
    }

    fn sorted_pair_paths(pairs: &[PairReport]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = pairs
            .iter()
            .map(|p| {
                let a = p.file_a.to_string_lossy().to_string();
                let b = p.file_b.to_string_lossy().to_string();
                if a < b {
                    (a, b)
                } else {
                    (b, a)
                }
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn lsh_remove_unknown_path_is_noop() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        let removed = idx.remove_by_path(&PathBuf::from("nope.rs"));
        assert!(removed.is_empty());
        assert_eq!(idx.tombstone_ratio(), 0.0);
        assert_eq!(idx.pair_report(0.5).len(), 0); // single live file
    }

    #[test]
    fn lsh_compact_on_empty_index_is_noop() {
        let mut idx = LshIndex::with_defaults();
        idx.compact();
        assert_eq!(idx.total_entries(), 0);
        assert_eq!(idx.file_count(), 0);
    }

    #[test]
    fn lsh_compact_with_no_tombstones_is_noop() {
        let mut idx = LshIndex::with_defaults();
        let set: Vec<u64> = (0..200).collect();
        idx.add_file("a.rs".into(), "Rust", sketch_of(&set), 200);
        idx.add_file("b.rs".into(), "Rust", sketch_of(&set), 200);
        let before = idx.pair_report(0.5).len();
        idx.compact();
        assert_eq!(idx.pair_report(0.5).len(), before);
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
