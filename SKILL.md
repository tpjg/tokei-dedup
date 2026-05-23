# Skill: tokei-dedup

Use when the user asks to find duplicate / cloned / copy-pasted code in a directory.

## Build (once)

```sh
cargo build --release
```

## Run

```sh
./target/release/dupe scan <DIR> --granularity function --blind aggressive --min-jaccard 0.6 --top 50 --json
```

Output: JSON to stdout. Parse `.findings` (array, score-sorted descending).

## Picking flags

- `--granularity function` for actionable findings (per-function). `--granularity file` for whole-file.
- `--blind aggressive` to catch renamed identifiers (Type-2 clones). `--blind mild` for exact-text clones.
- `--min-jaccard 0.6` is a sensible default. Raise to 0.8 for stricter; lower to 0.4 for more recall.
- Add `--only-lang Rust` (or `Python`, `JavaScript`, `Go`) to restrict.

## Finding shape

```json
{"score": 1.0, "exact_jaccard": 1.0, "shared": 80, "tags": ["cross-module"],
 "a": {"path": "...", "fn_name": "...", "line_start": 1, "line_end": 50},
 "b": {"path": "...", "fn_name": "...", "line_start": 100, "line_end": 150}}
```

`score` already incorporates tag adjustments. Sort by `score`. Tags: `cross-module` (boost), `test-only` / `generic-name` / `tiny` (demote), `subset` (flag).

## Don't use the LSP for agents

`dupe-lsp` is for interactive editors. Use the CLI.
