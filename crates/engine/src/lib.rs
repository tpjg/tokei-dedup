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

use ignore::{overrides::OverrideBuilder, WalkBuilder};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
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

pub use tokei_dedup_classifier::{Finding as ClassifiedFinding, ItemRef as FindingEndpoint, Tag};
pub use tokei_dedup_core::BlindMode as BlindModeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    File,
    Function,
}

/// Built-in directory/file names skipped at every depth when
/// [`WalkOptions::apply_default_excludes`] is set. Tries to cover the common cases that
/// projects don't always remember to gitignore (build outputs, dependency dirs, virtual
/// environments, editor metadata). Pure name match, no globs — `node_modules/foo.js` is
/// skipped but `my-node_modules/foo.js` is not.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // VCS
    ".git",
    ".svn",
    ".hg",
    // JS / Node
    "node_modules",
    "bower_components",
    ".next",
    ".nuxt",
    // Rust
    "target",
    // Generic build outputs
    "dist",
    "build",
    "out",
    "bin",
    "obj",
    "coverage",
    // Go / PHP
    "vendor",
    // Python
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    // Editor metadata
    ".idea",
    ".vscode",
];

/// Walker filtering options. All three layers compose — gitignore + default-excludes +
/// custom patterns. Disable any with the respective flag.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Honor `.gitignore`, `.ignore`, `.git/info/exclude`, and the user-global gitignore.
    /// Also filters hidden files (`.foo`). Equivalent to `WalkBuilder::standard_filters`.
    pub respect_gitignore: bool,
    /// Apply [`DEFAULT_EXCLUDES`] at every depth.
    pub apply_default_excludes: bool,
    /// Extra gitignore-style patterns to skip. Each entry may be a literal name (`target`)
    /// or a glob (`**/test_data/**`). A leading `!` is accepted but ignored — these are
    /// always exclude patterns from the caller's perspective.
    pub custom_excludes: Vec<String>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            apply_default_excludes: true,
            custom_excludes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub blind: BlindMode,
    pub granularity: Granularity,
    pub k: usize,
    pub window: usize,
    pub use_naive: bool,
    pub min_jaccard: f32,
    pub min_shared: u32,
    pub max_bucket: usize,
    pub only_lang: Option<String>,
    pub walk: WalkOptions,
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
            walk: WalkOptions::default(),
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

    let paths: Vec<PathBuf> = walk_filtered(workspace, &opts.walk);
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

/// Walk `workspace` honoring the three-layer ignore rules:
///
/// 1. `WalkBuilder::standard_filters(respect_gitignore)` — `.gitignore`, `.ignore`,
///    `.git/info/exclude`, the user-global gitignore, and hidden-file filtering.
/// 2. [`DEFAULT_EXCLUDES`] as gitignore-style overrides when `apply_default_excludes`.
/// 3. User patterns from `custom_excludes`, prefixed with `!` so they're treated as
///    excludes by `OverrideBuilder`.
///
/// `OverrideBuilder` reads `!pat` as "blacklist," and when *every* override is a
/// blacklist (the case here) non-matching paths are included normally.
pub fn walk_filtered(workspace: &Path, opts: &WalkOptions) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(workspace);
    builder.standard_filters(opts.respect_gitignore);

    let mut ob = OverrideBuilder::new(workspace);
    let mut any_pattern = false;
    if opts.apply_default_excludes {
        for &name in DEFAULT_EXCLUDES {
            if ob.add(&format!("!{name}")).is_ok() {
                any_pattern = true;
            }
        }
    }
    for pat in &opts.custom_excludes {
        let trimmed = pat.trim_start_matches('!').trim();
        if trimmed.is_empty() {
            continue;
        }
        if ob.add(&format!("!{trimmed}")).is_ok() {
            any_pattern = true;
        }
    }
    if any_pattern {
        if let Ok(overrides) = ob.build() {
            builder.overrides(overrides);
        }
    }

    builder
        .build()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.into_path())
        .collect()
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

