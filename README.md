# tokei-dedup

Language-agnostic duplicate code detection, built on a [tokei](https://github.com/XAMPPRocky/tokei)-style single-pass FSM tokenizer.

**Status: pre-alpha, through milestone 5.** Whole-pipeline scan (332-language tokenizer → winnowing fingerprints → MinHash + banded LSH → tree-sitter function slicing for Rust/Python/JS/Go → exact-Jaccard verifier → heuristic classifier → HTML/JSON/terminal report) plus a basic LSP server. See [`DESIGN.md`](./DESIGN.md) for the architecture and full roadmap.

## What it is

A pipeline that takes a directory of source code, normalizes it through a fast byte-level state machine (driven by tokei's vendored language definitions), and detects duplicate code at two granularities:

- **Whole-function duplicates** (Type 1–3 clones, via tree-sitter slicing today; AST-shape hashing planned in milestone 6)
- **Whole-file duplicates** (winnowing over the file's token stream)

No LLM in the loop. ~1500-file Python repo: end-to-end in well under a second.

## Why

The state of the art is fragmented: tree-sitter-based tools (nwave-dedup, CCDetect-lsp) are slow for large corpora; token-based tools (jscpd, CPD) are language-specific or have weak fuzzy matching; commercial tools (SonarQube) bury duplication as a sub-feature. None combine tokei-class tokenizer speed with modern clone-detection algorithms.

See [`DESIGN.md`](./DESIGN.md) for the long version.

## Install

Requires a stable Rust toolchain.

```sh
git clone https://github.com/tpjg/tokei-dedup
cd tokei-dedup
cargo build --release
# Produces ./target/release/dupe and ./target/release/dupe-lsp
```

## CLI: `dupe scan`

```sh
dupe scan <DIR> [OPTIONS]
```

Most common invocations:

```sh
# Whole-file mode, fast first pass
dupe scan src/

# Per-function clones — needs tree-sitter (Rust/Python/JS/Go currently)
dupe scan src/ --granularity function --blind aggressive --min-jaccard 0.7

# Render an HTML report alongside terminal output
dupe scan src/ --granularity function --html report.html

# Machine-readable JSON for scripting / agents
dupe scan src/ --granularity function --json
```

### Key options

| Flag | Default | Notes |
|---|---|---|
| `--granularity {file,function}` | `file` | `function` slices each file via tree-sitter; supported langs: Rust, Python, JavaScript, Go |
| `--blind {strict,mild,aggressive}` | `mild` | `mild` blinds literals; `aggressive` also blinds identifiers (catches renamed Type-2 clones) |
| `--min-jaccard <F>` | `0.5` | Threshold for the LSH path (default backend) |
| `--top <N>` | `20` | Number of pairs to print in the terminal report |
| `--html <PATH>` | — | Write a standalone HTML report (works with `--json`) |
| `--json` | off | Emit JSON to stdout instead of the human-readable summary |
| `--use-naive` | off | Fall back to the exact all-pairs index (slow on > 5k files; useful as an oracle) |
| `--k <N>` / `--window <N>` | `5`/`4` | MOSS winnowing parameters |
| `--only-lang <KEY>` | all | Restrict to one tokei language key (e.g. `Rust`, `Python`) |

### Output

Terminal:

```
Top 5 finding(s) of 247 (score ↓, granularity=Function, backend=lsh):
  1. score=1.000 j=1.000 shared=  80 (80/80) [Python]
       a: src/graphs/foo.py:146-197::cycle_nodes
       b: src/graphs/foo.py:371-422::cycle_nodes
  ...
```

JSON shape (one finding):

```json
{
  "rank": 1,
  "score": 1.000,
  "exact_jaccard": 1.000,
  "estimated_jaccard": 0.984,
  "shared": 80,
  "tags": [],
  "a": {"path":"...","lang":"Python","fn_name":"cycle_nodes","line_start":146,"line_end":197,"unique_fps":80},
  "b": {"path":"...","lang":"Python","fn_name":"cycle_nodes","line_start":371,"line_end":422,"unique_fps":80}
}
```

Tags that may appear on a finding (any combination):

- `cross-module` — endpoints are in different top-level paths (score × 1.4, capped at 1.0)
- `test-only` — both endpoints look like test code (score × 0.4)
- `generic-name` — a curated boring name (`__init__`, `setUp`, `main`, `fmt`, `clone`, …) (score × 0.6)
- `tiny` — both endpoints have < 15 unique fingerprints (score × 0.5)
- `subset` — smaller endpoint is ≥ 95% contained in the larger (flag only)

## LSP server: `dupe-lsp`

`dupe-lsp` runs over stdio. On `initialize`, it scans the workspace once with `--granularity function --blind aggressive --min-jaccard 0.6`, then publishes `HINT`-severity diagnostics on `didOpen` and `didSave`. Each diagnostic marks the function in the current file and links to the other endpoint via LSP related-information.

### VS Code

Add to `.vscode/settings.json` or a small extension wrapper:

```jsonc
{
  "languageServerExample.serverPath": "/path/to/dupe-lsp"
  // wire via your favorite generic-LSP-client extension
}
```

### Neovim (`nvim-lspconfig` 0.10+)

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
if not configs.dupe_lsp then
  configs.dupe_lsp = {
    default_config = {
      cmd = { 'dupe-lsp' },
      filetypes = { 'rust', 'python', 'javascript', 'go' },
      root_dir = lspconfig.util.root_pattern('.git', 'Cargo.toml', 'pyproject.toml'),
    },
  }
end
lspconfig.dupe_lsp.setup({})
```

### Helix (`.config/helix/languages.toml`)

```toml
[language-server.dupe-lsp]
command = "dupe-lsp"

[[language]]
name = "rust"
language-servers = [ "rust-analyzer", "dupe-lsp" ]
```

**Caveats:** v1 LSP server has no incremental re-indexing — clone diagnostics reflect the workspace state at server start. Restart `dupe-lsp` to pick up new clones. Incremental updates are milestone 6 work.

## For Claude Code (and other AI agents)

Use the **CLI in `--json` mode**. The LSP is designed for interactive editors and doesn't suit a one-shot analysis. See [`SKILL.md`](./SKILL.md).

## Architecture

```
files → normalizer (FSM)
         ↓
     [slicer] ── tree-sitter (function mode only)
         ↓
     fingerprinter ── winnowing + MinHash sketches
         ↓
     index ── LSH (default) or naive inverted (--use-naive)
         ↓
     verifier ── exact Jaccard from full fingerprint sets
         ↓
     classifier ── five-tag heuristic ranking
         ↓
     CLI / HTML / JSON / LSP
```

Crates: `core`, `lang-config`, `normalizer`, `slicer`, `fingerprinter`, `index`, `verifier`, `classifier`, `engine` (orchestrator), `cli`, `lsp-server`, `semantic` (stretch, milestone 8).

## Status

| # | Milestone | State |
|---|---|---|
| 0 | Scaffold + tokei-equivalent FSM, cross-validated | ✓ |
| 1 | Winnowing fingerprints + naive index | ✓ |
| 2 | MinHash + banded LSH | ✓ |
| 3 | Tree-sitter function slicing (4 langs) | ✓ |
| 4 | Verifier + classifier + HTML report | ✓ |
| 5 | LSP server (first cut) | ✓ |
| 6 | Incremental re-indexing, watch mode | — |
| 7 | Upstream `Visitor` PR to tokei | — |
| 8 | Semantic enrichment via LSP | — |

## License

MIT. Vendors tokei's `languages.json` under tokei's MIT license (see `vendor/tokei-LICENSE-MIT`).
