//! Pre-duplication search: find existing functions similar to code you're about to write.
//!
//! Two search modes:
//! - **Snippet (QBE)**: provide a rough code sketch, find structurally similar functions
//!   via MinHash LSH.
//! - **Keywords**: provide identifier fragments, find functions whose identifiers overlap
//!   via BM25 scoring.
//!
//! Both operate against a [`SearchIndex`] which can be built from a workspace directory
//! and optionally persisted to disk for fast repeated queries.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use tokei_dedup_core::{NormalizedToken, TokenKind};
use tokei_dedup_fingerprinter::{
    fingerprint_tokens, jaccard_from_sketches, MinHasher, Sketch, DEFAULT_MINHASH_SEED,
};
use tokei_dedup_index::{GranuleInfo, LshIndex};
use tokei_dedup_normalizer::Normalizer;
use tokei_dedup_slicer::Slicer;
use tokei_dedup_verifier::verify;

use crate::{walk_filtered, Granularity, ScanOptions};

/// A single entry in the search index — one per function (or file in file mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchEntry {
    pub path: PathBuf,
    pub lang: String,
    pub granule: Option<GranuleInfo>,
    /// MinHash sketch stored as a Vec for serde compatibility (Sketch is [u64; 128]).
    pub sketch_vec: Vec<u64>,
    pub unique_fps: u32,
    pub unique_set: Vec<u64>,
    /// Split + lowercased identifier sub-tokens (e.g. "validateEmail" → ["validate", "email"]).
    pub ident_tokens: Vec<String>,
}

impl SearchEntry {
    pub fn sketch(&self) -> Sketch {
        let mut s = [0u64; tokei_dedup_fingerprinter::SIGNATURE_SIZE];
        let len = s.len().min(self.sketch_vec.len());
        s[..len].copy_from_slice(&self.sketch_vec[..len]);
        s
    }
}

/// Configuration baked into a persisted index. A query using different settings
/// than the index was built with would produce meaningless results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndexConfig {
    pub blind: String,
    pub granularity: Granularity,
    pub k: usize,
    pub window: usize,
    pub workspace: PathBuf,
}

/// Persistent search index. Serializable to/from JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndex {
    pub config: SearchIndexConfig,
    pub entries: Vec<SearchEntry>,
}

/// A match returned by snippet or keyword search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub rank: usize,
    pub score: f32,
    pub path: PathBuf,
    pub lang: String,
    pub fn_name: Option<String>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub unique_fps: u32,
    /// For snippet search: exact Jaccard between query and match.
    pub jaccard: Option<f32>,
    /// For keyword search: BM25 score.
    pub bm25: Option<f32>,
}

/// Result of a search operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub query_mode: String,
    pub matches: Vec<SearchMatch>,
    pub entries_indexed: usize,
    pub elapsed_secs: f32,
}

// --- Identifier extraction ---------------------------------------------------------------

/// Split a single identifier into sub-tokens by camelCase and snake_case boundaries.
/// "validateEmailAddress" → ["validate", "email", "address"]
/// "validate_email_addr" → ["validate", "email", "addr"]
/// "HTMLParser" → ["html", "parser"]
/// Single-char tokens are dropped.
pub fn split_identifier(ident: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = ident.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if ch == '_' || ch == '-' {
            i += 1;
            continue;
        }

        let mut word = String::new();

        if ch.is_uppercase() {
            // Collect consecutive uppercase chars
            while i < chars.len() && chars[i].is_uppercase() {
                word.push(chars[i]);
                i += 1;
            }
            // If followed by lowercase, the last uppercase starts a new word:
            // "HTMLParser" → "HTML" ends at 'L', 'P' starts "Parser"
            // But 'L' is part of "HTML" only if there are lowercase chars after 'P'.
            if i < chars.len() && chars[i].is_lowercase() && word.len() > 1 {
                // Move the last uppercase char to the next word
                let last = word.pop().unwrap();
                if word.len() > 1 {
                    tokens.push(word.to_lowercase());
                }
                word = String::new();
                word.push(last);
            }
            // Collect trailing lowercase/digit chars
            while i < chars.len() && (chars[i].is_lowercase() || chars[i].is_ascii_digit()) {
                word.push(chars[i]);
                i += 1;
            }
        } else {
            // Starts with lowercase or digit
            while i < chars.len() && !chars[i].is_uppercase() && chars[i] != '_' && chars[i] != '-' {
                word.push(chars[i]);
                i += 1;
            }
        }

        if word.len() > 1 {
            tokens.push(word.to_lowercase());
        }
    }
    tokens
}