// --- Incremental engine (M6) ---------------------------------------------------------

/// Identity for an indexable entry, stable across LSH index ID reassignments.
/// One per file in file mode; one per granule (function) in function mode.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemKey {
    pub path: PathBuf,
    pub granule: Option<GranuleInfo>,
}

impl ItemKey {
    fn from_meta(meta: &tokei_dedup_index::FileMeta) -> Self {
        Self {
            path: meta.path.clone(),
            granule: meta.granule.clone(),
        }
    }
}

/// Pair identity used by [`IncrementalResult`] for diffing. Endpoints are stored
/// in a canonical order ([`PairKey::canonical`]) so `(a, b)` and `(b, a)` hash
/// to the same key.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct PairKey {
    pub a: ItemKey,
    pub b: ItemKey,
}

impl PairKey {
    pub fn canonical(a: ItemKey, b: ItemKey) -> Self {
        if a <= b {
            Self { a, b }
        } else {
            Self { a: b, b: a }
        }
    }
}

/// Diff between the previous and current finding set, produced by
/// [`IncrementalEngine::update`].
#[derive(Debug, Default)]
pub struct IncrementalResult {
    /// Pairs that crossed the threshold for the first time since the previous
    /// `update` (or since the initial scan).
    pub added: Vec<Finding>,
    /// Pairs that were findings before and are no longer (either because the
    /// shared content went away or one endpoint was deleted).
    pub removed: Vec<PairKey>,
    /// Pairs that are still findings but whose score or tags changed (e.g.
    /// because surrounding context shifted classifier signals).
    pub updated: Vec<Finding>,
    /// Wall-clock time the `update` call took.
    pub elapsed_secs: f32,
}

/// Long-lived index + caches for sub-workspace updates. The LSP server holds
/// one of these for the session: an [`initial_scan`](Self::initial_scan) at
/// `initialized` time, then [`update`](Self::update) on each save with the
/// set of changed paths.
///
/// LSH backend only (per `DESIGN.md` — the naive backend stays an oracle for
/// the test suite). Re-running a full `initial_scan` is always equivalent to
/// running an `update` covering every path; this is the basis of the
/// round-trip test in `crates/engine/src/lib.rs::incremental_tests`.
pub struct IncrementalEngine {
    workspace_root: PathBuf,
    opts: ScanOptions,
    normalizer: Normalizer,
    slicer: Slicer,
    minhasher: MinHasher,
    index: LshIndex,
    /// Per-entry unique fingerprint set, the verifier's input.
    /// Keyed by `ItemKey` (not LSH ID) so [`LshIndex::compact`] doesn't
    /// invalidate this map.
    unique_sets: HashMap<ItemKey, HashSet<u64>>,
    /// Current finding set, pair-keyed for cheap delta computation on update.
    findings: HashMap<PairKey, Finding>,
    /// Compact when the LSH tombstone ratio exceeds this. 0.25 is "drop the
    /// pile every time a quarter of entries are dead"; cheap enough.
    compact_threshold: f32,
}

impl IncrementalEngine {
    /// Create an engine rooted at `workspace_root` with the given scan options.
    /// `opts.use_naive` is ignored (always LSH); everything else is honoured.
    pub fn new(workspace_root: PathBuf, opts: ScanOptions) -> Self {
        Self {
            normalizer: Normalizer::new(opts.blind),
            slicer: Slicer::new(),
            minhasher: MinHasher::new(DEFAULT_MINHASH_SEED),
            index: LshIndex::with_defaults(),
            unique_sets: HashMap::new(),
            findings: HashMap::new(),
            workspace_root,
            opts,
            compact_threshold: 0.25,
        }
    }

    /// Equivalent ScanOptions accessor. The LSP layer reads this to log the
    /// active config.
    pub fn opts(&self) -> &ScanOptions {
        &self.opts
    }

