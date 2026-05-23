//! `dupe` — duplicate code finder.
//!
//! Milestone 1 surface: `dupe scan <dir>`. Walks a directory, normalizes each file by
//! language (via vendored tokei definitions), winnows fingerprints, and reports the
//! highest-overlap file pairs.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokei_dedup_core::{BlindMode, NormalizedToken};
use tokei_dedup_fingerprinter::{
    fingerprint_tokens, Fingerprint, MinHasher, Sketch, DEFAULT_MINHASH_SEED,
};
use tokei_dedup_index::{GranuleInfo, Index, LshIndex};
use tokei_dedup_lang_config as lang_config;
use tokei_dedup_normalizer::Normalizer;
use tokei_dedup_slicer::Slicer;
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
}

struct Item {
    path: PathBuf,
    lang: &'static str,
    granule: Option<GranuleInfo>,
    fps: Vec<Fingerprint>,
    sketch: Sketch,
    unique_fps: u32,
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

    let (mut pairs, backend_summary) = if a.use_naive {
        let mut idx = Index::new();
        let mut total_fps = 0usize;
        for e in &items {
            total_fps += e.fps.len();
            if let Some(g) = &e.granule {
                idx.add_granule(e.path.clone(), e.lang, g.clone(), &e.fps);
            } else {
                idx.add_file(e.path.clone(), e.lang, &e.fps);
            }
        }
        let summary = format!(
            "naive: {} entries, {} buckets, {} fingerprints",
            idx.file_count(),
            idx.bucket_count(),
            total_fps,
        );
        (idx.pair_report(a.min_shared, a.max_bucket), summary)
    } else {
        let mut idx = LshIndex::with_defaults();
        for e in &items {
            if let Some(g) = &e.granule {
                idx.add_granule(e.path.clone(), e.lang, g.clone(), e.sketch, e.unique_fps);
            } else {
                idx.add_file(e.path.clone(), e.lang, e.sketch, e.unique_fps);
            }
        }
        let cand = idx.candidate_pair_count();
        let summary = format!(
            "lsh: {} entries, {} band-buckets, {} candidate pairs",
            idx.file_count(),
            idx.bucket_count(),
            cand,
        );
        (idx.pair_report(a.min_jaccard), summary)
    };

    // Same-file granule pairs (e.g. a function compared to itself when nesting causes
    // the parent function to be its own granule) are noise — filter them out.
    if a.granularity == Granularity::Function {
        pairs.retain(|p| !(p.file_a == p.file_b && p.granule_a == p.granule_b));
    }

    if !a.quiet {
        eprintln!("Index: {backend_summary}");
    }

    pairs.sort_by(|x, y| {
        y.jaccard
            .partial_cmp(&x.jaccard)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.shared.cmp(&x.shared))
    });

    if pairs.is_empty() {
        println!("No candidate duplicates.");
        return Ok(());
    }

    let total = pairs.len();
    let shown = a.top.min(total);
    let approx = if a.use_naive { "" } else { " (estimated)" };
    println!(
        "Top {} candidate pair(s) of {} (jaccard ↓{}, granularity={:?}, params: k={}, w={}, blind={:?}):",
        shown, total, approx, a.granularity, a.k, a.window, a.blind,
    );
    for (rank, p) in pairs.iter().take(a.top).enumerate() {
        let lang_tag = if p.lang_a == p.lang_b {
            p.lang_a.clone()
        } else {
            format!("{}↔{}", p.lang_a, p.lang_b)
        };
        println!(
            "  {:>3}. j={:.3} shared={:>4} ({}/{}) [{}]",
            rank + 1,
            p.jaccard,
            p.shared,
            p.a_total,
            p.b_total,
            lang_tag,
        );
        println!("       a: {}", format_endpoint(&p.file_a, p.granule_a.as_ref()));
        println!("       b: {}", format_endpoint(&p.file_b, p.granule_b.as_ref()));
    }

    if !a.quiet {
        eprintln!("Total wall clock: {:.2}s", start.elapsed().as_secs_f32());
    }
    Ok(())
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
    let unique: HashSet<u64> = fps.iter().map(|f| f.hash).collect();
    let unique_count = unique.len() as u32;
    let unique_vec: Vec<u64> = unique.into_iter().collect();
    let sketch = minhasher.sketch(&unique_vec);
    Some(Item {
        path,
        lang,
        granule,
        fps,
        sketch,
        unique_fps: unique_count,
    })
}

fn tokens_in_byte_range(tokens: &[NormalizedToken], start: u32, end: u32) -> &[NormalizedToken] {
    // Tokens are emitted in order, so `partition_point` over `byte_start` works.
    let lo = tokens.partition_point(|t| t.byte_start < start);
    let hi = tokens.partition_point(|t| t.byte_start < end);
    &tokens[lo..hi]
}

fn format_endpoint(path: &Path, granule: Option<&GranuleInfo>) -> String {
    match granule {
        None => path.display().to_string(),
        Some(g) => {
            let name = g.fn_name.as_deref().unwrap_or("<anonymous>");
            format!(
                "{}:{}-{}::{}",
                path.display(),
                g.line_start,
                g.line_end,
                name
            )
        }
    }
}