/// Extract all identifier sub-tokens from a normalized token stream.
/// Only considers tokens with `kind == Ident` and `text.is_some()`.
fn extract_ident_tokens(tokens: &[NormalizedToken]) -> Vec<String> {
    let mut out = Vec::new();
    for t in tokens {
        if t.kind == TokenKind::Ident {
            if let Some(text) = &t.text {
                out.extend(split_identifier(text));
            }
        }
    }
    out
}

// --- BM25 keyword index ------------------------------------------------------------------

/// In-memory BM25 index over identifier sub-tokens.
struct KeywordIndex {
    /// term → vec of (entry_index, term_frequency)
    postings: HashMap<String, Vec<(usize, f32)>>,
    /// Number of unique terms per entry.
    doc_lengths: Vec<f32>,
    /// Average document length.
    avg_dl: f32,
    /// Total number of documents.
    n: usize,
}

impl KeywordIndex {
    fn build(entries: &[SearchEntry]) -> Self {
        let n = entries.len();
        let mut postings: HashMap<String, Vec<(usize, f32)>> = HashMap::new();
        let mut doc_lengths = Vec::with_capacity(n);

        for (idx, entry) in entries.iter().enumerate() {
            let mut tf_map: HashMap<&str, u32> = HashMap::new();
            for tok in &entry.ident_tokens {
                *tf_map.entry(tok.as_str()).or_default() += 1;
            }
            doc_lengths.push(entry.ident_tokens.len() as f32);
            for (term, count) in tf_map {
                postings
                    .entry(term.to_string())
                    .or_default()
                    .push((idx, count as f32));
            }
        }

        let avg_dl = if n > 0 {
            doc_lengths.iter().sum::<f32>() / n as f32
        } else {
            1.0
        };

        Self {
            postings,
            doc_lengths,
            avg_dl,
            n,
        }
    }

    /// BM25 search. Returns (entry_index, score) pairs, sorted descending.
    fn search(&self, query_terms: &[String], top: usize) -> Vec<(usize, f32)> {
        const K1: f32 = 1.2;
        const B: f32 = 0.75;

        let mut scores: HashMap<usize, f32> = HashMap::new();

        for term in query_terms {
            let Some(posting) = self.postings.get(term) else {
                continue;
            };
            let df = posting.len() as f32;
            let idf = ((self.n as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();
            if idf <= 0.0 {
                continue;
            }

            for &(doc_idx, tf) in posting {
                let dl = self.doc_lengths[doc_idx];
                let tf_norm = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / self.avg_dl));
                *scores.entry(doc_idx).or_default() += idf * tf_norm;
            }
        }

        let mut results: Vec<(usize, f32)> = scores.into_iter().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top);
        results
    }
}

// --- Search index building ---------------------------------------------------------------

