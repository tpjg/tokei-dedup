//! Token hashing, winnowing fingerprints, and MinHash sketches.
//!
//! Pipeline: `&[NormalizedToken]` → `Vec<u64>` (one hash per token) → `Vec<u64>` (one hash
//! per k-token n-gram) → `Vec<Fingerprint>` (winnowed subset). The MinHash sketch is then
//! computed over the *set* of winnowed fingerprint hashes for sub-linear pair retrieval
//! downstream (see `tokei_dedup_index::LshIndex`).
//!
//! Defaults follow the MOSS paper:
//! - `k = 5` — n-gram size.
//! - `w = 4` — winnowing window. Guarantee threshold `t = k + w - 1 = 8`.

use tokei_dedup_core::{NormalizedToken, TokenKind};
use xxhash_rust::xxh3::Xxh3;

/// A fingerprint surviving the winnowing pass.
///
/// `pos` is an index into the k-gram hash array. The n-gram at `pos` spans tokens
/// `[pos .. pos + k]` in the original stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    pub hash: u64,
    pub pos: u32,
}

pub const DEFAULT_K: usize = 5;
pub const DEFAULT_WINDOW: usize = 4;

/// Stable hash of a single token. Differentiates `TokenKind`, then mixes in `text` only
/// when present — so blinded literals (`text = None`) hash to a fixed per-kind value,
/// which is exactly what makes literal-renamed Type-2 clones match.
pub fn token_hash(t: &NormalizedToken) -> u64 {
    let mut h = Xxh3::new();
    h.update(&[token_kind_byte(t.kind)]);
    if let Some(text) = &t.text {
        h.update(text.as_bytes());
    }
    h.digest()
}

fn token_kind_byte(k: TokenKind) -> u8 {
    match k {
        TokenKind::Ident => 1,
        TokenKind::LitString => 2,
        TokenKind::LitNumber => 3,
        TokenKind::LitChar => 4,
        TokenKind::Operator => 5,
        TokenKind::Punctuation => 6,
        TokenKind::Whitespace => 7,
        TokenKind::LangPush(_) => 8,
        TokenKind::LangPop => 9,
    }
}

/// Compute hashes for each k-gram (sliding by 1 token). The n-gram at index `i` covers
/// tokens `[i .. i+k]`. Empty if `token_hashes.len() < k`.
pub fn k_grams(token_hashes: &[u64], k: usize) -> Vec<u64> {
    if k == 0 || token_hashes.len() < k {
        return Vec::new();
    }
    let n = token_hashes.len() - k + 1;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut h = Xxh3::new();
        for j in 0..k {
            h.update(&token_hashes[i + j].to_le_bytes());
        }
        out.push(h.digest());
    }
    out
}

/// MOSS winnowing: for each window of `w` k-gram hashes, emit one fingerprint — the
/// rightmost minimum hash in the window. Adjacent windows that pick the same fingerprint
/// dedupe to a single emission.
pub fn winnowing(kgram_hashes: &[u64], window: usize) -> Vec<Fingerprint> {
    let mut out: Vec<Fingerprint> = Vec::new();
    let n = kgram_hashes.len();
    if window == 0 || n < window {
        return out;
    }
    let mut last_emitted: Option<usize> = None;
    for start in 0..=n - window {
        let w = &kgram_hashes[start..start + window];
        // Rightmost minimum: scan left-to-right, replace on `<=`.
        let mut min_off = 0usize;
        let mut min_val = w[0];
        for (j, &h) in w.iter().enumerate() {
            if h <= min_val {
                min_val = h;
                min_off = j;
            }
        }
        let abs_pos = start + min_off;
        if last_emitted != Some(abs_pos) {
            out.push(Fingerprint {
                hash: min_val,
                pos: abs_pos as u32,
            });
            last_emitted = Some(abs_pos);
        }
    }
    out
}

/// Convenience: full pipeline from a token slice to fingerprints.
pub fn fingerprint_tokens(tokens: &[NormalizedToken], k: usize, window: usize) -> Vec<Fingerprint> {
    let token_hashes: Vec<u64> = tokens.iter().map(token_hash).collect();
    let kgrams = k_grams(&token_hashes, k);
    winnowing(&kgrams, window)
}

// --- MinHash --------------------------------------------------------------------------
//
// Classic permutation-based MinHash. For each of K linear hash functions
// `h_i(x) = a_i * x + b_i mod 2^64` (with `a_i` odd to guarantee a permutation), record
// `min` of `h_i(x)` over all fingerprint hashes in the file. Two sketches' fraction of
// matching slots is an unbiased estimator of the Jaccard similarity of the underlying
// fingerprint sets.

