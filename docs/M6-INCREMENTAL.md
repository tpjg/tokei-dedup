# Milestone 6 — incremental re-indexing

Design doc for the LSP-side incremental update path.

## Context

Post-PR #12 the LSP does a debounced (500 ms) full workspace rescan on every save. At the time of writing this is "fast enough":

- 343k LOC Rust+Gleam repo: < 1 s rescan, feels instant.
- Linux kernel (≈ 30M LOC): ~20 s — slow but tolerable since a human looking at clone reports will spend much longer digesting them.
- Acceptable for repos up to ~1M LOC.

Beyond that and the save-to-feedback latency becomes the bottleneck. The data structures don't support incremental update today — every scan rebuilds the LSH index from scratch and discards it after publishing.

**Goal.** When one file changes, do work proportional to that file's footprint, not the workspace size. Save latency should be dominated by editor round-trip, not by the scan.

**Non-goal.** A standalone `dupe watch` CLI. The CLI is already fast enough as a CLI — at the scale where it matters, parsing the report takes longer than producing it.

## What "incremental" means here

Touch file F:

1. Re-fingerprint F (cheap — one-file work, already parallelisable per granule).
2. Remove F's old fingerprints from the LSH index.
3. Insert F's new fingerprints.
4. Recompute *only the pairs that touch F* (old pairs to invalidate, new pairs to verify).
5. Diff the finding set; publish only the URIs whose diagnostics actually changed.

Adds, deletes, renames are the same pattern with one of those steps trivialised.

## The hard part: LSH index doesn't support removal today

`crates/index/src/lib.rs` is append-only. IDs are positional indices into `Vec<FileMeta>` / `Vec<Sketch>`, and `buckets: HashMap<u64, Vec<u32>>` references those IDs directly. No way to "remove" without reassigning everything.

**Chosen approach: tombstoning + lazy compaction.**

- `FileMeta` gains `alive: bool`.
- `LshIndex` gains a `tombstoned: usize` counter and a `path_to_ids: HashMap<PathBuf, Vec<u32>>` for "which IDs belong to this path?"
- `remove_by_path()` flips `alive = false` for matching IDs and bumps the counter.
- `pair_report()` and `candidate_pairs()` skip dead IDs.
- `compact()` rebuilds vectors + buckets when `tombstoned / files.len() > 0.25` (or on explicit request).

Memory cost: a dead `FileMeta` + `Sketch` is ~200 bytes. A 100k-granule index with 25 % tombstones is +5 MB. Acceptable.

ID stability: alive IDs never move except during `compact()`, which is a checkpoint we drive — the LSP can hold IDs across saves safely.

## Plan: three phases, one branch

This work all lands on `claude/m6-incremental-indexing` as one PR. Either the full end-to-end story works (CLI unchanged; LSP gets near-instant save latency on big repos) or we don't merge. The current CLI + full-rescan LSP already covers the < 1M LOC case well, so there's no pressure to ship a half-finished version.

Each phase below is a logical commit; reviewers can read the branch commit-by-commit.

### Phase 1 — `index`: removal API + compaction

**Files touched:** `crates/index/src/lib.rs`, new tests in same file.

- Add `alive: bool` to `FileMeta`.
- Add `path_to_ids: HashMap<PathBuf, Vec<u32>>` to `LshIndex` (tracks granules within a file too — one path can map to many IDs).
- New methods:
  - `remove_by_path(&Path) -> Vec<u32>` — tombstone all entries for that path, return the IDs removed (callers use these to enumerate partner pairs for invalidation).
  - `partners_of(id: u32) -> impl Iterator<Item = (u32, f32)>` — walk band buckets for `id`, dedupe live IDs, return with sketch-Jaccard estimate.
  - `compact(&mut self)` — drop tombstoned entries, reassign IDs contiguously, rebuild buckets and path map.
  - `tombstone_ratio(&self) -> f32`.
- All existing methods (`pair_report`, `candidate_pairs`, `bucket_size_histogram`) updated to skip dead IDs.

**Tests.** Build index of 10 files, remove 1, assert `partners_of` for the remaining 9 doesn't see the removed one. Compact, repeat with fewer tombstones. Confirms tombstoning doesn't subtly leak into pair retrieval. Round-trip: `add → remove → compact → add same path again` should produce the same query results as `add` from a fresh index.

**Net change:** ~200 LOC. No user-visible behaviour change yet.

### Phase 2 — `engine`: per-path entry + `IncrementalEngine`

**Files touched:** `crates/engine/src/lib.rs`.

- Refactor the existing `process_path` (private) into a public `process_one_path` that returns `Vec<Item>` for one file. No behaviour change for the existing batch path.
- New struct:

  ```rust
  pub struct IncrementalEngine {
      workspace_root: PathBuf,
      opts: ScanOptions,
      normalizer: Normalizer,
      slicer: Slicer,
      minhasher: MinHasher,
      index: LshIndex,
      // pair-keyed cache so we can diff between updates
      verified: HashMap<PairKey, Verified>,
  }

  impl IncrementalEngine {
      pub fn new(workspace_root: PathBuf, opts: ScanOptions) -> Self;
      pub fn initial_scan(&mut self) -> ScanResult;
      pub fn update(&mut self, changed_paths: &[PathBuf]) -> IncrementalResult;
  }
  ```