impl SearchIndex {
    /// Build a search index from a workspace directory.
    pub fn build(workspace: &Path, opts: &ScanOptions) -> Self {
        let normalizer = Normalizer::new(opts.blind);
        let ident_normalizer = Normalizer::new(tokei_dedup_core::BlindMode::Strict);
        let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
        let slicer = Slicer::new();

        let paths: Vec<PathBuf> = walk_filtered(workspace, &opts.walk);

        let entries: Vec<SearchEntry> = paths
            .par_iter()
            .flat_map(|p| {
                process_path_for_search(
                    &normalizer,
                    &ident_normalizer,
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

        SearchIndex {
            config: SearchIndexConfig {
                blind: format!("{:?}", opts.blind),
                granularity: opts.granularity,
                k: opts.k,
                window: opts.window,
                workspace: workspace.to_owned(),
            },
            entries,
        }
    }

    /// Save the index to a JSON file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    /// Load a previously saved index.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        serde_json::from_reader(reader)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    /// Search by code snippet. The snippet is normalized and fingerprinted using the
    /// same settings as the index, then compared against all entries via LSH.
    pub fn search_snippet(
        &self,
        snippet: &str,
        lang_hint: Option<&str>,
        min_jaccard: f32,
        top: usize,
    ) -> SearchResult {
        let start = std::time::Instant::now();

        let lang_key = lang_hint
            .and_then(resolve_lang_key)
            .unwrap_or_else(|| guess_lang_from_snippet(snippet));

        let blind = match self.config.blind.as_str() {
            "Strict" => tokei_dedup_core::BlindMode::Strict,
            "Aggressive" => tokei_dedup_core::BlindMode::Aggressive,
            _ => tokei_dedup_core::BlindMode::Mild,
        };
        let normalizer = Normalizer::new(blind);
        let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);

        let out = normalizer.process(snippet, lang_key);
        if out.tokens.is_empty() {
            return SearchResult {
                query_mode: "snippet".into(),
                matches: Vec::new(),
                entries_indexed: self.entries.len(),
                elapsed_secs: start.elapsed().as_secs_f32(),
            };
        }

        let fps = fingerprint_tokens(&out.tokens, self.config.k, self.config.window);
        if fps.is_empty() {
            return SearchResult {
                query_mode: "snippet".into(),
                matches: Vec::new(),
                entries_indexed: self.entries.len(),
                elapsed_secs: start.elapsed().as_secs_f32(),
            };
        }

        let query_unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
        let query_unique_vec: Vec<u64> = query_unique.iter().copied().collect();
        let query_sketch = minhasher.sketch(&query_unique_vec);

        // Build a temporary LSH index and query it
        let mut lsh = LshIndex::with_defaults();
        for (idx, entry) in self.entries.iter().enumerate() {
            if let Some(ref g) = entry.granule {
                lsh.add_granule(
                    entry.path.clone(),
                    &entry.lang,
                    g.clone(),
                    entry.sketch(),
                    entry.unique_fps,
                );
            } else {
                lsh.add_file(entry.path.clone(), &entry.lang, entry.sketch(), entry.unique_fps);
            }
            let _ = idx;
        }

        // For small indexes or short queries, brute-force is fast and avoids
        // LSH false negatives on short functions where band collisions are rare.
        let use_brute_force = self.entries.len() < 5000 || query_unique.len() < 20;

        let candidate_ids: Vec<(u32, f32)> = if use_brute_force {
            (0..self.entries.len() as u32)
                .map(|id| {
                    let entry = &self.entries[id as usize];
                    let est = jaccard_from_sketches(&query_sketch, &entry.sketch());
                    (id, est)
                })
                .filter(|(_, est)| *est >= min_jaccard * 0.5)
                .collect()
        } else {
            lsh.query_sketch(&query_sketch)
        };

        let mut matches: Vec<SearchMatch> = candidate_ids
            .into_iter()
            .filter_map(|(id, est)| {
                let entry = &self.entries[id as usize];
                let entry_set: HashSet<u64> = entry.unique_set.iter().copied().collect();
                let v = verify(0, 0, est, &query_unique, &entry_set);
                if v.exact_jaccard < min_jaccard {
                    return None;
                }
                Some(SearchMatch {
                    rank: 0,
                    score: v.exact_jaccard,
                    path: entry.path.clone(),
                    lang: entry.lang.clone(),
                    fn_name: entry.granule.as_ref().and_then(|g| g.fn_name.clone()),
                    line_start: entry.granule.as_ref().map(|g| g.line_start),
                    line_end: entry.granule.as_ref().map(|g| g.line_end),
                    unique_fps: entry.unique_fps,
                    jaccard: Some(v.exact_jaccard),
                    bm25: None,
                })
            })
            .collect();

        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches.truncate(top);
        for (i, m) in matches.iter_mut().enumerate() {
            m.rank = i + 1;
        }

        SearchResult {
            query_mode: "snippet".into(),
            matches,
            entries_indexed: self.entries.len(),
            elapsed_secs: start.elapsed().as_secs_f32(),
        }
    }

    /// Search by keywords. Keywords are split the same way identifiers are, then
    /// matched against the identifier sub-token index via BM25.
    pub fn search_keywords(&self, keywords: &str, top: usize) -> SearchResult {
        let start = std::time::Instant::now();

        let query_terms: Vec<String> = keywords
            .split_whitespace()
            .flat_map(|w| split_identifier(w))
            .collect();

        if query_terms.is_empty() {
            return SearchResult {
                query_mode: "keywords".into(),
                matches: Vec::new(),
                entries_indexed: self.entries.len(),
                elapsed_secs: start.elapsed().as_secs_f32(),
            };
        }

        let kw_index = KeywordIndex::build(&self.entries);
        let results = kw_index.search(&query_terms, top);

        let matches: Vec<SearchMatch> = results
            .into_iter()
            .enumerate()
            .map(|(rank, (idx, bm25_score))| {
                let entry = &self.entries[idx];
                SearchMatch {
                    rank: rank + 1,
                    score: bm25_score,
                    path: entry.path.clone(),
                    lang: entry.lang.clone(),
                    fn_name: entry.granule.as_ref().and_then(|g| g.fn_name.clone()),
                    line_start: entry.granule.as_ref().map(|g| g.line_start),
                    line_end: entry.granule.as_ref().map(|g| g.line_end),
                    unique_fps: entry.unique_fps,
                    jaccard: None,
                    bm25: Some(bm25_score),
                }
            })
            .collect();

        SearchResult {
            query_mode: "keywords".into(),
            matches,
            entries_indexed: self.entries.len(),
            elapsed_secs: start.elapsed().as_secs_f32(),
        }
    }
}

// --- Internal helpers --------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn process_path_for_search(
    normalizer: &Normalizer,
    ident_normalizer: &Normalizer,
    slicer: &Slicer,
    minhasher: &MinHasher,
    granularity: Granularity,
    path: &Path,
    k: usize,
    window: usize,
    only_lang: Option<&str>,
) -> Vec<SearchEntry> {
    use tokei_dedup_lang_config as lang_config;

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

    let blinded = normalizer.process(&content, lang_key);
    // Use strict mode for identifier extraction so text is preserved
    let strict = ident_normalizer.process(&content, lang_key);

    if blinded.tokens.is_empty() {
        return Vec::new();
    }

    match granularity {
        Granularity::File => {
            build_search_entry(minhasher, path, lang_key, None, &blinded.tokens, &strict.tokens, k, window)
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
                    let blinded_toks = tokens_in_byte_range(&blinded.tokens, g.byte_start, g.byte_end);
                    let strict_toks = tokens_in_byte_range(&strict.tokens, g.byte_start, g.byte_end);
                    let info = GranuleInfo {
                        fn_name: g.name,
                        line_start: g.line_start,
                        line_end: g.line_end,
                    };
                    build_search_entry(minhasher, &g.file, lang_key, Some(info), blinded_toks, strict_toks, k, window)
                })
                .collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_search_entry(
    minhasher: &MinHasher,
    path: &Path,
    lang: &str,
    granule: Option<GranuleInfo>,
    blinded_tokens: &[NormalizedToken],
    strict_tokens: &[NormalizedToken],
    k: usize,
    window: usize,
) -> Option<SearchEntry> {
    let fps = fingerprint_tokens(blinded_tokens, k, window);
    if fps.is_empty() {
        return None;
    }
    let unique_set: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
    let unique_count = unique_set.len() as u32;
    let unique_vec: Vec<u64> = unique_set.iter().copied().collect();
    let sketch = minhasher.sketch(&unique_vec);
    let ident_tokens = extract_ident_tokens(strict_tokens);

    Some(SearchEntry {
        path: path.to_owned(),
        lang: lang.to_string(),
        granule,
        sketch_vec: sketch.to_vec(),
        unique_fps: unique_count,
        unique_set: unique_vec,
        ident_tokens,
    })
}

fn tokens_in_byte_range(tokens: &[NormalizedToken], start: u32, end: u32) -> &[NormalizedToken] {
    let lo = tokens.partition_point(|t| t.byte_start < start);
    let hi = tokens.partition_point(|t| t.byte_start < end);
    &tokens[lo..hi]
}

fn resolve_lang_key(hint: &str) -> Option<&'static str> {
    use tokei_dedup_lang_config as lang_config;
    // Try exact key match ("Python", "Rust", etc.)
    if lang_config::by_key(hint).is_some() {
        // by_key returns &'static LanguageDef but not the key itself; we need a
        // &'static str. Walk the registry to get the interned key.
        for (key, _) in lang_config::all() {
            if key.eq_ignore_ascii_case(hint) {
                return Some(key);
            }
        }
    }
    // Try common extensions ("py", "rs", etc.)
    if let Some((key, _)) = lang_config::by_extension(hint) {
        return Some(key);
    }
    None
}

fn guess_lang_from_snippet(snippet: &str) -> &'static str {
    let lower = snippet.to_lowercase();
    if lower.contains("def ") && lower.contains(':') {
        "Python"
    } else if lower.contains("fn ") && lower.contains("->") {
        "Rust"
    } else if lower.contains("func ") {
        "Go"
    } else if lower.contains("function ") || lower.contains("=>") || lower.contains("const ") {
        "JavaScript"
    } else if lower.contains("public ") || lower.contains("private ") {
        "Java"
    } else {
        "Python" // fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_identifier_snake_case() {
        assert_eq!(
            split_identifier("validate_email_address"),
            vec!["validate", "email", "address"]
        );
    }

