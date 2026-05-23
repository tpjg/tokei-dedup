//! Naive inverted-index over fingerprint hashes.
//!
//! Milestone 1 strategy: bucket fingerprints by hash, then for each bucket count file
//! pairs co-occurring. Files sharing many fingerprints are candidate clones.
//!
//! Cost: `O(sum_over_buckets size^2)`. A "popular" hash (boilerplate `import os` shows up
//! in every file) explodes one bucket into `N^2/2` pairs. The `max_bucket_size` cap
//! filters that — boilerplate-class hashes are skipped entirely. LSH (milestone 2) makes
//! this sub-linear.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokei_dedup_fingerprinter::Fingerprint;

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub id: u32,
    pub path: PathBuf,
    pub lang: String,
    /// Distinct fingerprint hash count for the file (denominator of Jaccard).
    pub unique_fps: u32,
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

    /// Add a file. Returns the assigned file id.
    pub fn add_file(&mut self, path: PathBuf, lang: &str, fps: &[Fingerprint]) -> u32 {
        let id = self.files.len() as u32;
        let unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
        let unique_count = unique.len() as u32;
        for h in &unique {
            self.inverted.entry(*h).or_default().push(id);
        }
        self.files.push(FileMeta {
            id,
            path,
            lang: lang.into(),
            unique_fps: unique_count,
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
}
