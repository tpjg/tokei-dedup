//! `dupe` — duplicate code finder.
//!
//! Milestone 1 surface: `dupe scan <dir>`. Walks a directory, normalizes each file by
//! language (via vendored tokei definitions), winnows fingerprints, and reports the
//! highest-overlap file pairs.

mod html;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
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

#[derive(Parser)]
#[command(name = "dupe", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory and report candidate duplicate file pairs.
    Scan {
        /// Directory to scan.
        dir: PathBuf,

        /// k-gram size.
        #[arg(long, default_value_t = tokei_dedup_fingerprinter::DEFAULT_K)]
        k: usize,

        /// Winnowing window size.
        #[arg(long, default_value_t = tokei_dedup_fingerprinter::DEFAULT_WINDOW)]
        window: usize,

        /// Number of pairs to print.
        #[arg(long, default_value_t = 20)]
        top: usize,

        /// Token blinding aggressiveness.
        #[arg(long, value_enum, default_value_t = Blind::Mild)]
        blind: Blind,

        /// Restrict to a single language (tokei key, e.g. `Rust`, `Python`).
        #[arg(long)]
        only_lang: Option<String>,

        /// `file`: compare whole files. `function`: tree-sitter-slice each supported
        /// file into per-function granules and compare those.
        #[arg(long, value_enum, default_value_t = Granularity::File)]
        granularity: Granularity,

        /// Use the naive all-pairs index (milestone-1 path). Slow on >5k files but
        /// produces exact shared-fingerprint counts.
        #[arg(long, conflicts_with = "min_jaccard")]
        use_naive: bool,

        /// LSH mode: minimum estimated Jaccard for a pair to be reported.
        #[arg(long, default_value_t = 0.5)]
        min_jaccard: f32,

        /// Naive mode only: minimum distinct shared fingerprints.
        #[arg(long, default_value_t = 10)]
        min_shared: u32,

        /// Naive mode only: drop fingerprint buckets larger than this.
        #[arg(long, default_value_t = 50)]
        max_bucket: usize,

        /// Hide per-file progress noise.
        #[arg(long, short)]
        quiet: bool,

        /// Write an HTML report to this path (in addition to the terminal output).
        #[arg(long)]
        html: Option<PathBuf>,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum Blind {
    Strict,
    Mild,
    Aggressive,
}

impl From<Blind> for BlindMode {
    fn from(b: Blind) -> Self {
        match b {
            Blind::Strict => BlindMode::Strict,
            Blind::Mild => BlindMode::Mild,
            Blind::Aggressive => BlindMode::Aggressive,
        }
    }
}

#[derive(Copy, Clone, ValueEnum, Debug, PartialEq, Eq)]
enum Granularity {
    File,
    Function,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan {
            dir,
            k,
            window,
            top,
            blind,
            only_lang,
            granularity,
            use_naive,
            min_jaccard,
            min_shared,
            max_bucket,
            quiet,
            html,
        } => scan(ScanArgs {
            dir,
            k,
            window,
            top,
            blind: blind.into(),
            only_lang,
            granularity,
            use_naive,
            min_jaccard,
            min_shared,
            max_bucket,
            quiet,
            html,
        }),
    }
}

struct ScanArgs {
    dir: PathBuf,
    k: usize,
    window: usize,
    top: usize,
    blind: BlindMode,
    only_lang: Option<String>,
    granularity: Granularity,
    use_naive: bool,
    min_jaccard: f32,
    min_shared: u32,
    max_bucket: usize,
    quiet: bool,
    html: Option<PathBuf>,
}

struct Item {
    path: PathBuf,
    lang: &'static str,
    granule: Option<GranuleInfo>,
    fps: Vec<Fingerprint>,
    sketch: Sketch,
    unique_fps: u32,
    /// Cached unique fingerprint hash set — built once at fingerprint time and reused
    /// by the verifier without recomputation.
    unique_set: HashSet<u64>,
}

