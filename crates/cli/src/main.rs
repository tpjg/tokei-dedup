//! `dupe` — duplicate code finder.
//!
//! Thin CLI on top of [`tokei_dedup_engine`]. Parses arguments, drives the scan, then
//! emits either a human-readable terminal report, an HTML report, or JSON.

mod html;
mod json_out;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use tokei_dedup_classifier::ItemRef;
use tokei_dedup_core::BlindMode;
use tokei_dedup_engine::{scan, Granularity as EngineGranularity, ScanOptions, ScanResult, WalkOptions};

#[derive(Parser)]
#[command(name = "dupe", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory and report candidate duplicate pairs.
    Scan {
        dir: PathBuf,

        #[arg(long, default_value_t = tokei_dedup_fingerprinter::DEFAULT_K)]
        k: usize,

        #[arg(long, default_value_t = tokei_dedup_fingerprinter::DEFAULT_WINDOW)]
        window: usize,

        #[arg(long, default_value_t = 20)]
        top: usize,

        #[arg(long, value_enum, default_value_t = Blind::Mild)]
        blind: Blind,

        #[arg(long)]
        only_lang: Option<String>,

        #[arg(long, value_enum, default_value_t = Granularity::File)]
        granularity: Granularity,

        #[arg(long, conflicts_with = "min_jaccard")]
        use_naive: bool,

        #[arg(long, default_value_t = 0.5)]
        min_jaccard: f32,

        #[arg(long, default_value_t = 10)]
        min_shared: u32,

        #[arg(long, default_value_t = 50)]
        max_bucket: usize,

        #[arg(long, short)]
        quiet: bool,

        /// Write a standalone HTML report to this path (in addition to terminal output).
        #[arg(long)]
        html: Option<PathBuf>,

        /// Emit JSON to stdout instead of the human-readable summary. Useful for
        /// piping into other tools and for agent workflows.
        #[arg(long)]
        json: bool,

        /// Extra gitignore-style pattern to exclude (repeatable). Examples:
        /// `--exclude target --exclude '**/test_data/**'`.
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,

        /// Don't honor `.gitignore` / `.ignore` files or the hidden-file filter.
        #[arg(long)]
        no_gitignore: bool,

        /// Don't apply the built-in DEFAULT_EXCLUDES list (target, node_modules,
        /// dist, build, .venv, …). Use `--no-default-excludes --no-gitignore` to
        /// scan everything except what you pass to `--exclude`.
        #[arg(long)]
        no_default_excludes: bool,
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

impl From<Granularity> for EngineGranularity {
    fn from(g: Granularity) -> Self {
        match g {
            Granularity::File => EngineGranularity::File,
            Granularity::Function => EngineGranularity::Function,
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
            granularity,
            use_naive,
            min_jaccard,
            min_shared,
            max_bucket,
            quiet,
            html,
            json,
            exclude,
            no_gitignore,
            no_default_excludes,
        } => {
            let opts = ScanOptions {
                blind: blind.into(),
                granularity: granularity.into(),
                k,
                window,
                use_naive,
                min_jaccard,
                min_shared,
                max_bucket,
                only_lang,
                walk: WalkOptions {
                    respect_gitignore: !no_gitignore,
                    apply_default_excludes: !no_default_excludes,
                    custom_excludes: exclude,
                },
            };
            let result = scan(&dir, &opts);

            if !quiet && !json {
                eprintln!(
                    "Scanned {} files, indexed {} entries; {} candidate pairs ({}); {} findings in {:.2}s",
                    result.files_walked,
                    result.entries_indexed,
                    result.candidate_pairs,
                    result.backend,
                    result.findings.len(),
                    result.elapsed_secs,
                );
            }

            if json {
                json_out::print(&result, &dir);
            } else {
                print_text(&result, top, &granularity);
            }

            if let Some(path) = html.as_deref() {
                let summary = html::Summary {
                    scan_dir: dir.display().to_string(),
                    scanned_files: result.files_walked,
                    entries: result.entries_indexed,
                    candidate_pairs: result.candidate_pairs,
                    findings: result.findings.len(),
                    elapsed_secs: result.elapsed_secs,
                    granularity: format!("{granularity:?}"),
                    blind: format!("{:?}", opts.blind),
                    backend: result.backend.into(),
                };
                html::render(&result.findings, &summary, path)?;
                if !quiet && !json {
                    eprintln!("HTML report: {}", path.display());
                }
            }
            Ok(())
        }
    }
}

fn print_text(result: &ScanResult, top: usize, granularity: &Granularity) {
    if result.findings.is_empty() {
        println!("No candidate duplicates.");
        return;
    }
    let total = result.findings.len();
    let shown = top.min(total);
    println!(
        "Top {} finding(s) of {} (score ↓, granularity={:?}, backend={}):",
        shown, total, granularity, result.backend,
    );
    for (rank, f) in result.findings.iter().take(top).enumerate() {
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
}

pub(crate) fn format_item_ref(item: &ItemRef) -> String {
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

