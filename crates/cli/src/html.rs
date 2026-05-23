//! Static HTML report renderer.
//!
//! Self-contained single file with embedded CSS. For each finding renders side-by-side
//! source snippets read from disk at report time — if a file moved/changed since the
//! scan, the snippet shows "[source unavailable]" rather than crashing.
//!
//! Two perf-shaped invariants:
//!
//! - **Per-render source cache.** A `HashMap<PathBuf, Option<String>>` lives for the
//!   duration of one `render()` call. Each file is read at most once even when it shows
//!   up in 20 different findings. On Linux-kernel-scale corpora this drops the HTML
//!   phase from a minute to a few seconds.
//! - **Bounded output.** `render()` honors a `max_findings` cap — the caller decides how
//!   many findings to commit to the HTML file. Callers using `--top N` for the terminal
//!   report should pass the same `N` here.

use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use tokei_dedup_classifier::{Finding, ItemRef};

pub struct Summary {
    pub scan_dir: String,
    pub scanned_files: usize,
    pub entries: usize,
    pub candidate_pairs: usize,
    pub findings: usize,
    pub elapsed_secs: f32,
    pub granularity: String,
    pub blind: String,
    pub backend: String,
}

/// In-flight source cache. `None` means we tried to read the file and failed (deleted
/// since the scan, encoding error, …); the renderer falls back to "[source unavailable]".
type SnippetCache = HashMap<PathBuf, Option<String>>;