    /// Wipe the index and re-fingerprint the whole workspace. Returns a
    /// [`ScanResult`] shaped exactly like a one-shot [`scan`] call so the LSP
    /// can log identical "scan complete" messages.
    pub fn initial_scan(&mut self) -> ScanResult {
        let start = Instant::now();
        self.index = LshIndex::with_defaults();
        self.unique_sets.clear();
        self.findings.clear();

        let paths = walk_filtered(&self.workspace_root, &self.opts.walk);
        let files_walked = paths.len();

        let items: Vec<Item> = paths
            .iter()
            .flat_map(|p| {
                process_path(
                    &self.normalizer,
                    &self.slicer,
                    &self.minhasher,
                    self.opts.granularity,
                    p,
                    self.opts.k,
                    self.opts.window,
                    self.opts.only_lang.as_deref(),
                )
            })
            .collect();

        let entries_indexed = items.len();
        for item in items {
            self.insert_item(item);
        }

        let candidate_pairs = self.index.candidate_pair_count();
        let mut findings: Vec<Finding> = Vec::new();
        for (a_id, b_id, est) in self.index.candidate_pairs() {
            if est < self.opts.min_jaccard {
                continue;
            }
            if let Some(finding) = self.verify_and_classify(a_id, b_id, est) {
                let key = self.pair_key_from_ids(a_id, b_id);
                self.findings.insert(key, finding.clone());
                findings.push(finding);
            }
        }

        if self.opts.granularity == Granularity::Function {
            findings.retain(|f| !same_endpoint(&f.a, &f.b));
            self.findings
                .retain(|_, f| !same_endpoint(&f.a, &f.b));
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
            entries_indexed,
            candidate_pairs,
            elapsed_secs: start.elapsed().as_secs_f32(),
            backend: "lsh",
        }
    }