- `IncrementalResult`:

  ```rust
  pub struct IncrementalResult {
      pub added: Vec<Finding>,
      pub removed: Vec<PairKey>,
      pub updated: Vec<Finding>,   // same pair, changed score/tags
      pub elapsed_secs: f32,
  }
  ```

- `PairKey` is `(path_a, granule_a_range, path_b, granule_b_range)` — stable across reruns so the LSP layer can diff.

**Tests.** Fixture-driven: use the existing `tests/fixtures/copy-paste-functions/` corpus. Mutate files between `update()` calls (add a clone → expect `added` entries; remove the clone → expect `removed`). Cross-validate by comparing `(initial_scan + N updates).findings` against `scan(final state)` — they must agree on the set of `(PairKey, score)` pairs.

**Net change:** ~250 LOC. New public API; nothing in cli or lsp-server calls it yet.

### Phase 3 — `lsp-server`: switch to incremental flow

**Files touched:** `crates/lsp-server/src/main.rs`.

- `State` gains:
  - `engine: Option<IncrementalEngine>` — long-lived; built once on `initialized`.
  - `file_hashes: HashMap<PathBuf, u64>` — short xxh3 of file content, so `didSave` on an unchanged file is a no-op (formatter-on-save round-trips, etc.).
  - `pair_findings: HashMap<PairKey, RankedFinding>` — replaces the path-keyed `by_file`. We derive `by_file` for `publish_for_uri` from this.
- `did_save` path:
  1. Read + hash the saved file. If unchanged from `file_hashes`, return.
  2. Call `engine.update(&[saved_path])`.
  3. Merge `IncrementalResult` into `pair_findings`, re-rank globally (top-N tiering still needs the full sorted view), publish only URIs whose diagnostic list actually changed.
- Wire up `workspace/didCreateFiles`, `workspace/didRenameFiles`, `workspace/didDeleteFiles` — they call `engine.update()` with the affected paths. Rename = delete + add internally.
- The 500 ms debounce + full rescan loop stays as the *fallback*: if the editor doesn't reliably send didRename / didDelete (Helix, older Neovim), the debounced full rescan keeps the index honest. Behind a new init option `incremental` (default `true` once Phase 3 is confirmed working; can flip to `false` to force full rescans).

**Tests.** Existing LSP tests still pass (they cover config parsing + ranking + grouping, none of which changes). Engine-level diff tests from Phase 2 carry the correctness load.

**Acceptance (manual).** Open a clone, observe diagnostic. Save the file with the clone removed, diagnostic disappears in < 100 ms. Save an unrelated file, log shows hash skip and no scan. Save a file with a new clone added, diagnostic appears.

**Net change:** ~250 LOC, mostly orchestration.

## What stays the same

- Fingerprint / sketch / verifier / classifier pipeline — only the indexing layer learns to remove.
- `--use-naive` backend — not getting incremental support. It's an oracle for tests; rebuilding on each save is fine.
- HTML report — one-shot, no incremental story.
- CLI — unchanged. `dupe scan` remains a batch command.

## Risks

1. **Pair set churn under high-edit-rate editing.** Active typing means every save changes content; hash skip can't help. Mitigation: incremental cost per file is O(F's granule count × avg bucket size), not O(workspace). At 1k granules × 5-deep buckets that's a few thousand verifier calls per save, milliseconds.

2. **`workspace/didDelete` / `didRename` reliability.** Editors are inconsistent. If a delete is missed, the index keeps stale entries until next compaction or the fallback full rescan. The 500 ms debounce fallback absorbs this.

3. **Rank churn making the panel jumpy.** With top-N tiering, a single save can promote/demote findings across the threshold. Severity flickers between WARNING and HINT. **Deferred** — ship simple "re-rank globally every update" first; add hysteresis only if real-world use surfaces the problem.

4. **Index growth in long sessions.** Tombstones accumulate; compaction must actually run. Trigger: when `tombstone_ratio > 0.25`, compact at the *end* of the next `update()` call, after diagnostics are published. User never sees the cost.

## Open question that's already resolved

- ~~`dupe watch` CLI in M6?~~ **No.** CLI is fast enough; if you can produce a Linux-kernel-scale report in 20 s, parsing the report takes longer than producing it.
- ~~`incremental` default?~~ **`true`** once Phase 3 is verified working. Full-rescan stays as escape hatch behind `"incremental": false`.
- ~~Rank hysteresis in M6?~~ **No.** Keep it simple; revisit if it actually flickers.

## Done means

- All three phases land on one branch.
- Manual acceptance test on a real repo (343 k LOC Rust+Gleam and the Linux kernel) shows sub-second save-to-diagnostic latency on the small repo and sub-2-second on the big one.
- Full test suite green, including the existing real-corpus integration test from PR #11.

If any phase doesn't pan out cleanly, the branch doesn't merge. The current shipping LSP already covers the < 1M LOC case well.