/// MinHash signature length. 128 slots ≈ ±4% standard error on Jaccard estimates.
pub const SIGNATURE_SIZE: usize = 128;

pub type Sketch = [u64; SIGNATURE_SIZE];

/// Holds the K hash-function parameters. Cheap to clone (constant-size arrays).
#[derive(Debug, Clone)]
pub struct MinHasher {
    a: [u64; SIGNATURE_SIZE],
    b: [u64; SIGNATURE_SIZE],
}

impl MinHasher {
    /// Deterministic constructor: same `seed` yields identical hash functions across
    /// runs, so sketches stored in one run can be compared with sketches from another.
    pub fn new(seed: u64) -> Self {
        let mut a = [0u64; SIGNATURE_SIZE];
        let mut b = [0u64; SIGNATURE_SIZE];
        for i in 0..SIGNATURE_SIZE {
            let ai = mix(seed, i as u64, 0xA1);
            // `a` must be odd so `a * x mod 2^64` is a permutation.
            a[i] = ai | 1;
            b[i] = mix(seed, i as u64, 0xB2);
        }
        Self { a, b }
    }

    /// Compute the MinHash sketch over a set of fingerprint hashes. Duplicate inputs are
    /// harmless — they only ever match an existing min — so the caller can pass either
    /// the raw `Fingerprint::hash` stream or a deduplicated set.
    pub fn sketch(&self, fingerprint_hashes: &[u64]) -> Sketch {
        let mut sig = [u64::MAX; SIGNATURE_SIZE];
        for &x in fingerprint_hashes {
            self.update_sketch(&mut sig, x);
        }
        sig
    }

    /// Convenience: pull `hash` out of each Fingerprint and sketch.
    pub fn sketch_fingerprints(&self, fps: &[Fingerprint]) -> Sketch {
        let mut sig = [u64::MAX; SIGNATURE_SIZE];
        for f in fps {
            self.update_sketch(&mut sig, f.hash);
        }
        sig
    }

    #[inline]
    fn update_sketch(&self, sig: &mut Sketch, x: u64) {
        for (i, slot) in sig.iter_mut().enumerate() {
            let h = self.a[i].wrapping_mul(x).wrapping_add(self.b[i]);
            if h < *slot {
                *slot = h;
            }
        }
    }
}

/// Default MinHasher seed used across the workspace. Pinned so persisted sketches stay
/// comparable.
pub const DEFAULT_MINHASH_SEED: u64 = 0x736b65746368u64; // "sketch"

/// Estimated Jaccard similarity = fraction of identical slots across the two signatures.
pub fn jaccard_from_sketches(a: &Sketch, b: &Sketch) -> f32 {
    let mut matches = 0u32;
    for i in 0..SIGNATURE_SIZE {
        if a[i] == b[i] {
            matches += 1;
        }
    }
    matches as f32 / SIGNATURE_SIZE as f32
}