    #[test]
    fn split_identifier_camel_case() {
        assert_eq!(
            split_identifier("validateEmailAddress"),
            vec!["validate", "email", "address"]
        );
    }

    #[test]
    fn split_identifier_acronym() {
        assert_eq!(split_identifier("HTMLParser"), vec!["html", "parser"]);
    }

    #[test]
    fn split_identifier_single_char_dropped() {
        assert_eq!(split_identifier("a_b_validate"), vec!["validate"]);
    }

    #[test]
    fn split_identifier_all_lower() {
        assert_eq!(split_identifier("validate"), vec!["validate"]);
    }

    #[test]
    fn snippet_search_finds_similar_function() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/fixtures/search-corpus");
        if !corpus.exists() {
            return;
        }
        let opts = ScanOptions {
            granularity: Granularity::Function,
            blind: tokei_dedup_core::BlindMode::Aggressive,
            ..ScanOptions::default()
        };
        let index = SearchIndex::build(&corpus, &opts);
        assert!(index.entries.len() > 10);

        let sketch = r#"
def validate_email(email):
    if not email or '@' not in email:
        return False
    parts = email.split('@')
    if len(parts) != 2:
        return False
    local, domain = parts
    if '.' not in domain:
        return False
    return True
"#;
        let result = index.search_snippet(sketch, Some("Python"), 0.2, 5);
        assert!(!result.matches.is_empty());
        assert_eq!(
            result.matches[0].fn_name.as_deref(),
            Some("verify_email_format")
        );
    }

    #[test]
    fn keyword_search_finds_by_identifiers() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/fixtures/search-corpus");
        if !corpus.exists() {
            return;
        }
        let opts = ScanOptions {
            granularity: Granularity::Function,
            blind: tokei_dedup_core::BlindMode::Aggressive,
            ..ScanOptions::default()
        };
        let index = SearchIndex::build(&corpus, &opts);

        let result = index.search_keywords("retry backoff exponential", 5);
        assert!(!result.matches.is_empty());
        assert_eq!(
            result.matches[0].fn_name.as_deref(),
            Some("retry_with_backoff")
        );
    }

    #[test]
    fn no_matches_for_novel_code() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/fixtures/search-corpus");
        if !corpus.exists() {
            return;
        }
        let opts = ScanOptions {
            granularity: Granularity::Function,
            blind: tokei_dedup_core::BlindMode::Aggressive,
            ..ScanOptions::default()
        };
        let index = SearchIndex::build(&corpus, &opts);

        let sketch = r#"
