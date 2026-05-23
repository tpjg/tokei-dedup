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
}