fn mix(seed: u64, idx: u64, tag: u64) -> u64 {
    let mut h = Xxh3::with_seed(seed);
    h.update(&idx.to_le_bytes());
    h.update(&tag.to_le_bytes());
    h.digest()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokei_dedup_core::{NormalizedToken, TokenKind};

    fn ident(text: &str) -> NormalizedToken {
        NormalizedToken {
            kind: TokenKind::Ident,
            text: Some(text.into()),
            byte_start: 0,
            byte_end: 0,
            line: 0,
            col: 0,
            lang_stack: vec![],
        }
    }

    fn punct(c: char) -> NormalizedToken {
        NormalizedToken {
            kind: TokenKind::Punctuation,
            text: Some(c.to_string()),
            byte_start: 0,
            byte_end: 0,
            line: 0,
            col: 0,
            lang_stack: vec![],
        }
    }

    fn lit_str() -> NormalizedToken {
        NormalizedToken {
            kind: TokenKind::LitString,
            text: None,
            byte_start: 0,
            byte_end: 0,
            line: 0,
            col: 0,
            lang_stack: vec![],
        }
    }

    #[test]
    fn token_hash_distinguishes_kinds() {
        let a = ident("foo");
        let mut b = ident("foo");
        b.kind = TokenKind::Punctuation;
        assert_ne!(token_hash(&a), token_hash(&b));
    }

    #[test]
    fn blinded_literals_hash_equally() {
        // The blinding contract: two literals with text=None must hash equal so that
        // `"foo"` and `"bar"` are treated as the same token.
        let a = lit_str();
        let b = lit_str();
        assert_eq!(token_hash(&a), token_hash(&b));
    }

    #[test]
    fn k_grams_empty_when_too_short() {
        assert!(k_grams(&[1u64, 2, 3], 5).is_empty());
    }

    #[test]
    fn k_grams_length() {
        let hashes = vec![1u64, 2, 3, 4, 5, 6, 7];
        assert_eq!(k_grams(&hashes, 3).len(), 5);
    }

    #[test]
    fn winnowing_picks_rightmost_on_ties() {
        let hashes = vec![5u64, 5, 5, 5];
        let fps = winnowing(&hashes, 3);
        assert_eq!(fps.len(), 2);
        assert_eq!(fps[0].pos, 2);
        assert_eq!(fps[1].pos, 3);
    }

    #[test]
    fn identical_streams_produce_identical_fingerprints() {
        let stream: Vec<_> = "fn foo ( x ) { return x + 1 }"
            .split_whitespace()
            .map(ident)
            .collect();
        let a = fingerprint_tokens(&stream, 3, 3);
        let b = fingerprint_tokens(&stream, 3, 3);
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn copy_paste_shares_most_fingerprints() {
        let shared: Vec<_> = "fn foo a b c d e f g h i j k l m"
            .split_whitespace()
            .map(ident)
            .collect();
        let mut a = shared.clone();
        a.extend([ident("aa"), ident("bb")]);
        let mut b = shared.clone();
        b.extend([ident("cc"), ident("dd")]);

        let fa = fingerprint_tokens(&a, DEFAULT_K, DEFAULT_WINDOW);
        let fb = fingerprint_tokens(&b, DEFAULT_K, DEFAULT_WINDOW);
        let hashes_a: std::collections::HashSet<u64> = fa.iter().map(|f| f.hash).collect();
        let shared_count = fb.iter().filter(|f| hashes_a.contains(&f.hash)).count();
        assert!(
            shared_count >= 2,
            "expected at least 2 shared fingerprints, got {shared_count}"
        );
    }

    #[test]
    fn punctuation_changes_propagate() {
        let a = vec![ident("a"), ident("b"), ident("c"), ident("d"), ident("e"), ident("f")];
        let b = vec![ident("a"), ident("b"), punct(';'), ident("d"), ident("e"), ident("f")];
        let fa = fingerprint_tokens(&a, 3, 2);
        let fb = fingerprint_tokens(&b, 3, 2);
        assert_ne!(fa, fb);
    }

    // --- MinHash tests -------------------------------------------------------------

    #[test]
    fn minhash_deterministic_with_seed() {
        let mh1 = MinHasher::new(42);
        let mh2 = MinHasher::new(42);
        let fps = vec![1u64, 2, 3, 4, 5];
        assert_eq!(mh1.sketch(&fps), mh2.sketch(&fps));
    }

    #[test]
    fn minhash_sketch_identical_for_same_set() {
        let mh = MinHasher::new(DEFAULT_MINHASH_SEED);
        let set = vec![10u64, 20, 30, 40, 50, 60, 70, 80];
        let s1 = mh.sketch(&set);
        let mut shuffled = set.clone();
        shuffled.reverse();
        let s2 = mh.sketch(&shuffled);
        // MinHash is order-independent: same input set → identical sketch.
        assert_eq!(s1, s2);
    }

    #[test]
    fn minhash_disjoint_sets_estimate_near_zero() {
        let mh = MinHasher::new(DEFAULT_MINHASH_SEED);
        let a: Vec<u64> = (0..200).collect();
        let b: Vec<u64> = (1000..1200).collect();
        let est = jaccard_from_sketches(&mh.sketch(&a), &mh.sketch(&b));
        assert!(est < 0.10, "disjoint sets should estimate near 0, got {est}");
    }

    #[test]
    fn minhash_full_overlap_estimate_one() {
        let mh = MinHasher::new(DEFAULT_MINHASH_SEED);
        let set: Vec<u64> = (0..50).collect();
        let est = jaccard_from_sketches(&mh.sketch(&set), &mh.sketch(&set));
        assert!((est - 1.0).abs() < 1e-6);
    }

    #[test]
    fn minhash_estimate_tracks_true_jaccard() {
        // Two sets sharing 100 of 200 elements each → true Jaccard = 100 / (200+200-100) = 1/3.
        let mh = MinHasher::new(DEFAULT_MINHASH_SEED);
        let common: Vec<u64> = (0..100).collect();
        let a_only: Vec<u64> = (100..200).collect();
        let b_only: Vec<u64> = (200..300).collect();
        let mut a = common.clone();
        a.extend(&a_only);
        let mut b = common.clone();
        b.extend(&b_only);
        let est = jaccard_from_sketches(&mh.sketch(&a), &mh.sketch(&b));
        let truth = 1.0 / 3.0;
        // ±5σ for 128-slot MinHash with p=1/3 is ~0.21. Plenty of margin.
        assert!(
            (est - truth).abs() < 0.10,
            "MinHash estimate {est} too far from truth {truth}"
        );
    }
}
