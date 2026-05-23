# Skill: tokei-dedup

Use when the user asks to find duplicate / cloned / copy-pasted code in a directory.

## Install (once)

```sh
curl -fsSL https://github.com/tpjg/tokei-dedup/releases/latest/download/install.sh | sh
```

Drops `dupe` and `dupe-lsp` into `$HOME/.local/bin` (override with `BIN_DIR=...`).

## Run

```sh
dupe scan <DIR> --granularity function --blind aggressive --min-jaccard 0.6 --top 50 --json
```

Output: JSON to stdout. Parse `.findings` (array, score-sorted descending).

## Picking flags

- `--granularity function` for actionable findings (per-function). `--granularity file` for whole-file.
- `--blind aggressive` to catch renamed identifiers (Type-2 clones). `--blind mild` for exact-text clones.
- `--min-jaccard 0.6` is a sensible default. Raise to 0.8 for stricter; lower to 0.4 for more recall.
- Add `--only-lang <KEY>` to restrict. Function-mode supports `Rust`, `Python`, `JavaScript`, `TypeScript`, `Go`, `Java`, `C` (+`CHeader`), `Cpp` (+`CppHeader`/`CppModule`/`ObjectiveCpp`), `Ruby`, `CSharp`, `Gleam`. Other languages fall back to file mode automatically.
- `.gitignore` + a built-in list (`target`, `node_modules`, `dist`, `build`, `.venv`, `__pycache__`, …) skip build dirs automatically. Add `--exclude PATTERN` (repeatable, gitignore-style) for project-specific noise. Override with `--no-gitignore` / `--no-default-excludes`.

## Finding shape

```json
{"score": 1.0, "exact_jaccard": 1.0, "shared": 80, "tags": ["cross-module"],
 "a": {"path": "...", "fn_name": "...", "line_start": 1, "line_end": 50},
 "b": {"path": "...", "fn_name": "...", "line_start": 100, "line_end": 150}}
```

`score` already incorporates tag adjustments. Sort by `score`. Tags: `cross-module` (boost), `test-only` / `generic-name` / `tiny` (demote), `subset` (flag).

## Don't use the LSP for agents

`dupe-lsp` is for interactive editors. Use the CLI.
