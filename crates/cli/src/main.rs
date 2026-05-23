//! `dupe` — duplicate code finder.
//!
//! Milestone 1 surface: `dupe scan <dir>`. Walks a directory, normalizes each file by
//! language (via vendored tokei definitions), winnows fingerprints, and reports the
//! highest-overlap file pairs.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokei_dedup_core::BlindMode;
use tokei_dedup_fingerprinter::{fingerprint_tokens, Fingerprint};
use tokei_dedup_index::Index;
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

        /// Minimum distinct fingerprints two files must share to be reported.
        #[arg(long, default_value_t = 10)]
        min_shared: u32,

        /// Drop fingerprint buckets larger than this (boilerplate suppression).
        #[arg(long, default_value_t = 50)]
        max_bucket: usize,

        /// Number of pairs to print.
        #[arg(long, default_value_t = 20)]
        top: usize,

        /// Token blinding aggressiveness.
        #[arg(long, value_enum, default_value_t = Blind::Mild)]
        blind: Blind,

        /// Restrict to a single language (tokei key, e.g. `Rust`, `Python`).
        #[arg(long)]
        only_lang: Option<String>,

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
            min_shared,
            max_bucket,
            top,
            blind,
            only_lang,
            quiet,
        } => scan(&dir, k, window, min_shared, max_bucket, top, blind.into(), only_lang.as_deref(), quiet),
    }
}

struct PerFile {
    path: PathBuf,
    lang: &'static str,
    fps: Vec<Fingerprint>,
}

#[allow(clippy::too_many_arguments)]
fn scan(
    dir: &Path,
    k: usize,
    window: usize,
    min_shared: u32,
    max_bucket: usize,
    top: usize,
    blind: BlindMode,
    only_lang: Option<&str>,
    quiet: bool,
) -> Result<()> {
    let normalizer = Normalizer::new(blind);
    let start = Instant::now();

    // Collect file paths first so we can parallelize cleanly.
    let paths: Vec<PathBuf> = WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();

    if !quiet {
        eprintln!("Scanning {} files under {}", paths.len(), dir.display());
    }

    // Normalize + fingerprint in parallel.
    let entries: Vec<PerFile> = paths
        .par_iter()
        .filter_map(|p| process_file(&normalizer, p, k, window, only_lang))
        .collect();

    let normalize_elapsed = start.elapsed();
    if !quiet {
        eprintln!(
            "Normalized {} files in {:.2}s",
            entries.len(),
            normalize_elapsed.as_secs_f32()
        );
    }

    // Build the index serially — fingerprint hashmap ops aren't worth parallelizing
    // until corpora are 100x larger.
    let mut idx = Index::new();
    let mut total_fps = 0usize;
    for e in entries {
        total_fps += e.fps.len();
        idx.add_file(e.path, e.lang, &e.fps);
    }

    if !quiet {
        eprintln!(
            "Index: {} files, {} distinct fingerprint buckets ({} fingerprints total)",
            idx.file_count(),
            idx.bucket_count(),
            total_fps,
        );
    }

    let mut pairs = idx.pair_report(min_shared, max_bucket);
    pairs.sort_by(|a, b| {
        b.jaccard
            .partial_cmp(&a.jaccard)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.shared.cmp(&a.shared))
    });

    if pairs.is_empty() {
        println!("No candidate duplicates above min_shared={min_shared}.");
        return Ok(());
    }

    let total = pairs.len();
    let shown = top.min(total);
    println!(
        "Top {} candidate pair(s) of {} (jaccard ↓, params: k={}, w={}, blind={:?}):",
        shown, total, k, window, blind,
    );
    for (rank, p) in pairs.iter().take(top).enumerate() {
        let lang_tag = if p.lang_a == p.lang_b {
            p.lang_a.clone()
        } else {
            format!("{}↔{}", p.lang_a, p.lang_b)
        };
        println!(
            "  {:>3}. j={:.3} shared={:>4} ({}/{}/{}) [{}]",
            rank + 1,
            p.jaccard,
            p.shared,
            p.a_total,
            p.b_total,
            (p.a_total + p.b_total).saturating_sub(p.shared),
            lang_tag,
        );
        println!("       a: {}", p.file_a.display());
        println!("       b: {}", p.file_b.display());
    }

    if !quiet {
        eprintln!("Total wall clock: {:.2}s", start.elapsed().as_secs_f32());
    }
    Ok(())
}

fn process_file(
    normalizer: &Normalizer,
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
    Some(PerFile {
        path: path.to_owned(),
        lang: lang_key,
        fps,
    })
}
