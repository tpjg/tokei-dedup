//! `dupe-lsp` — LSP server that publishes clone diagnostics for the current workspace.
//!
//! v1 model: scan the workspace once on `initialized`; cache findings by file path;
//! publish diagnostics on `didOpen` and `didSave`. `didChange` is intentionally ignored
//! — incremental re-fingerprinting requires LSH-entry removal which the index doesn't
//! support yet. Trigger a rescan via the `dupe-lsp/rescan` workspace command (TODO) or
//! restart the server.
//!
//! The server runs over stdio (LSP convention): editor pipes `--stdio`, the server
//! reads JSON-RPC frames from stdin and writes to stdout.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokei_dedup_classifier::Finding;
use tokei_dedup_engine::{scan, BlindModeExt, Granularity, ScanOptions, WalkOptions};
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Editor-supplied configuration received in the LSP `initialize` request's
/// `initializationOptions`. All fields optional; missing keys keep the
/// intentionally strict defaults (see [`default_scan_opts`]). Unknown keys
/// are tolerated to keep the LSP forward-compatible — wrong values produce
/// a `WARNING` log on the client.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitOptions {
    granularity: Option<String>,
    blind: Option<String>,
    min_jaccard: Option<f32>,
    exclude: Option<Vec<String>>,
}

struct DupeServer {
    client: Client,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    workspace_root: Option<PathBuf>,
    /// Scan options resolved from `initializationOptions` overlaid on
    /// [`default_scan_opts`]. Locked in at `initialize` time; one workspace
    /// scan uses one config.
    scan_opts: ScanOptions,
    /// Map from absolute file path → all findings touching that file. Built once at
    /// `initialized` and cached for the session.
    by_file: HashMap<PathBuf, Vec<Finding>>,
    scanned: bool,
}