    /// Apply `changed_paths` (files added, modified, deleted, or renamed) and
    /// return the diff against the previous finding set.
    ///
    /// For each path:
    ///   1. Capture currently-affected pair keys via [`LshIndex::partners_of`]
    ///      on the *live* IDs (this works correctly because tombstoning is
    ///      deferred to step 2).
    ///   2. [`LshIndex::remove_by_path`] tombstones the old entries.
    ///   3. Re-fingerprint the path; insert new entries.
    ///   4. Enumerate live partners of the new IDs to discover candidate pairs.
    ///
    /// Then verify+classify all candidates collected in step 4, diff against
    /// `self.findings`, and update.
    ///
    /// Paths that no longer exist on disk become pure deletions (step 3 is a
    /// no-op). Paths that are added for the first time have no step 1.
    pub fn update(&mut self, changed_paths: &[PathBuf]) -> IncrementalResult {
        let start = Instant::now();
        let mut result = IncrementalResult::default();
        if changed_paths.is_empty() {
            result.elapsed_secs = start.elapsed().as_secs_f32();
            return result;
        }

        // Step 1: snapshot the pair keys that touch any changed path. We do
        // this BEFORE the removal step so the LSH IDs we walk are still
        // alive; partners_of works on dead IDs too but `meta()` is clearer
        // when the entry is alive.
        let mut touched_pairs: HashSet<PairKey> = HashSet::new();
        // Per path, also remember the ItemKeys that existed pre-update so we
        // can drop their unique_sets entries if they don't reappear.
        let mut pre_update_item_keys: HashSet<ItemKey> = HashSet::new();
        for path in changed_paths {
            for old_id in self.index.ids_for_path(path) {
                let meta = self.index.meta(old_id);
                pre_update_item_keys.insert(ItemKey::from_meta(meta));
                for (partner_id, _est) in self.index.partners_of(old_id) {
                    touched_pairs.insert(self.pair_key_from_ids(old_id, partner_id));
                }
            }
        }

        // Step 2: tombstone.
        for path in changed_paths {
            self.index.remove_by_path(path);
        }

        // Step 3: re-fingerprint and re-insert.
        let mut new_candidates: HashSet<(u32, u32)> = HashSet::new();
        let mut post_update_item_keys: HashSet<ItemKey> = HashSet::new();
        for path in changed_paths {
            let items = process_path(
                &self.normalizer,
                &self.slicer,
                &self.minhasher,
                self.opts.granularity,
                path,
                self.opts.k,
                self.opts.window,
                self.opts.only_lang.as_deref(),
            );
            for item in items {
                let key = ItemKey {
                    path: item.path.clone(),
                    granule: item.granule.clone(),
                };
                post_update_item_keys.insert(key.clone());
                let new_id = self.insert_item(item);
                // Discover candidate pairs touching the new entry.
                for (partner_id, est) in self.index.partners_of(new_id) {
                    if est < self.opts.min_jaccard {
                        continue;
                    }
                    let (lo, hi) = if new_id < partner_id {
                        (new_id, partner_id)
                    } else {
                        (partner_id, new_id)
                    };
                    new_candidates.insert((lo, hi));
                    touched_pairs.insert(self.pair_key_from_ids(new_id, partner_id));
                }
            }
        }

        // Drop unique_sets for items that existed pre-update and didn't come
        // back. Leaving stale entries would just leak memory.
        for stale in pre_update_item_keys.difference(&post_update_item_keys) {
            self.unique_sets.remove(stale);
        }

        // Step 4: verify + classify the new candidate set. Build a fresh map
        // for the touched pair keys.
        let mut fresh: HashMap<PairKey, Finding> = HashMap::new();
        for (a_id, b_id) in &new_candidates {
            let est = sketch_jaccard(&self.index, *a_id, *b_id);
            if let Some(finding) = self.verify_and_classify(*a_id, *b_id, est) {
                if self.opts.granularity == Granularity::Function
                    && same_endpoint(&finding.a, &finding.b)
                {
                    continue;
                }
                let pk = self.pair_key_from_ids(*a_id, *b_id);
                fresh.insert(pk, finding);
            }
        }

        // Diff: for every pair_key in touched_pairs, compare old vs fresh.
        for pk in &touched_pairs {
            let old = self.findings.get(pk).cloned();
            let new = fresh.remove(pk);
            match (old, new) {
                (None, None) => {} // touched but never crossed the threshold
                (None, Some(finding)) => {
                    self.findings.insert(pk.clone(), finding.clone());
                    result.added.push(finding);
                }
                (Some(_), None) => {
                    self.findings.remove(pk);
                    result.removed.push(pk.clone());
                }
                (Some(prev), Some(curr)) => {
                    if finding_changed(&prev, &curr) {
                        result.updated.push(curr.clone());
                    }
                    self.findings.insert(pk.clone(), curr);
                }
            }
        }
        // Anything in `fresh` that wasn't in touched_pairs is a brand-new
        // pair to a partner not previously seen — emit as added.
        for (pk, finding) in fresh {
            self.findings.insert(pk, finding.clone());
            result.added.push(finding);
        }

        // Lazy compaction: drop tombstones if they've piled up. IDs change
        // here, but unique_sets and findings are keyed by ItemKey/PairKey
        // so they're unaffected.
        if self.index.tombstone_ratio() > self.compact_threshold {
            self.index.compact();
        }

        result.elapsed_secs = start.elapsed().as_secs_f32();
        result
    }

