//! `dupe-lsp` — LSP server that publishes clone diagnostics for the current workspace.
//!
//! Lifecycle:
//!   1. `initialize`: read `initializationOptions`, lock in scan + publish config.
//!   2. `initialized`: scan once, eagerly publish diagnostics for every affected
//!      URI, then spawn a background rescan loop that listens for save events.
//!   3. `didOpen`: re-emit cached diagnostics for the opened file (covers files
//!      that became visible after the initial publish).
//!   4. `didSave`: if `rescanOnSave` is on, kick the rescan loop; otherwise
//!      re-publish from cache.
//!
//! The rescan loop debounces 500 ms after the latest save and coalesces save
//! storms into a single workspace rescan. Inline `didChange` events are
//! ignored — true incremental re-fingerprinting needs LSH-entry removal,
//! which is milestone-6 work (see `DESIGN.md`).
//!
//! Diagnostic severity is tiered: the top N findings by score get a visible
//! severity (default `INFORMATION`, surfaces in editor "Problems" panels);
//! the long tail gets `HINT` (faint inline only). Both knobs are config.
//!
//! The server runs over stdio (LSP convention): editor pipes `--stdio`, the
//! server reads JSON-RPC frames from stdin and writes to stdout.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokei_dedup_classifier::Finding;
use tokei_dedup_engine::{scan, BlindModeExt, Granularity, ScanOptions, WalkOptions};
use tokio::sync::{Mutex, Notify};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Debounce window for `rescanOnSave`: a save event waits this long before
/// kicking a rescan, and subsequent saves within the window coalesce into
/// the same rescan. 500ms is a common "after-save linting" value; long
/// enough to absorb formatter-on-save save storms, short enough that
/// users don't notice the delay.
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(500);

/// Editor-supplied configuration received in the LSP `initialize` request's
/// `initializationOptions`. All fields optional; missing keys keep the
/// intentionally strict defaults (see [`default_scan_opts`] and
/// [`default_publish_opts`]). Unknown keys are tolerated for
/// forward-compatibility; wrong values produce a `WARNING` log.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitOptions {
    granularity: Option<String>,
    blind: Option<String>,
    min_jaccard: Option<f32>,
    exclude: Option<Vec<String>>,
    highlight_top: Option<usize>,
    highlight_severity: Option<String>,
    tail_severity: Option<String>,
    rescan_on_save: Option<bool>,
}

/// Resolved diagnostic-publishing policy.
#[derive(Clone, Debug)]
struct PublishOptions {
    /// Findings with score-rank `< highlight_top` get [`Self::highlight_severity`].
    /// Set to 0 to demote everything to the tail.
    highlight_top: usize,
    highlight_severity: DiagnosticSeverity,
    /// `None` means findings outside the top are not published at all
    /// (`tailSeverity: "off"`).
    tail_severity: Option<DiagnosticSeverity>,
    /// Whether `didSave` triggers a full workspace rescan.
    rescan_on_save: bool,
}

fn default_publish_opts() -> PublishOptions {
    PublishOptions {
        highlight_top: 20,
        // WARNING (not INFORMATION) because Zed — and several other
        // editors — hide INFORMATION-level diagnostics from the project
        // panel by default. "Possible clone" is arguably warning-grade
        // attention anyway. Downgrade via `highlightSeverity` if you want
        // it quieter.
        highlight_severity: DiagnosticSeverity::WARNING,
        tail_severity: Some(DiagnosticSeverity::HINT),
        rescan_on_save: true,
    }
}

/// A finding tagged with the severity its rank earned. Both endpoints of a
/// pair share the same severity (rank is per-pair, not per-endpoint).
#[derive(Clone, Debug)]
struct RankedFinding {
    finding: Finding,
    severity: DiagnosticSeverity,
}

struct DupeServer {
    client: Client,
    state: Arc<Mutex<State>>,
    /// Set by `did_save`; cleared at the start of each rescan. Acts as the
    /// "we have unflushed save events" flag for the rescan loop.
    rescan_dirty: Arc<AtomicBool>,
    /// Wakes the rescan loop. `did_save` calls `notify_one`; the loop
    /// `notified().await`s.
    rescan_notify: Arc<Notify>,
    /// Set once when `initialized` spawns the rescan loop so the spawn
    /// itself is idempotent.
    rescan_loop_started: AtomicBool,
}

