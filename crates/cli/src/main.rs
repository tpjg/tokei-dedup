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
use tokei_dedup_core::BlindMode;
use tokei_dedup_fingerprinter::{
    fingerprint_tokens, Fingerprint, MinHasher, Sketch, DEFAULT_MINHASH_SEED,
};
use tokei_dedup_index::{Index, LshIndex};
use tokei_dedup_lang_config as lang_config;
use tokei_dedup_normalizer::Normalizer;
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
    use_naive: bool,
    min_jaccard: f32,
    min_shared: u32,
    max_bucket: usize,
    quiet: bool,
}

struct PerFile {
    path: PathBuf,
    lang: &'static str,
    fps: Vec<Fingerprint>,
    sketch: Sketch,
    unique_fps: u32,
}

fn scan(a: ScanArgs) -> Result<()> {
    let normalizer = Normalizer::new(a.blind);
    let minhasher = MinHasher::new(DEFAULT_MINHASH_SEED);
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

    let entries: Vec<PerFile> = paths
        .par_iter()
        .filter_map(|p| {
            process_file(
                &normalizer,
                &minhasher,
                p,
                a.k,
                a.window,
                a.only_lang.as_deref(),
            )
        })
        .collect();

    let normalize_elapsed = start.elapsed();
    if !a.quiet {
        eprintln!(
            "Normalized + fingerprinted {} files in {:.2}s",
            entries.len(),
            normalize_elapsed.as_secs_f32(),
        );
    }

    let (mut pairs, backend_summary) = if a.use_naive {
        let mut idx = Index::new();
        let mut total_fps = 0usize;
        for e in &entries {
            total_fps += e.fps.len();
            idx.add_file(e.path.clone(), e.lang, &e.fps);
        }
        let summary = format!(
            "naive: {} files, {} buckets, {} fingerprints",
            idx.file_count(),
            idx.bucket_count(),
            total_fps,
        );
        (idx.pair_report(a.min_shared, a.max_bucket), summary)
    } else {
        let mut idx = LshIndex::with_defaults();
        for e in &entries {
            idx.add_file(e.path.clone(), e.lang, e.sketch, e.unique_fps);
        }
        let cand = idx.candidate_pair_count();
        let summary = format!(
            "lsh: {} files, {} band-buckets, {} candidate pairs",
            idx.file_count(),
            idx.bucket_count(),
            cand,
        );
        (idx.pair_report(a.min_jaccard), summary)
    };

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
        "Top {} candidate pair(s) of {} (jaccard ↓{}, params: k={}, w={}, blind={:?}):",
        shown, total, approx, a.k, a.window, a.blind,
    );
    let _seen: HashSet<()> = HashSet::new();
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
        println!("       a: {}", p.file_a.display());
        println!("       b: {}", p.file_b.display());
    }

    if !a.quiet {
        eprintln!("Total wall clock: {:.2}s", start.elapsed().as_secs_f32());
    }
    Ok(())
}

fn process_file(
    normalizer: &Normalizer,
    minhasher: &MinHasher,
    path: &Path,
    k: usize,
    window: usize,
    only_lang: Option<&str>,
) -> Option<PerFile> {
    let ext = path.extension()?.to_str()?;
    let (lang_key, _def) = lang_config::by_extension(ext)?;
    if let Some(want) = only_lang {
        if !lang_key.eq_ignore_ascii_case(want) {
            return None;
        }
    }
    let content = std::fs::read_to_string(path).ok()?;
    let out = normalizer.process(&content, lang_key);
    if out.tokens.is_empty() {
        return None;
    }
    let fps = fingerprint_tokens(&out.tokens, k, window);
    if fps.is_empty() {
        return None;
    }
    let unique: std::collections::HashSet<u64> = fps.iter().map(|f| f.hash).collect();
    let unique_count = unique.len() as u32;
    let unique_vec: Vec<u64> = unique.into_iter().collect();
    let sketch = minhasher.sketch(&unique_vec);
    Some(PerFile {
        path: path.to_owned(),
        lang: lang_key,
        fps,
        sketch,
        unique_fps: unique_count,
    })
}