    /// Current finding set as a flat iterator. Order is unspecified; the
    /// caller sorts by their preferred metric.
    pub fn findings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.values()
    }

    /// Number of pair-keyed findings currently held.
    pub fn finding_count(&self) -> usize {
        self.findings.len()
    }

    /// Insert one item into the LSH index and the unique-sets map. Returns
    /// the new LSH ID.
    fn insert_item(&mut self, item: Item) -> u32 {
        let key = ItemKey {
            path: item.path.clone(),
            granule: item.granule.clone(),
        };
        let id = if let Some(g) = item.granule.clone() {
            self.index
                .add_granule(item.path.clone(), item.lang, g, item.sketch, item.unique_fps)
        } else {
            self.index
                .add_file(item.path.clone(), item.lang, item.sketch, item.unique_fps)
        };
        self.unique_sets.insert(key, item.unique_set);
        id
    }

    /// Verify a candidate pair against the exact unique-set Jaccard and run
    /// it through the classifier. Returns `None` if exact Jaccard falls below
    /// `min_jaccard` or either unique set is missing (shouldn't happen).
    fn verify_and_classify(&self, a_id: u32, b_id: u32, est: f32) -> Option<Finding> {
        let meta_a = self.index.meta(a_id);
        let meta_b = self.index.meta(b_id);
        let key_a = ItemKey::from_meta(meta_a);
        let key_b = ItemKey::from_meta(meta_b);
        let set_a = self.unique_sets.get(&key_a)?;
        let set_b = self.unique_sets.get(&key_b)?;
        let v = verify(a_id, b_id, est, set_a, set_b);
        if v.exact_jaccard < self.opts.min_jaccard {
            return None;
        }
        Some(classify_with_root(
            &v,
            meta_to_item_ref(meta_a),
            meta_to_item_ref(meta_b),
            Some(&self.workspace_root),
        ))
    }

    fn pair_key_from_ids(&self, a_id: u32, b_id: u32) -> PairKey {
        let a = ItemKey::from_meta(self.index.meta(a_id));
        let b = ItemKey::from_meta(self.index.meta(b_id));
        PairKey::canonical(a, b)
    }
}

fn sketch_jaccard(index: &LshIndex, a_id: u32, b_id: u32) -> f32 {
    // The LshIndex doesn't expose sketches directly; compute via partners_of
    // on `a_id` filtered to `b_id`. Cheap because partners_of walks the
    // band buckets once and we discard everything except the b_id entry.
    for (partner, est) in index.partners_of(a_id) {
        if partner == b_id {
            return est;
        }
    }
    // If b_id isn't a partner (no shared band), conservative fallback.
    0.0
}