def calculate_haversine_distance(lat1, lon1, lat2, lon2):
    import math
    R = 6371
    dlat = math.radians(lat2 - lat1)
    dlon = math.radians(lon2 - lon1)
    a = math.sin(dlat/2)**2 + math.cos(math.radians(lat1)) * math.cos(math.radians(lat2)) * math.sin(dlon/2)**2
    c = 2 * math.asin(math.sqrt(a))
    return R * c
"#;
        let result = index.search_snippet(sketch, Some("Python"), 0.2, 5);
        assert!(result.matches.is_empty());
    }

    #[test]
    fn index_persistence_roundtrip() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/fixtures/search-corpus");
        if !corpus.exists() {
            return;
        }
        let opts = ScanOptions {
            granularity: Granularity::Function,
            blind: tokei_dedup_core::BlindMode::Aggressive,
            ..ScanOptions::default()
        };
        let index = SearchIndex::build(&corpus, &opts);
        let dir = std::env::temp_dir().join("dupe-test-index");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test-index.json");
        index.save(&path).unwrap();
        let loaded = SearchIndex::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), index.entries.len());

        // Loaded index should produce the same search results
        let result = loaded.search_keywords("email format verify", 3);
        assert!(!result.matches.is_empty());
        assert_eq!(
            result.matches[0].fn_name.as_deref(),
            Some("verify_email_format")
        );
        std::fs::remove_file(&path).ok();
    }
}
