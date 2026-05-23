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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokei_dedup_classifier::Finding;
use tokei_dedup_engine::{scan, ScanOptions};
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

struct DupeServer {
    client: Client,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    workspace_root: Option<PathBuf>,
    /// Map from absolute file path → all findings touching that file. Built once at
    /// `initialized` and cached for the session.
    by_file: HashMap<PathBuf, Vec<Finding>>,
    scanned: bool,
}

#[tower_lsp::async_trait]
impl LanguageServer for DupeServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root = workspace_root(&params);
        {
            let mut state = self.state.lock().await;
            state.workspace_root = root;
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
        let Some(root) = self.state.lock().await.workspace_root.clone() else {
            self.client
                .log_message(MessageType::WARNING, "dupe-lsp: no workspace root, skipping scan")
                .await;
            return;
        };
        self.client
            .log_message(MessageType::INFO, format!("dupe-lsp: scanning {}", root.display()))
            .await;
        let opts = default_opts();
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

fn default_opts() -> ScanOptions {
    ScanOptions {
        blind: tokei_dedup_engine::BlindModeExt::Aggressive,
        granularity: tokei_dedup_engine::Granularity::Function,
        min_jaccard: 0.6,
        ..ScanOptions::default()
    }
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