fn scan(a: ScanArgs) -> Result<()> {
    let normalizer = Normalizer::new(a.blind);
    let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
    let slicer = Slicer::new();
    let start = Instant::now();

    let paths: Vec<PathBuf> = WalkDir::new(&a.dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();

    if !a.quiet {
        eprintln!("Scanning {} files under {}", paths.len(), a.dir.display());
    }

    let items: Vec<Item> = paths
        .par_iter()
        .flat_map(|p| {
            process_path(
                &normalizer,
                &slicer,
                &minhasher,
                a.granularity,
                p,
                a.k,
                a.window,
                a.only_lang.as_deref(),
            )
        })
        .collect();

    let normalize_elapsed = start.elapsed();
    if !a.quiet {
        let unit = if a.granularity == Granularity::Function {
            "granules"
        } else {
            "files"
        };
        eprintln!(
            "Normalized + fingerprinted {} {unit} in {:.2}s",
            items.len(),
            normalize_elapsed.as_secs_f32(),
        );
    }

    let (mut findings, backend_summary, candidate_count) = if a.use_naive {
        run_naive_pipeline(&items, a.min_shared, a.max_bucket, &a.dir)
    } else {
        run_lsh_pipeline(&items, a.min_jaccard, &a.dir)
    };

    // Self-pairs (the same granule on both sides — possible with nested-function
    // emission) are noise.
    if a.granularity == Granularity::Function {
        findings.retain(|f| !same_endpoint(&f.a, &f.b));
    }

    if !a.quiet {
        eprintln!("Index: {backend_summary}");
    }

    findings.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.shared.cmp(&x.shared))
    });

    if findings.is_empty() {
        println!("No candidate duplicates.");
        if let Some(html_path) = &a.html {
            write_html(
                &findings,
                &items,
                candidate_count,
                a.use_naive,
                &a,
                start.elapsed().as_secs_f32(),
                html_path,
            )?;
            eprintln!("HTML report: {}", html_path.display());
        }
        return Ok(());
    }

    let total = findings.len();
    let shown = a.top.min(total);
    println!(
        "Top {} finding(s) of {} (score ↓, granularity={:?}, params: k={}, w={}, blind={:?}):",
        shown, total, a.granularity, a.k, a.window, a.blind,
    );
    for (rank, f) in findings.iter().take(a.top).enumerate() {
        let lang_tag = if f.a.lang == f.b.lang {
            f.a.lang.clone()
        } else {
            format!("{}↔{}", f.a.lang, f.b.lang)
        };
        let tag_list = if f.tags.is_empty() {
            String::new()
        } else {
            format!(
                " [{}]",
                f.tags
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        println!(
            "  {:>3}. score={:.3} j={:.3} shared={:>4} ({}/{}) [{}]{}",
            rank + 1,
            f.score,
            f.exact_jaccard,
            f.shared,
            f.a.unique_fps,
            f.b.unique_fps,
            lang_tag,
            tag_list,
        );
        println!("       a: {}", format_item_ref(&f.a));
        println!("       b: {}", format_item_ref(&f.b));
    }

    if let Some(html_path) = &a.html {
        write_html(
            &findings,
            &items,
            candidate_count,
            a.use_naive,
            &a,
            start.elapsed().as_secs_f32(),
            html_path,
        )?;
        eprintln!("HTML report: {}", html_path.display());
    }

    if !a.quiet {
        eprintln!("Total wall clock: {:.2}s", start.elapsed().as_secs_f32());
    }
    Ok(())
}

fn run_lsh_pipeline(
    items: &[Item],
    min_jaccard: f32,
    scan_root: &Path,
) -> (Vec<Finding>, String, usize) {
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
    let summary = format!(
        "lsh: {} entries, {} band-buckets, {} candidate pairs",
        idx.file_count(),
        idx.bucket_count(),
        cand_count,
    );
    let findings: Vec<Finding> = candidates
        .into_iter()
        .filter(|(_, _, est)| *est >= min_jaccard)
        .filter_map(|(a_id, b_id, est)| {
            let set_a = &items[a_id as usize].unique_set;
            let set_b = &items[b_id as usize].unique_set;
            let v = verify(a_id, b_id, est, set_a, set_b);
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
    (findings, summary, cand_count)
}

fn run_naive_pipeline(
    items: &[Item],
    min_shared: u32,
    max_bucket: usize,
    scan_root: &Path,
) -> (Vec<Finding>, String, usize) {
    let mut idx = Index::new();
    let mut total_fps = 0usize;
    for e in items {
        total_fps += e.fps.len();
        if let Some(g) = &e.granule {
            idx.add_granule(e.path.clone(), e.lang, g.clone(), &e.fps);
        } else {
            idx.add_file(e.path.clone(), e.lang, &e.fps);
        }
    }
    let pairs = idx.pair_report(min_shared, max_bucket);
    let cand_count = pairs.len();
    let summary = format!(
        "naive: {} entries, {} buckets, {} fingerprints",
        idx.file_count(),
        idx.bucket_count(),
        total_fps,
    );
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
    (findings, summary, cand_count)
}

#[allow(clippy::too_many_arguments)]
fn write_html(
    findings: &[Finding],
    items: &[Item],
    candidate_pairs: usize,
    use_naive: bool,
    a: &ScanArgs,
    elapsed_secs: f32,
    output: &Path,
) -> Result<()> {
    let summary = html::Summary {
        scan_dir: a.dir.display().to_string(),
        scanned_files: items
            .iter()
            .map(|i| i.path.as_path())
            .collect::<HashSet<_>>()
            .len(),
        entries: items.len(),
        candidate_pairs,
        findings: findings.len(),
        elapsed_secs,
        granularity: format!("{:?}", a.granularity),
        blind: format!("{:?}", a.blind),
        backend: if use_naive { "naive".into() } else { "lsh".into() },
    };
    html::render(findings, &summary, output)?;
    Ok(())
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

fn format_item_ref(item: &ItemRef) -> String {
    match &item.granule {
        None => item.path.display().to_string(),
        Some(g) => {
            let name = g.fn_name.as_deref().unwrap_or("<anonymous>");
            format!(
                "{}:{}-{}::{}",
                item.path.display(),
                g.line_start,
                g.line_end,
                name
            )
        }
    }
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
        Granularity::File => build_item(minhasher, path.to_owned(), lang_key, None, &out.tokens, k, window)
            .into_iter()
            .collect(),
        Granularity::Function => {
            if !Slicer::supports(lang_key) {
                return Vec::new();
            }
            let granules = slicer.slice(lang_key, path.to_owned(), content.as_bytes());
            granules
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
    // Tokens are emitted in order, so `partition_point` over `byte_start` works.
    let lo = tokens.partition_point(|t| t.byte_start < start);
    let hi = tokens.partition_point(|t| t.byte_start < end);
    &tokens[lo..hi]
}

