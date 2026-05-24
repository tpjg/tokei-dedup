//! Filesystem watcher that complements `did_save` for changes made outside the
//! editor (Claude / git checkout / formatters / build scripts).
//!
//! Strategy: walk the workspace once via the same [`walk_filtered_dirs`] rules
//! the engine uses, then register a *non-recursive* notify watch on every
//! allowed directory. Excluded directories (`DEFAULT_EXCLUDES`, custom
//! `exclude` patterns, gitignore) get no watch at all — no inotify slot
//! consumed, no FSEventStream subscribed.
//!
//! Events are funneled into the same `pending_paths` set + `rescan_notify`
//! that `did_save` uses, so the existing 500ms debounce coalesces storms and
//! the xxh3 hash dedup in `filter_unchanged_by_hash` absorbs the
//! editor-save-then-fs-event double fire automatically.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(not(target_os = "macos"))]
use tokei_dedup_engine::walk_filtered_dirs;
use tokei_dedup_engine::{WalkOptions, DEFAULT_EXCLUDES};
use tokio::sync::{mpsc, Mutex, Notify};
use tower_lsp::lsp_types::MessageType;
use tower_lsp::Client;

/// Owns the live notify watcher. Dropping this struct unregisters every watch.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
}

impl FileWatcher {
    pub fn spawn(
        workspace_root: PathBuf,
        walk_opts: WalkOptions,
        pending_paths: Arc<Mutex<HashSet<PathBuf>>>,
        rescan_notify: Arc<Notify>,
        client: Client,
    ) -> Result<Self, notify::Error> {
        // Build the set of names to filter at event time. The walker prunes
        // by these at startup, but we use them again to:
        //   (a) drop events whose path lies under an excluded dir (defense
        //       in depth — e.g. a watched dir's own create/remove event
        //       has its own path, not the excluded child's);
        //   (b) cheaply reject paths inside newly-created excluded dirs
        //       (we don't watch new subdirs, but the create event for
        //       the subdir itself fires from the parent watch).
        let exclude_names = build_exclude_names(&walk_opts);

        // Tokio's UnboundedSender::send is sync, so the notify thread can
        // push without blocking and without needing a std::thread bridge.
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })?;

        let summary = add_watches(&mut watcher, &workspace_root, &walk_opts);

        let client_for_log = client.clone();
        let WatchSummary {
            watched,
            errors,
            first_error,
            strategy,
        } = &summary;
        let summary = format!(
            "dupe-lsp: file watcher armed — strategy={strategy}, {watched} watches, {errors} errors{}",
            first_error
                .as_ref()
                .map(|e| format!(" (first: {e})"))
                .unwrap_or_default()
        );
        let errors = *errors;
        tokio::spawn(async move {
            let level = if errors > 0 {
                MessageType::WARNING
            } else {
                MessageType::INFO
            };
            client_for_log.log_message(level, summary).await;
        });

        // Event pump. Lives as long as the channel has senders alive,
        // which is as long as `_watcher` is held by the parent struct.
        let client_pump = client.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if !is_interesting(&event.kind) {
                    continue;
                }
                let raw_count = event.paths.len();
                let first_raw = event
                    .paths
                    .first()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".into());
                let kept: Vec<PathBuf> = event
                    .paths
                    .into_iter()
                    .filter(|p| !is_under_excluded(p, &exclude_names))
                    .collect();
                // Verbose: one LOG line per event. Zed hides LOG by
                // default; bump LSP log verbosity to surface it when
                // debugging why a change wasn't picked up.
                client_pump
                    .log_message(
                        MessageType::LOG,
                        format!(
                            "dupe-lsp: fs event {:?} kept {}/{} first={}",
                            event.kind,
                            kept.len(),
                            raw_count,
                            first_raw,
                        ),
                    )
                    .await;
                if kept.is_empty() {
                    continue;
                }
                let mut inserted = 0usize;
                {
                    let mut pp = pending_paths.lock().await;
                    for p in kept {
                        if pp.insert(p) {
                            inserted += 1;
                        }
                    }
                }
                if inserted > 0 {
                    rescan_notify.notify_one();
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }
}

struct WatchSummary {
    watched: usize,
    errors: usize,
    first_error: Option<String>,
    strategy: &'static str,
}

/// macOS: FSEvents is recursive at the kernel level, and notify's
/// `NonRecursive` mode on macOS does not reliably surface events for
/// files created INSIDE the watched directory (it's a post-hoc filter
/// that's both brittle and wasteful with 6000+ FSEventStreams). One
/// `Recursive` watch on the workspace root gives full coverage via a
/// single FSEventStream. Events from excluded paths are still dropped
/// by `is_under_excluded` at receive time, so excluded dirs never
/// trigger rescans — they just cost a memcmp on each event.
#[cfg(target_os = "macos")]
fn add_watches(
    watcher: &mut RecommendedWatcher,
    workspace_root: &Path,
    _walk_opts: &WalkOptions,
) -> WatchSummary {
    match watcher.watch(workspace_root, RecursiveMode::Recursive) {
        Ok(()) => WatchSummary {
            watched: 1,
            errors: 0,
            first_error: None,
            strategy: "recursive-root (macOS / FSEvents)",
        },
        Err(e) => WatchSummary {
            watched: 0,
            errors: 1,
            first_error: Some(format!("{}: {}", workspace_root.display(), e)),
            strategy: "recursive-root (macOS / FSEvents)",
        },
    }
}

/// Linux / Windows / BSD: per-dir `NonRecursive` watches over the
/// `walk_filtered_dirs` set. Excluded directories get no watch at all
/// — important on Linux because `RecursiveMode::Recursive` would cause
/// notify to walk and add inotify watches per subdir, re-including
/// everything we just pruned and risking the per-user inotify limit.
#[cfg(not(target_os = "macos"))]
fn add_watches(
    watcher: &mut RecommendedWatcher,
    workspace_root: &Path,
    walk_opts: &WalkOptions,
) -> WatchSummary {
    let dirs = walk_filtered_dirs(workspace_root, walk_opts);
    let mut s = WatchSummary {
        watched: 0,
        errors: 0,
        first_error: None,
        strategy: "per-dir non-recursive",
    };
    for dir in &dirs {
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => s.watched += 1,
            Err(e) => {
                s.errors += 1;
                if s.first_error.is_none() {
                    s.first_error = Some(format!("{}: {}", dir.display(), e));
                }
            }
        }
    }
    s
}

fn build_exclude_names(opts: &WalkOptions) -> HashSet<String> {
    let mut names: HashSet<String> = HashSet::new();
    if opts.apply_default_excludes {
        for name in DEFAULT_EXCLUDES {
            names.insert((*name).to_string());
        }
    }
    for pat in &opts.custom_excludes {
        let trimmed = pat.trim_start_matches('!').trim();
        if !trimmed.is_empty() {
            names.insert(trimmed.to_string());
        }
    }
    names
}

fn is_interesting(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// True if any path component literally matches an excluded name. Cheaper
/// than re-running the gitignore matcher per event, and consistent with the
/// "literal name" semantics of `DEFAULT_EXCLUDES`.
fn is_under_excluded(path: &Path, excludes: &HashSet<String>) -> bool {
    path.components().any(|c| {
        matches!(c, Component::Normal(s) if excludes.contains(&*s.to_string_lossy()))
    })
}