#[derive(Default)]
struct State {
    workspace_root: Option<PathBuf>,
    scan_opts: ScanOptions,
    publish_opts: Option<PublishOptions>,
    /// Map from absolute file path → score-ranked findings touching that file.
    by_file: HashMap<PathBuf, Vec<RankedFinding>>,
    /// URIs we've published a non-empty diagnostic list for. On rescan we
    /// publish empty lists for any that drop out, so removed clones don't
    /// leave stale diagnostics in the editor.
    published_uris: HashSet<Url>,
    scanned: bool,
}

#[tower_lsp::async_trait]
impl LanguageServer for DupeServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root = workspace_root(&params);
        let (scan_opts, publish_opts, warnings) =
            resolve_init_opts(params.initialization_options.as_ref());
        for w in &warnings {
            self.client.log_message(MessageType::WARNING, w).await;
        }
        {
            let mut state = self.state.lock().await;
            state.workspace_root = root;
            state.scan_opts = scan_opts;
            state.publish_opts = Some(publish_opts);
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::NONE,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "dupe-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        run_scan_and_publish(&self.client, &self.state).await;
        self.spawn_rescan_loop();
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.publish_for_uri(&params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let rescan_on_save = {
            let state = self.state.lock().await;
            state
                .publish_opts
                .as_ref()
                .map(|p| p.rescan_on_save)
                .unwrap_or(true)
        };
        if rescan_on_save {
            // Mark the workspace dirty and kick the loop; the loop debounces
            // and runs the actual scan. The save event itself returns
            // immediately.
            self.rescan_dirty.store(true, Ordering::Relaxed);
            self.rescan_notify.notify_one();
        } else {
            // No rescan — just refresh cached diagnostics for the saved file.
            self.publish_for_uri(&params.text_document.uri).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl DupeServer {
    /// Re-publish cached diagnostics for a single file. Used on `didOpen`
    /// and on `didSave` when `rescanOnSave` is off.
    async fn publish_for_uri(&self, uri: &Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let diagnostics = {
            let state = self.state.lock().await;
            state
                .by_file
                .get(&path)
                .map(|findings| {
                    findings
                        .iter()
                        .filter_map(|rf| make_diagnostic(&rf.finding, rf.severity, &path))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    /// Idempotently spawn the background rescan loop. Driven by
    /// `rescan_notify` / `rescan_dirty`. The loop waits for the next
    /// notify, sleeps [`RESCAN_DEBOUNCE`] to coalesce save storms, then
    /// runs a scan if the dirty flag was set. Save events arriving
    /// mid-scan re-set the flag, so a follow-up scan runs once the
    /// current one finishes.
    fn spawn_rescan_loop(&self) {
        if self.rescan_loop_started.swap(true, Ordering::SeqCst) {
            return; // already running
        }
        let dirty = self.rescan_dirty.clone();
        let notify = self.rescan_notify.clone();
        let client = self.client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                tokio::time::sleep(RESCAN_DEBOUNCE).await;
                if dirty.swap(false, Ordering::Relaxed) {
                    run_scan_and_publish(&client, &state).await;
                }
            }
        });
    }
}

/// Run the scan with whatever options are currently in `state`, rank
/// findings, swap state, and publish diagnostics for every affected URI
/// (clearing any URIs whose findings disappeared since the last scan).
///
/// Free function (not a method) so the rescan task can call it directly
/// without keeping a `DupeServer` reference across `await` points.
async fn run_scan_and_publish(client: &Client, state: &Arc<Mutex<State>>) {
    let (root, scan_opts, pub_opts) = {
        let s = state.lock().await;
        (
            s.workspace_root.clone(),
            s.scan_opts.clone(),
            s.publish_opts.clone().unwrap_or_else(default_publish_opts),
        )
    };
    let Some(root) = root else {
        client
            .log_message(
                MessageType::WARNING,
                "dupe-lsp: no workspace root, skipping scan",
            )
            .await;
        return;
    };
    client
        .log_message(
            MessageType::INFO,
            format!(
                "dupe-lsp: scanning {} (granularity={:?}, blind={:?}, min_jaccard={:.2}, excludes={})",
                root.display(),
                scan_opts.granularity,
                scan_opts.blind,
                scan_opts.min_jaccard,
                scan_opts.walk.custom_excludes.len(),
            ),
        )
        .await;
    let root_clone = root.clone();
    let scan_opts_clone = scan_opts.clone();
    let scan_result =
        tokio::task::spawn_blocking(move || scan(&root_clone, &scan_opts_clone))
            .await
            .ok();
    let Some(result) = scan_result else {
        client
            .log_message(MessageType::ERROR, "dupe-lsp: scan task failed")
            .await;
        return;
    };
    let total = result.findings.len();
    let elapsed = result.elapsed_secs;
    let by_file = rank_and_group(result.findings, &pub_opts);

    let new_uris: HashSet<Url> = by_file
        .keys()
        .filter_map(|p| Url::from_file_path(p).ok())
        .collect();
    let to_clear: Vec<Url> = {
        let s = state.lock().await;
        s.published_uris.difference(&new_uris).cloned().collect()
    };
    let to_publish: Vec<(Url, Vec<Diagnostic>)> = by_file
        .iter()
        .filter_map(|(p, rfs)| {
            let uri = Url::from_file_path(p).ok()?;
            let diags = rfs
                .iter()
                .filter_map(|rf| make_diagnostic(&rf.finding, rf.severity, p))
                .collect();
            Some((uri, diags))
        })
        .collect();
    let touched_files = by_file.len();
    {
        let mut s = state.lock().await;
        s.by_file = by_file;
        s.published_uris = new_uris;
        s.scanned = true;
    }

    // Lock released before the publish loop: `publish_diagnostics` can be
    // long for big results and we don't want to block did_open / did_save
    // during it.
    for uri in to_clear {
        client.publish_diagnostics(uri, vec![], None).await;
    }
    for (uri, diags) in to_publish {
        client.publish_diagnostics(uri, diags, None).await;
    }

    let highlighted = std::cmp::min(total, pub_opts.highlight_top);
    client
        .log_message(
            MessageType::INFO,
            format!(
                "dupe-lsp: scan complete — {total} findings ({highlighted} highlighted) touching {touched_files} files in {elapsed:.2}s",
            ),
        )
        .await;
}

fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        if let Some(first) = folders.first() {
            if let Ok(p) = first.uri.to_file_path() {
                return Some(p);
            }
        }
    }
    #[allow(deprecated)]
    if let Some(uri) = &params.root_uri {
        if let Ok(p) = uri.to_file_path() {
            return Some(p);
        }
    }
    None
}

/// Intentionally strict baseline: function-granularity, aggressive blinding,
/// Jaccard floor 0.8. Users widen via config when they want recall over
/// precision.
fn default_scan_opts() -> ScanOptions {
    ScanOptions {
        blind: BlindModeExt::Aggressive,
        granularity: Granularity::Function,
        min_jaccard: 0.8,
        ..ScanOptions::default()
    }
}

/// Overlay editor-supplied `initializationOptions` onto the defaults.
/// Returns scan options, publish options, and any non-fatal warnings.
fn resolve_init_opts(
    raw: Option<&serde_json::Value>,
) -> (ScanOptions, PublishOptions, Vec<String>) {
    let mut scan_opts = default_scan_opts();
    let mut pub_opts = default_publish_opts();
    let mut warnings = Vec::new();
    let Some(value) = raw else {
        return (scan_opts, pub_opts, warnings);
    };
    let parsed: InitOptions = match serde_json::from_value(value.clone()) {
        Ok(p) => p,
        Err(e) => {
            warnings.push(format!(
                "dupe-lsp: ignoring malformed initializationOptions: {e}"
            ));
            return (scan_opts, pub_opts, warnings);
        }
    };
    if let Some(g) = parsed.granularity.as_deref() {
        match g {
            "file" => scan_opts.granularity = Granularity::File,
            "function" => scan_opts.granularity = Granularity::Function,
            other => warnings.push(format!(
                "dupe-lsp: unknown granularity {other:?} (expected 'file' or 'function'); keeping default"
            )),
        }
    }
    if let Some(b) = parsed.blind.as_deref() {
        match b {
            "strict" => scan_opts.blind = BlindModeExt::Strict,
            "mild" => scan_opts.blind = BlindModeExt::Mild,
            "aggressive" => scan_opts.blind = BlindModeExt::Aggressive,
            other => warnings.push(format!(
                "dupe-lsp: unknown blind {other:?} (expected 'strict', 'mild', or 'aggressive'); keeping default"
            )),
        }
    }
    if let Some(j) = parsed.min_jaccard {
        if (0.0..=1.0).contains(&j) {
            scan_opts.min_jaccard = j;
        } else {
            warnings.push(format!(
                "dupe-lsp: minJaccard={j} out of [0, 1]; keeping default"
            ));
        }
    }
    if let Some(ex) = parsed.exclude {
        scan_opts.walk = WalkOptions {
            custom_excludes: ex,
            ..scan_opts.walk
        };
    }
    if let Some(n) = parsed.highlight_top {
        pub_opts.highlight_top = n;
    }
    if let Some(s) = parsed.highlight_severity.as_deref() {
        match parse_severity(s, false) {
            SevParse::Sev(sev) => pub_opts.highlight_severity = sev,
            SevParse::Off => {
                warnings.push("dupe-lsp: highlightSeverity cannot be 'off'; keeping default".into())
            }
            SevParse::Unknown(msg) => {
                warnings.push(format!("dupe-lsp: highlightSeverity {msg}"))
            }
        }
    }
    if let Some(s) = parsed.tail_severity.as_deref() {
        match parse_severity(s, true) {
            SevParse::Sev(sev) => pub_opts.tail_severity = Some(sev),
            SevParse::Off => pub_opts.tail_severity = None,
            SevParse::Unknown(msg) => {
                warnings.push(format!("dupe-lsp: tailSeverity {msg}"))
            }
        }
    }
    if let Some(b) = parsed.rescan_on_save {
        pub_opts.rescan_on_save = b;
    }
    (scan_opts, pub_opts, warnings)
}

/// Outcome of parsing a severity string from config.
enum SevParse {
    Sev(DiagnosticSeverity),
    /// User wrote `"off"`. Only meaningful for `tailSeverity`; for
    /// `highlightSeverity` we reject it with a warning at the callsite.
    Off,
    /// Malformed input. The `String` is a complete user-facing message,
    /// minus the `dupe-lsp:` prefix and the option name (the caller adds
    /// those).
    Unknown(String),
}

fn parse_severity(s: &str, allow_off: bool) -> SevParse {
    match s {
        "hint" => SevParse::Sev(DiagnosticSeverity::HINT),
        "information" | "info" => SevParse::Sev(DiagnosticSeverity::INFORMATION),
        "warning" | "warn" => SevParse::Sev(DiagnosticSeverity::WARNING),
        "off" if allow_off => SevParse::Off,
        other => {
            let allowed = if allow_off {
                "'hint', 'information', 'warning', or 'off'"
            } else {
                "'hint', 'information', or 'warning'"
            };
            SevParse::Unknown(format!("unknown value {other:?}; expected {allowed}"))
        }
    }
}

/// Sort findings by score (highest first), assign severity by rank, and
/// build a path → ranked-finding map. Findings beyond `highlight_top` with
/// `tail_severity = None` are dropped from the map entirely.
fn rank_and_group(
    findings: Vec<Finding>,
    pub_opts: &PublishOptions,
) -> HashMap<PathBuf, Vec<RankedFinding>> {
    let mut sorted = findings;
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut map: HashMap<PathBuf, Vec<RankedFinding>> = HashMap::new();
    for (rank, f) in sorted.into_iter().enumerate() {
        let severity = if rank < pub_opts.highlight_top {
            Some(pub_opts.highlight_severity)
        } else {
            pub_opts.tail_severity
        };
        let Some(sev) = severity else {
            continue; // tailSeverity = "off"
        };
        let a_path = f.a.path.clone();
        let b_path = f.b.path.clone();
        let same_file = a_path == b_path;
        let rf = RankedFinding {
            finding: f,
            severity: sev,
        };
        map.entry(a_path).or_default().push(rf.clone());
        if !same_file {
            map.entry(b_path).or_default().push(rf);
        }
    }
    map
}

/// Render a finding as a Diagnostic anchored at the endpoint that lives in
/// `this_path`. Returns `None` if neither endpoint belongs to this file or
/// if we lack a granule range (file-level findings can't be anchored).
fn make_diagnostic(
    f: &Finding,
    severity: DiagnosticSeverity,
    this_path: &Path,
) -> Option<Diagnostic> {
    let (this, other) = if f.a.path == this_path {
        (&f.a, &f.b)
    } else if f.b.path == this_path {
        (&f.b, &f.a)
    } else {
        return None;
    };
    let g = this.granule.as_ref()?;
    let range = Range {
        start: Position {
            line: g.line_start.saturating_sub(1),
            character: 0,
        },
        end: Position {
            line: g.line_end,
            character: 0,
        },
    };
    let other_label = match &other.granule {
        Some(og) => format!(
            "{}:{}::{}",
            other.path.display(),
            og.line_start,
            og.fn_name.as_deref().unwrap_or("<anonymous>")
        ),
        None => other.path.display().to_string(),
    };
    let mut message = format!(
        "Possible clone of {} (j={:.2}, score={:.2})",
        other_label, f.exact_jaccard, f.score,
    );
    if !f.tags.is_empty() {
        let tag_list = f
            .tags
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(",");
        message.push_str(&format!(" [{tag_list}]"));
    }
    let related = other.granule.as_ref().and_then(|og| {
        Url::from_file_path(&other.path).ok().map(|uri| {
            vec![DiagnosticRelatedInformation {
                location: Location {
                    uri,
                    range: Range {
                        start: Position {
                            line: og.line_start.saturating_sub(1),
                            character: 0,
                        },
                        end: Position {
                            line: og.line_end,
                            character: 0,
                        },
                    },
                },
                message: "Other endpoint of this clone pair".into(),
            }]
        })
    });
    Some(Diagnostic {
        range,
        severity: Some(severity),
        code: None,
        code_description: None,
        source: Some("tokei-dedup".into()),
        message,
        related_information: related,
        tags: None,
        data: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokei_dedup_classifier::{GranuleRef, ItemRef, Tag};

    #[test]
    fn scan_defaults_are_intentionally_strict() {
        let (opts, _pub, warns) = resolve_init_opts(None);
        assert!(warns.is_empty());
        assert_eq!(opts.granularity, Granularity::Function);
        assert!(matches!(opts.blind, BlindModeExt::Aggressive));
        assert!((opts.min_jaccard - 0.8).abs() < f32::EPSILON);
        assert!(opts.walk.custom_excludes.is_empty());
    }

    #[test]
    fn publish_defaults() {
        let (_scan, pub_opts, warns) = resolve_init_opts(None);
        assert!(warns.is_empty());
        assert_eq!(pub_opts.highlight_top, 20);
        // Default is WARNING — INFORMATION is filtered out of Zed's
        // project diagnostics panel.
        assert_eq!(pub_opts.highlight_severity, DiagnosticSeverity::WARNING);
        assert_eq!(pub_opts.tail_severity, Some(DiagnosticSeverity::HINT));
        assert!(pub_opts.rescan_on_save);
    }

    #[test]
    fn all_scan_keys_round_trip() {
        let raw = json!({
            "granularity": "file",
            "blind": "mild",
            "minJaccard": 0.55,
            "exclude": ["**/generated/**", "tests/**"],
        });
        let (opts, _pub, warns) = resolve_init_opts(Some(&raw));
        assert!(warns.is_empty(), "got {warns:?}");
        assert_eq!(opts.granularity, Granularity::File);
        assert!(matches!(opts.blind, BlindModeExt::Mild));
        assert!((opts.min_jaccard - 0.55).abs() < 1e-6);
        assert_eq!(opts.walk.custom_excludes, vec!["**/generated/**", "tests/**"]);
    }

    #[test]
    fn all_publish_keys_round_trip() {
        let raw = json!({
            "highlightTop": 5,
            "highlightSeverity": "warning",
            "tailSeverity": "information",
            "rescanOnSave": false,
        });
        let (_scan, pub_opts, warns) = resolve_init_opts(Some(&raw));
        assert!(warns.is_empty(), "got {warns:?}");
        assert_eq!(pub_opts.highlight_top, 5);
        assert_eq!(pub_opts.highlight_severity, DiagnosticSeverity::WARNING);
        assert_eq!(pub_opts.tail_severity, Some(DiagnosticSeverity::INFORMATION));
        assert!(!pub_opts.rescan_on_save);
    }

    #[test]
    fn tail_severity_off_drops_long_tail() {
        let raw = json!({ "tailSeverity": "off" });
        let (_scan, pub_opts, warns) = resolve_init_opts(Some(&raw));
        assert!(warns.is_empty(), "got {warns:?}");
        assert!(pub_opts.tail_severity.is_none());
    }

    #[test]
    fn unknown_severity_warns_and_keeps_default() {
        let raw = json!({ "highlightSeverity": "nonsense" });
        let (_scan, pub_opts, warns) = resolve_init_opts(Some(&raw));
        assert_eq!(pub_opts.highlight_severity, DiagnosticSeverity::WARNING);
        assert!(
            warns.iter().any(|w| w.contains("highlightSeverity") && w.contains("nonsense")),
            "expected warning naming the bad value; got {warns:?}"
        );
    }

    #[test]
    fn unknown_granularity_warns_and_keeps_default() {
        let raw = json!({ "granularity": "nonsense" });
        let (opts, _pub, warns) = resolve_init_opts(Some(&raw));
        assert_eq!(opts.granularity, Granularity::Function);
        assert!(warns.iter().any(|w| w.contains("granularity") && w.contains("nonsense")));
    }

    #[test]
    fn out_of_range_min_jaccard_warns() {
        let raw = json!({ "minJaccard": 1.5 });
        let (opts, _pub, warns) = resolve_init_opts(Some(&raw));
        assert!((opts.min_jaccard - 0.8).abs() < f32::EPSILON);
        assert!(warns.iter().any(|w| w.contains("minJaccard")));
    }

    #[test]
    fn malformed_json_warns_and_returns_defaults() {
        let raw = json!({ "exclude": 42 });
        let (opts, pub_opts, warns) = resolve_init_opts(Some(&raw));
        assert!((opts.min_jaccard - 0.8).abs() < f32::EPSILON);
        assert_eq!(pub_opts.highlight_top, 20);
        assert!(warns.iter().any(|w| w.contains("malformed")));
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        let raw = json!({ "granularity": "function", "futureKnob": true });
        let (_opts, _pub, warns) = resolve_init_opts(Some(&raw));
        assert!(warns.is_empty(), "got {warns:?}");
    }

    fn fake_finding(a: &str, b: &str, score: f32) -> Finding {
        Finding {
            a: ItemRef {
                path: PathBuf::from(a),
                lang: "Rust".into(),
                granule: Some(GranuleRef {
                    fn_name: Some("f".into()),
                    line_start: 1,
                    line_end: 10,
                }),
                unique_fps: 20,
            },
            b: ItemRef {
                path: PathBuf::from(b),
                lang: "Rust".into(),
                granule: Some(GranuleRef {
                    fn_name: Some("g".into()),
                    line_start: 20,
                    line_end: 30,
                }),
                unique_fps: 20,
            },
            exact_jaccard: 0.9,
            estimated_jaccard: 0.85,
            shared: 15,
            tags: Vec::<Tag>::new(),
            score,
        }
    }

    #[test]
    fn rank_and_group_tiers_by_score() {
        let mut findings = Vec::new();
        // 30 findings with descending scores; each touches a unique pair of
        // files so we can count per-severity-tier deterministically.
        for i in 0..30 {
            findings.push(fake_finding(
                &format!("a{i}.rs"),
                &format!("b{i}.rs"),
                1.0 - i as f32 * 0.01,
            ));
        }
        let pub_opts = default_publish_opts();
        let map = rank_and_group(findings, &pub_opts);
        let mut highlighted = 0;
        let mut tail = 0;
        for rfs in map.values() {
            for rf in rfs {
                match rf.severity {
                    DiagnosticSeverity::WARNING => highlighted += 1,
                    DiagnosticSeverity::HINT => tail += 1,
                    other => panic!("unexpected severity {other:?}"),
                }
            }
        }
        // 30 findings × 2 endpoints = 60 entries total.
        // Top 20 highlighted (× 2 endpoints) = 40.
        // Remaining 10 tail (× 2 endpoints) = 20.
        assert_eq!(highlighted, 40);
        assert_eq!(tail, 20);
    }

    #[test]
    fn rank_and_group_drops_tail_when_off() {
        let findings: Vec<_> = (0..30)
            .map(|i| {
                fake_finding(
                    &format!("a{i}.rs"),
                    &format!("b{i}.rs"),
                    1.0 - i as f32 * 0.01,
                )
            })
            .collect();
        let pub_opts = PublishOptions {
            highlight_top: 5,
            highlight_severity: DiagnosticSeverity::INFORMATION,
            tail_severity: None,
            rescan_on_save: true,
        };
        let map = rank_and_group(findings, &pub_opts);
        let total: usize = map.values().map(|v| v.len()).sum();
        // Only top 5 published, each touching 2 files → 10 entries.
        assert_eq!(total, 10);
    }

    #[test]
    fn rank_and_group_handles_intra_file_clone_once() {
        // A finding where both endpoints are the same file should only
        // appear once in the map for that file, not twice.
        let findings = vec![fake_finding("same.rs", "same.rs", 1.0)];
        let map = rank_and_group(findings, &default_publish_opts());
        assert_eq!(map.len(), 1);
        assert_eq!(map[&PathBuf::from("same.rs")].len(), 1);
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| DupeServer {
        client,
        state: Arc::new(Mutex::new(State::default())),
        rescan_dirty: Arc::new(AtomicBool::new(false)),
        rescan_notify: Arc::new(Notify::new()),
        rescan_loop_started: AtomicBool::new(false),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
