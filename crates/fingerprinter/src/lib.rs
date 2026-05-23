//! Token hashing, k-grams, and MOSS-style winnowing fingerprints.
//!
//! Pipeline: `&[NormalizedToken]` → `Vec<u64>` (one hash per token) → `Vec<u64>` (one hash
//! per k-token n-gram) → `Vec<Fingerprint>` (winnowed subset, one per window).
//!
//! Defaults follow the MOSS paper:
//! - `k = 5` — n-gram size. Determines the minimum length match we can detect.
//! - `w = 4` — winnowing window. Guarantee threshold `t = k + w - 1 = 8`: any match of
//!   8 or more consecutive identical k-gram hashes is guaranteed to share at least one
//!   fingerprint.

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
}