pub fn render(
    findings: &[Finding],
    summary: &Summary,
    output: &Path,
    max_findings: usize,
) -> std::io::Result<()> {
    let to_render = &findings[..max_findings.min(findings.len())];

    let mut html = String::with_capacity(64 * 1024);
    html.push_str(HEAD_OPEN);
    html.push_str(STYLE);
    html.push_str(HEAD_CLOSE);
    html.push_str(&render_summary(summary, to_render.len()));
    if to_render.is_empty() {
        html.push_str(r#"<p class="empty">No findings above threshold.</p>"#);
    }
    let mut cache: SnippetCache = HashMap::new();
    for (rank, f) in to_render.iter().enumerate() {
        write!(&mut html, "{}", render_finding(rank + 1, f, &mut cache)).ok();
    }
    html.push_str(FOOT);
    fs::write(output, html)
}

fn render_summary(s: &Summary, displayed: usize) -> String {
    let truncated_note = if displayed < s.findings {
        format!(
            r#"<dt>Showing</dt><dd>top {displayed} of {total} (use --top to widen)</dd>"#,
            displayed = displayed,
            total = s.findings,
        )
    } else {
        format!(
            r#"<dt>Showing</dt><dd>all {displayed}</dd>"#,
            displayed = displayed,
        )
    };

    format!(
        r#"<h1>tokei-dedup report</h1>
<dl class="summary">
  <dt>Scan root</dt><dd><code>{root}</code></dd>
  <dt>Granularity</dt><dd>{gran}</dd>
  <dt>Backend</dt><dd>{backend}</dd>
  <dt>Blind mode</dt><dd>{blind}</dd>
  <dt>Files scanned</dt><dd>{files}</dd>
  <dt>Entries indexed</dt><dd>{entries}</dd>
  <dt>Candidate pairs</dt><dd>{cand}</dd>
  <dt>Findings (post-classify)</dt><dd>{findings}</dd>
  {truncated_note}
  <dt>Elapsed</dt><dd>{secs:.2}s</dd>
</dl>
"#,
        root = escape(&s.scan_dir),
        gran = escape(&s.granularity),
        backend = escape(&s.backend),
        blind = escape(&s.blind),
        files = s.scanned_files,
        entries = s.entries,
        cand = s.candidate_pairs,
        findings = s.findings,
        truncated_note = truncated_note,
        secs = s.elapsed_secs,
    )
}

fn render_finding(rank: usize, f: &Finding, cache: &mut SnippetCache) -> String {
    let tags_html: String = f
        .tags
        .iter()
        .map(|t| {
            format!(
                r#"<span class="tag tag-{kind}">{label}</span>"#,
                kind = t.as_str(),
                label = t.as_str(),
            )
        })
        .collect();

    format!(
        r##"<section class="finding" id="finding-{rank}">
  <header>
    <h2><a href="#finding-{rank}">#{rank}</a> · score {score:.3} · jaccard {j:.3}
        <span class="meta">({shared} shared / {a_total} ‖ {b_total} unique fps)</span></h2>
    <div class="tags">{tags}</div>
  </header>
  <div class="panes">
    {pane_a}
    {pane_b}
  </div>
</section>
"##,
        rank = rank,
        score = f.score,
        j = f.exact_jaccard,
        shared = f.shared,
        a_total = f.a.unique_fps,
        b_total = f.b.unique_fps,
        tags = tags_html,
        pane_a = render_pane(&f.a, cache),
        pane_b = render_pane(&f.b, cache),
    )
}

fn render_pane(item: &ItemRef, cache: &mut SnippetCache) -> String {
    let header = match &item.granule {
        Some(g) => {
            let name = g.fn_name.as_deref().unwrap_or("<anonymous>");
            format!(
                "{path}:{ls}-{le}::{name}",
                path = item.path.display(),
                ls = g.line_start,
                le = g.line_end,
                name = name
            )
        }
        None => item.path.display().to_string(),
    };
    let snippet = match &item.granule {
        Some(g) => extract_lines(&item.path, g.line_start, g.line_end, cache),
        None => extract_lines(&item.path, 1, 30, cache),
    };
    format!(
        r#"<div class="pane">
      <div class="pane-head"><code>{header}</code> <span class="lang">[{lang}]</span></div>
      <pre><code>{snippet}</code></pre>
    </div>"#,
        header = escape(&header),
        lang = escape(&item.lang),
        snippet = render_numbered_snippet(&snippet, item.granule.as_ref().map(|g| g.line_start)),
    )
}

fn render_numbered_snippet(text: &str, first_line: Option<u32>) -> String {
    let mut out = String::with_capacity(text.len() * 5 / 4);
    for (idx, line) in text.lines().enumerate() {
        let n = first_line.unwrap_or(1) + idx as u32;
        write!(out, r#"<span class="ln">{n:>4}</span>  {}{}"#, escape(line), '\n').ok();
    }
    out
}

fn extract_lines(
    path: &Path,
    line_start: u32,
    line_end: u32,
    cache: &mut SnippetCache,
) -> String {
    // `entry().or_insert_with` reads each file at most once per `render()` call. None
    // means we tried and failed; subsequent lookups skip the I/O and return immediately.
    let entry = cache
        .entry(path.to_path_buf())
        .or_insert_with(|| fs::read_to_string(path).ok());
    let Some(content) = entry.as_deref() else {
        return "[source unavailable]".into();
    };
    let lines: Vec<&str> = content.lines().collect();
    let ls = (line_start.saturating_sub(1) as usize).min(lines.len());
    let le = (line_end as usize).min(lines.len());
    if ls >= le {
        return String::new();
    }
    lines[ls..le].join("\n")
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

const HEAD_OPEN: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>tokei-dedup report</title>
<style>"#;

const STYLE: &str = r#"
:root {
  --fg: #1a1a1a;
  --muted: #666;
  --bg: #fafafa;
  --pane-bg: #fff;
  --border: #e0e0e0;
  --accent: #0366d6;
}
* { box-sizing: border-box; }
body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  margin: 0 auto;
  max-width: 1400px;
  padding: 1.5em;
  color: var(--fg);
  background: var(--bg);
}
h1 { margin-top: 0; }
.summary {
  display: grid;
  grid-template-columns: max-content 1fr;
  column-gap: 1em;
  row-gap: 0.25em;
  background: var(--pane-bg);
  border: 1px solid var(--border);
  padding: 1em;
  border-radius: 6px;
}
.summary dt { color: var(--muted); }
.summary dd { margin: 0; }
.empty { color: var(--muted); font-style: italic; }
.finding {
  background: var(--pane-bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  margin: 1.5em 0;
  padding: 1em;
}
.finding h2 {
  margin: 0;
  font-size: 1.1em;
  font-weight: 600;
}
.finding h2 a {
  color: var(--accent);
  text-decoration: none;
}
.finding h2 .meta {
  color: var(--muted);
  font-weight: normal;
  font-size: 0.9em;
}
.tags { margin: 0.5em 0; }
.tag {
  display: inline-block;
  padding: 0.1em 0.6em;
  margin-right: 0.4em;
  border-radius: 10px;
  font-size: 0.8em;
  font-weight: 500;
  background: #eee;
}
.tag-test-only    { background: #fff3cd; color: #856404; }
.tag-cross-module { background: #d4edda; color: #155724; }
.tag-tiny         { background: #fce4ec; color: #880e4f; }
.tag-generic-name { background: #e3e7fc; color: #1a237e; }
.tag-subset       { background: #f8d7da; color: #721c24; }
.panes {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 1em;
  margin-top: 0.5em;
}
.pane {
  min-width: 0;
}
.pane-head {
  font-size: 0.85em;
  margin-bottom: 0.25em;
  word-break: break-all;
}
.pane-head .lang {
  color: var(--muted);
  margin-left: 0.5em;
}
pre {
  background: #f6f8fa;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 0.5em;
  margin: 0;
  overflow-x: auto;
  font-size: 0.85em;
  line-height: 1.4;
  max-height: 600px;
}
.ln {
  display: inline-block;
  width: 3em;
  color: #999;
  user-select: none;
  text-align: right;
  padding-right: 0.5em;
  border-right: 1px solid var(--border);
  margin-right: 0.5em;
}
@media (max-width: 900px) {
  .panes { grid-template-columns: 1fr; }
}
"#;

const HEAD_CLOSE: &str = r#"</style>
</head>
<body>
"#;

const FOOT: &str = r#"
<footer style="margin-top: 3em; color: var(--muted); font-size: 0.85em;">
  Generated by <a href="https://github.com/tpjg/tokei-dedup">tokei-dedup</a>.
</footer>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_handles_special_chars() {
        assert_eq!(escape("a < b & c > d \"e\""), "a &lt; b &amp; c &gt; d &quot;e&quot;");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn render_summary_includes_all_fields() {
        let s = Summary {
            scan_dir: "/tmp/x".into(),
            scanned_files: 100,
            entries: 200,
            candidate_pairs: 50,
            findings: 5,
            elapsed_secs: 1.23,
            granularity: "Function".into(),
            blind: "Aggressive".into(),
            backend: "lsh".into(),
        };
        let s_html = render_summary(&s, 5);
        assert!(s_html.contains("/tmp/x"));
        assert!(s_html.contains("Function"));
        assert!(s_html.contains("Aggressive"));
        assert!(s_html.contains("1.23"));
        assert!(s_html.contains("100"));
        assert!(s_html.contains("all 5"));
    }

    #[test]
    fn render_summary_shows_truncation_note() {
        let s = Summary {
            scan_dir: "/x".into(),
            scanned_files: 100,
            entries: 200,
            candidate_pairs: 50,
            findings: 1000,
            elapsed_secs: 1.0,
            granularity: "Function".into(),
            blind: "Mild".into(),
            backend: "lsh".into(),
        };
        let s_html = render_summary(&s, 50);
        assert!(
            s_html.contains("top 50 of 1000"),
            "expected truncation note, got:\n{s_html}"
        );
    }
}