#[tower_lsp::async_trait]
impl LanguageServer for DupeServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root = workspace_root(&params);
        let (scan_opts, warnings) = resolve_scan_opts(params.initialization_options.as_ref());
        for w in &warnings {
            self.client.log_message(MessageType::WARNING, w).await;
        }
        {
            let mut state = self.state.lock().await;
            state.workspace_root = root;
            state.scan_opts = scan_opts;
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
        let (root, opts) = {
            let state = self.state.lock().await;
            (state.workspace_root.clone(), state.scan_opts.clone())
        };
        let Some(root) = root else {
            self.client
                .log_message(MessageType::WARNING, "dupe-lsp: no workspace root, skipping scan")
                .await;
            return;
        };
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "dupe-lsp: scanning {} (granularity={:?}, blind={:?}, min_jaccard={:.2}, excludes={})",
                    root.display(),
                    opts.granularity,
                    opts.blind,
                    opts.min_jaccard,
                    opts.walk.custom_excludes.len(),
                ),
            )
            .await;
        // engine::scan walks the FS and runs CPU work — punt to a blocking thread so
        // the LSP async runtime stays responsive.
        let root_clone = root.clone();
        let scan_result =
            tokio::task::spawn_blocking(move || scan(&root_clone, &opts))
                .await
                .ok();
        let Some(result) = scan_result else {
            self.client
                .log_message(MessageType::ERROR, "dupe-lsp: scan task failed")
                .await;
            return;
        };
        let count = result.findings.len();
        let elapsed = result.elapsed_secs;
        let by_file = group_by_file(result.findings);
        let touched: Vec<PathBuf> = by_file.keys().cloned().collect();
        {
            let mut state = self.state.lock().await;
            state.by_file = by_file;
            state.scanned = true;
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!("dupe-lsp: scan complete — {count} findings touching {} files in {elapsed:.2}s",
                    touched.len()),
            )
            .await;
        // Publish for any file the editor opens later via didOpen.
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.publish_for_uri(&params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // No incremental rescan yet; just re-publish cached diagnostics.
        self.publish_for_uri(&params.text_document.uri).await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl DupeServer {
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
                        .filter_map(|f| make_diagnostic(f, &path))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        self.client.publish_diagnostics(uri.clone(), diagnostics, None).await;
    }
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
/// and a Jaccard floor of 0.8. The bias is towards few high-confidence
/// diagnostics out of the box; users widen the net through
/// `initializationOptions.minJaccard` if they want recall over precision.
fn default_scan_opts() -> ScanOptions {
    ScanOptions {
        blind: BlindModeExt::Aggressive,
        granularity: Granularity::Function,
        min_jaccard: 0.8,
        ..ScanOptions::default()
    }
}

/// Overlay editor-supplied `initializationOptions` onto [`default_scan_opts`].
/// Returns the resolved options plus any non-fatal warning messages (unknown
/// enum values, malformed JSON, etc.) for the client to surface.
fn resolve_scan_opts(raw: Option<&serde_json::Value>) -> (ScanOptions, Vec<String>) {
    let mut opts = default_scan_opts();
    let mut warnings = Vec::new();
    let Some(value) = raw else { return (opts, warnings) };
    let parsed: InitOptions = match serde_json::from_value(value.clone()) {
        Ok(p) => p,
        Err(e) => {
            warnings.push(format!(
                "dupe-lsp: ignoring malformed initializationOptions: {e}"
            ));
            return (opts, warnings);
        }
    };
    if let Some(g) = parsed.granularity.as_deref() {
        match g {
            "file" => opts.granularity = Granularity::File,
            "function" => opts.granularity = Granularity::Function,
            other => warnings.push(format!(
                "dupe-lsp: unknown granularity {other:?} (expected 'file' or 'function'); keeping default"
            )),
        }
    }
    if let Some(b) = parsed.blind.as_deref() {
        match b {
            "strict" => opts.blind = BlindModeExt::Strict,
            "mild" => opts.blind = BlindModeExt::Mild,
            "aggressive" => opts.blind = BlindModeExt::Aggressive,
            other => warnings.push(format!(
                "dupe-lsp: unknown blind {other:?} (expected 'strict', 'mild', or 'aggressive'); keeping default"
            )),
        }
    }
    if let Some(j) = parsed.min_jaccard {
        if (0.0..=1.0).contains(&j) {
            opts.min_jaccard = j;
        } else {
            warnings.push(format!(
                "dupe-lsp: minJaccard={j} out of [0, 1]; keeping default"
            ));
        }
    }
    if let Some(ex) = parsed.exclude {
        opts.walk = WalkOptions {
            custom_excludes: ex,
            ..opts.walk
        };
    }
    (opts, warnings)
}

fn group_by_file(findings: Vec<Finding>) -> HashMap<PathBuf, Vec<Finding>> {
    let mut map: HashMap<PathBuf, Vec<Finding>> = HashMap::new();
    for f in findings {
        // Each finding is referenced from both endpoints.
        let a = f.a.path.clone();
        let b = f.b.path.clone();
        map.entry(a).or_default().push(f.clone());
        if f.a.path != f.b.path {
            map.entry(b).or_default().push(f);
        }
    }
    map
}

/// Render a finding as a Diagnostic anchored at the endpoint that lives in `this_path`.
/// Returns `None` if neither endpoint belongs to this file or if we lack a granule
/// range (file-level findings without specific lines can't be anchored).
fn make_diagnostic(f: &Finding, this_path: &Path) -> Option<Diagnostic> {
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
        severity: Some(DiagnosticSeverity::HINT),
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

    #[test]
    fn defaults_are_intentionally_strict() {
        let (opts, warns) = resolve_scan_opts(None);
        assert!(warns.is_empty(), "no input should produce no warnings");
        assert_eq!(opts.granularity, Granularity::Function);
        assert!(matches!(opts.blind, BlindModeExt::Aggressive));
        assert!((opts.min_jaccard - 0.8).abs() < f32::EPSILON);
        assert!(opts.walk.custom_excludes.is_empty());
    }

    #[test]
    fn all_keys_round_trip() {
        let raw = json!({
            "granularity": "file",
            "blind": "mild",
            "minJaccard": 0.55,
            "exclude": ["**/generated/**", "tests/**"],
        });
        let (opts, warns) = resolve_scan_opts(Some(&raw));
        assert!(warns.is_empty(), "valid config should not warn; got {warns:?}");
        assert_eq!(opts.granularity, Granularity::File);
        assert!(matches!(opts.blind, BlindModeExt::Mild));
        assert!((opts.min_jaccard - 0.55).abs() < 1e-6);
        assert_eq!(opts.walk.custom_excludes, vec!["**/generated/**", "tests/**"]);
    }

    #[test]
    fn unknown_enum_value_warns_and_keeps_default() {
        let raw = json!({ "granularity": "nonsense" });
        let (opts, warns) = resolve_scan_opts(Some(&raw));
        assert_eq!(opts.granularity, Granularity::Function);
        assert!(
            warns.iter().any(|w| w.contains("granularity") && w.contains("nonsense")),
            "expected a warning naming the bad value; got {warns:?}"
        );
    }

    #[test]
    fn out_of_range_min_jaccard_warns() {
        let raw = json!({ "minJaccard": 1.5 });
        let (opts, warns) = resolve_scan_opts(Some(&raw));
        assert!((opts.min_jaccard - 0.8).abs() < f32::EPSILON);
        assert!(warns.iter().any(|w| w.contains("minJaccard")));
    }

    #[test]
    fn malformed_json_warns_and_returns_defaults() {
        // Wrong type for `exclude` (number instead of array of strings).
        let raw = json!({ "exclude": 42 });
        let (opts, warns) = resolve_scan_opts(Some(&raw));
        assert!((opts.min_jaccard - 0.8).abs() < f32::EPSILON);
        assert!(
            warns.iter().any(|w| w.contains("malformed")),
            "expected a malformed-input warning; got {warns:?}"
        );
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        // Forward-compat: new keys we don't know about yet should not break old servers.
        let raw = json!({ "granularity": "function", "futureKnob": true });
        let (_opts, warns) = resolve_scan_opts(Some(&raw));
        assert!(warns.is_empty(), "unknown keys should be silently ignored; got {warns:?}");
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| DupeServer {
        client,
        state: Arc::new(Mutex::new(State::default())),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