/// Findings differ "meaningfully" if score, exact Jaccard, or tag set changed.
/// Sub-epsilon score wobble is treated as no change so we don't spam the LSP
/// client with updates on every save that nudges the classifier by 1e-7.
fn finding_changed(prev: &Finding, curr: &Finding) -> bool {
    if (prev.score - curr.score).abs() > 1e-4 {
        return true;
    }
    if (prev.exact_jaccard - curr.exact_jaccard).abs() > 1e-4 {
        return true;
    }
    if prev.shared != curr.shared {
        return true;
    }
    let prev_tags: HashSet<&str> = prev.tags.iter().map(|t| t.as_str()).collect();
    let curr_tags: HashSet<&str> = curr.tags.iter().map(|t| t.as_str()).collect();
    prev_tags != curr_tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_options_default_is_sensible() {
        let o = ScanOptions::default();
        assert_eq!(o.granularity, Granularity::File);
        assert!(!o.use_naive);
        assert!(o.min_jaccard > 0.0 && o.min_jaccard < 1.0);
        assert!(o.walk.respect_gitignore);
        assert!(o.walk.apply_default_excludes);
    }

    /// Build a temp dir with `src/main.rs` plus a few common-build-dir noise files.
    /// Returns the dir handle (drop to clean up) and its path.
    fn temp_project_with_build_noise() -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new("tokei-dedup-walk").unwrap();
        let root = dir.path().to_owned();
        let make = |rel: &str, content: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        };
        make("src/main.rs", "fn main() {}\n");
        make("src/lib.rs", "pub fn x() {}\n");
        make("target/debug/build/foo.rs", "// build artifact\n");
        make("node_modules/foo/index.js", "module.exports = {};\n");
        make("dist/bundle.js", "console.log('built');\n");
        make("vendor/lib.go", "package vendored\n");
        make(".venv/lib/site.py", "# venv\n");
        (dir, root)
    }

    #[test]
    fn default_excludes_skip_build_dirs() {
        let (_dir, root) = temp_project_with_build_noise();
        let paths = walk_filtered(&root, &WalkOptions::default());
        let names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.strip_prefix(&root).ok())
            .map(|p| p.display().to_string())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("main.rs")));
        assert!(names.iter().any(|n| n.ends_with("lib.rs")));
        // All the build/dependency dirs must be absent.
        for noisy in &["target", "node_modules", "dist", "vendor", ".venv"] {
            assert!(
                !names.iter().any(|n| n.contains(noisy)),
                "expected {noisy} to be excluded, got files: {names:?}"
            );
        }
    }

    #[test]
    fn disabling_default_excludes_lets_build_dirs_through() {
        let (_dir, root) = temp_project_with_build_noise();
        let opts = WalkOptions {
            respect_gitignore: false,
            apply_default_excludes: false,
            custom_excludes: vec![],
        };
        let paths = walk_filtered(&root, &opts);
        // With everything off we see every file we created.
        assert!(paths.len() >= 6);
    }

    #[test]
    fn custom_excludes_skip_user_patterns() {
        let (_dir, root) = temp_project_with_build_noise();
        // Disable defaults; rely on a single custom pattern.
        let opts = WalkOptions {
            respect_gitignore: false,
            apply_default_excludes: false,
            custom_excludes: vec!["target".into()],
        };
        let paths = walk_filtered(&root, &opts);
        assert!(paths.iter().any(|p| p.ends_with("main.rs")));
        assert!(
            !paths.iter().any(|p| p.components().any(|c| c.as_os_str() == "target")),
            "custom_excludes should drop target/"
        );
    }

    #[test]
    fn gitignore_layer_is_honored() {
        let (_dir, root) = temp_project_with_build_noise();
        // The `ignore` crate only reads .gitignore when the directory looks like a git
        // repo (presence of .git/). Fake that with an empty `.git` dir — saves a `git
        // init` subprocess in the test.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("custom_out")).unwrap();
        fs::write(root.join("custom_out/foo.rs"), "// gen\n").unwrap();
        fs::write(root.join(".gitignore"), "custom_out/\n").unwrap();
        let opts = WalkOptions {
            respect_gitignore: true,
            apply_default_excludes: false,
            custom_excludes: vec![],
        };
        let paths = walk_filtered(&root, &opts);
        assert!(
            !paths.iter().any(|p| p.components().any(|c| c.as_os_str() == "custom_out")),
            ".gitignore should hide custom_out/"
        );
        // src/ files still appear.
        assert!(paths.iter().any(|p| p.ends_with("main.rs")));
    }

    // --- IncrementalEngine tests (M6 phase 2) ----------------------------------------

    fn incremental_opts() -> ScanOptions {
        // File mode keeps these tests independent of the tree-sitter slicer
        // (which would otherwise need fixture languages built). The engine
        // logic exercised is the same.
        ScanOptions {
            granularity: Granularity::File,
            min_jaccard: 0.5,
            ..ScanOptions::default()
        }
    }

    /// Same Rust content twice. file_a / file_b filenames so the engine treats
    /// them as separate files. Returns absolute paths.
    fn write_two_clones(root: &Path) -> (PathBuf, PathBuf) {
        let body = (0..40)
            .map(|i| format!("fn func_{i}(x: i32) -> i32 {{ x + {i} * 7 }}\n"))
            .collect::<String>();
        let a = root.join("a.rs");
        let b = root.join("b.rs");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        (a, b)
    }

    fn finding_paths(f: &Finding) -> (PathBuf, PathBuf) {
        if f.a.path <= f.b.path {
            (f.a.path.clone(), f.b.path.clone())
        } else {
            (f.b.path.clone(), f.a.path.clone())
        }
    }

    fn collect_finding_paths(findings: &HashMap<PairKey, Finding>) -> Vec<(PathBuf, PathBuf)> {
        let mut out: Vec<_> = findings.values().map(finding_paths).collect();
        out.sort();
        out
    }

    #[test]
    fn incremental_initial_scan_matches_full_scan() {
        let dir = tempdir::TempDir::new("inc-init").unwrap();
        let root = dir.path().to_owned();
        write_two_clones(&root);
        fs::create_dir_all(root.join(".git")).unwrap();

        let opts = incremental_opts();
        let mut engine = IncrementalEngine::new(root.clone(), opts.clone());
        let result_inc = engine.initial_scan();

        let result_full = scan(&root, &opts);
        assert_eq!(
            result_inc.findings.len(),
            result_full.findings.len(),
            "IncrementalEngine::initial_scan must produce the same number of findings as scan()"
        );
        assert!(!result_inc.findings.is_empty(), "expected at least one pair");
    }

    #[test]
    fn incremental_update_no_change_is_no_op_diff() {
        let dir = tempdir::TempDir::new("inc-noop").unwrap();
        let root = dir.path().to_owned();
        let (a, _b) = write_two_clones(&root);
        fs::create_dir_all(root.join(".git")).unwrap();

        let mut engine = IncrementalEngine::new(root.clone(), incremental_opts());
        engine.initial_scan();
        let before = engine.finding_count();
        let diff = engine.update(&[a]); // same content on disk
        assert_eq!(engine.finding_count(), before, "finding set unchanged");
        assert!(
            diff.added.is_empty() && diff.removed.is_empty() && diff.updated.is_empty(),
            "expected empty diff for an unchanged-content update; got {diff:?}"
        );
    }

    #[test]
    fn incremental_update_removes_pair_when_clone_breaks() {
        let dir = tempdir::TempDir::new("inc-rm").unwrap();
        let root = dir.path().to_owned();
        let (a, _b) = write_two_clones(&root);
        fs::create_dir_all(root.join(".git")).unwrap();

        let mut engine = IncrementalEngine::new(root.clone(), incremental_opts());
        engine.initial_scan();
        assert!(engine.finding_count() >= 1);

        // Replace a.rs with entirely different content so the pair drops.
        fs::write(
            &a,
            "fn totally_different() -> &'static str { \"this is unique content\" }\n",
        )
        .unwrap();
        let diff = engine.update(&[a]);

        assert_eq!(
            engine.finding_count(),
            0,
            "pair should be gone after content diverges"
        );
        assert!(
            !diff.removed.is_empty(),
            "expected at least one removed pair; got {diff:?}"
        );
        assert!(diff.added.is_empty(), "no new pair should appear");
    }

    #[test]
    fn incremental_update_adds_pair_when_clone_appears() {
        let dir = tempdir::TempDir::new("inc-add").unwrap();
        let root = dir.path().to_owned();
        fs::create_dir_all(root.join(".git")).unwrap();
        let unique = root.join("solo.rs");
        fs::write(&unique, "fn alone() -> i32 { 42 }\n").unwrap();

        let mut engine = IncrementalEngine::new(root.clone(), incremental_opts());
        engine.initial_scan();
        assert_eq!(engine.finding_count(), 0, "no pairs in a single-file workspace");

        // Create a clone pair: replace solo and add a copy.
        let (a, b) = write_two_clones(&root);
        let diff = engine.update(&[a, b]);

        assert!(
            engine.finding_count() >= 1,
            "expected a pair to surface after adding cloned files"
        );
        assert!(
            !diff.added.is_empty(),
            "expected at least one added pair; got {diff:?}"
        );
    }

    #[test]
    fn incremental_update_handles_deleted_file() {
        let dir = tempdir::TempDir::new("inc-del").unwrap();
        let root = dir.path().to_owned();
        let (a, _b) = write_two_clones(&root);
        fs::create_dir_all(root.join(".git")).unwrap();

        let mut engine = IncrementalEngine::new(root.clone(), incremental_opts());
        engine.initial_scan();
        assert!(engine.finding_count() >= 1);

        fs::remove_file(&a).unwrap();
        let diff = engine.update(&[a]);

        assert_eq!(engine.finding_count(), 0, "pair gone after deletion");
        assert!(!diff.removed.is_empty(), "expected removed pairs; got {diff:?}");
    }

    #[test]
    fn incremental_round_trip_against_full_rebuild() {
        // After a sequence of updates, the finding set must equal what a fresh
        // initial_scan over the final disk state would produce. This is the
        // strongest correctness check we can run without a verifier oracle —
        // it catches any divergence between the incremental and one-shot paths.
        let dir = tempdir::TempDir::new("inc-round").unwrap();
        let root = dir.path().to_owned();
        fs::create_dir_all(root.join(".git")).unwrap();
        let a = root.join("a.rs");
        let b = root.join("b.rs");
        let c = root.join("c.rs");
        let body1 = (0..40)
            .map(|i| format!("fn body1_{i}(x: i32) -> i32 {{ x + {i} * 7 }}\n"))
            .collect::<String>();
        let body2 = (0..40)
            .map(|i| format!("fn body2_{i}(y: i32) -> i32 {{ y * {i} - 3 }}\n"))
            .collect::<String>();

        // Initial state: a=body1, b=body1, c=body1 → three-way clone.
        fs::write(&a, &body1).unwrap();
        fs::write(&b, &body1).unwrap();
        fs::write(&c, &body1).unwrap();

        let opts = incremental_opts();
        let mut engine = IncrementalEngine::new(root.clone(), opts.clone());
        engine.initial_scan();

        // Mutate: a stays body1, b becomes body2, c gets deleted.
        fs::write(&b, &body2).unwrap();
        fs::remove_file(&c).unwrap();
        engine.update(&[b.clone(), c.clone()]);

        // Now build a fresh engine over the *same* final disk state.
        let mut fresh = IncrementalEngine::new(root.clone(), opts.clone());
        fresh.initial_scan();

        let after_updates = collect_finding_paths(&engine.findings);
        let from_scratch = collect_finding_paths(&fresh.findings);
        assert_eq!(
            after_updates, from_scratch,
            "incremental updates must converge to the same finding set as a from-scratch scan"
        );
    }

    #[test]
    fn incremental_update_compacts_when_tombstones_pile_up() {
        // Force enough tombstones to cross the 0.25 threshold and verify
        // compaction runs (tombstone_ratio resets to 0).
        let dir = tempdir::TempDir::new("inc-compact").unwrap();
        let root = dir.path().to_owned();
        fs::create_dir_all(root.join(".git")).unwrap();
        let body = (0..40)
            .map(|i| format!("fn f_{i}() -> i32 {{ {i} }}\n"))
            .collect::<String>();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
            fs::write(root.join(name), &body).unwrap();
        }

        let mut engine = IncrementalEngine::new(root.clone(), incremental_opts());
        engine.initial_scan();
        assert_eq!(engine.index.tombstone_ratio(), 0.0);

        // Mutate two of five files → 40 % tombstone ratio (above the 0.25 trigger).
        for name in ["a.rs", "b.rs"] {
            fs::write(
                root.join(name),
                format!("fn unique_{name} () -> i32 {{ 0 }}\n"),
            )
            .unwrap();
        }
        engine.update(&[root.join("a.rs"), root.join("b.rs")]);
        assert_eq!(
            engine.index.tombstone_ratio(),
            0.0,
            "compaction should have run during update"
        );
    }
}
