//! Exact-Jaccard verification of LSH candidate pairs.
//!
//! LSH produces estimated Jaccard with ±~4% standard error (128-slot MinHash). For the
//! shortlist of candidate pairs the user actually sees, recomputing the exact value from
//! the full fingerprint sets is cheap and removes that noise. This crate is intentionally
//! tiny — it's just set algebra on `u64` hashes — but isolates the contract so the
//! classifier and report layer can rely on `jaccard` being precise.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verified {
    /// Index of the first item in the caller's ordering (e.g. LSH file id).
    pub a_id: u32,
    pub b_id: u32,
    /// Exact Jaccard, computed from the full fingerprint sets.
    pub exact_jaccard: f32,
    /// What LSH thought — kept for diagnostics.
    pub estimated_jaccard: f32,
    /// |A ∩ B|.
    pub shared: u32,
    /// |A ∪ B|.
    pub union: u32,
}

/// Compute exact Jaccard between two fingerprint hash *sets*. Duplicates within each
/// input are ignored.
pub fn exact_jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> (f32, u32) {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let intersection = small.iter().filter(|h| large.contains(h)).count();
    let union = a.len() + b.len() - intersection;
    let jaccard = if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    };
    (jaccard, intersection as u32)
}

/// Verify a single candidate pair given its two fingerprint sets.
pub fn verify(
    a_id: u32,
    b_id: u32,
    estimated: f32,
    set_a: &HashSet<u64>,
    set_b: &HashSet<u64>,
) -> Verified {
    let (exact, shared) = exact_jaccard(set_a, set_b);
    let union = (set_a.len() + set_b.len()) as u32 - shared;
    Verified {
        a_id,
        b_id,
        exact_jaccard: exact,
        estimated_jaccard: estimated,
        shared,
        union,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[u64]) -> HashSet<u64> {
        items.iter().copied().collect()
    }

    #[test]
    fn identical_sets_jaccard_one() {
        let (j, n) = exact_jaccard(&s(&[1, 2, 3]), &s(&[1, 2, 3]));
        assert!((j - 1.0).abs() < 1e-6);
        assert_eq!(n, 3);
    }

    #[test]
    fn disjoint_sets_jaccard_zero() {
        let (j, n) = exact_jaccard(&s(&[1, 2, 3]), &s(&[4, 5, 6]));
        assert_eq!(j, 0.0);
        assert_eq!(n, 0);
    }

    #[test]
    fn empty_sets_jaccard_zero() {
        let (j, n) = exact_jaccard(&s(&[]), &s(&[]));
        assert_eq!(j, 0.0);
        assert_eq!(n, 0);
    }

    #[test]
    fn partial_overlap_correct() {
        // 2 shared out of 5 unique: 1, 2 in both; 3 only in a; 4, 5 only in b.
        let (j, n) = exact_jaccard(&s(&[1, 2, 3]), &s(&[1, 2, 4, 5]));
        assert_eq!(n, 2);
        // Jaccard = 2 / (3 + 4 - 2) = 2/5 = 0.4
        assert!((j - 0.4).abs() < 1e-6);
    }

    #[test]
    fn subset_jaccard_matches_size_ratio() {
        // {1,2} ⊂ {1,2,3,4} → intersection=2, union=4, J=0.5
        let (j, n) = exact_jaccard(&s(&[1, 2]), &s(&[1, 2, 3, 4]));
        assert_eq!(n, 2);
        assert!((j - 0.5).abs() < 1e-6);
    }

    #[test]
    fn verify_packages_everything() {
        let v = verify(7, 11, 0.42, &s(&[1, 2, 3]), &s(&[2, 3, 4]));
        assert_eq!(v.a_id, 7);
        assert_eq!(v.b_id, 11);
        assert_eq!(v.shared, 2);
        assert_eq!(v.union, 4);
        assert!((v.exact_jaccard - 0.5).abs() < 1e-6);
        assert_eq!(v.estimated_jaccard, 0.42);
    }
}
