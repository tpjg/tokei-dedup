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

Prebuilt binaries for Linux (x86_64 gnu/musl, aarch64 gnu) and macOS (aarch64 / Apple Silicon) are published on every `v*` tag. Intel Macs aren't covered by prebuilt binaries — GitHub is sunsetting the free x86_64 macOS runners; build from source instead.

```sh
curl -fsSL https://github.com/tpjg/tokei-dedup/releases/latest/download/install.sh | sh
```

The script detects your OS/arch, downloads the matching tarball, verifies its SHA256, and installs `dupe` + `dupe-lsp` into `$HOME/.local/bin`. Override with `BIN_DIR=/usr/local/bin sh` or pin a version with `VERSION=v0.1.0 sh`.

### Manual download

If you'd rather not curl-pipe, grab the tarball straight from the [releases page](https://github.com/tpjg/tokei-dedup/releases) — e.g. `tokei-dedup-x86_64-unknown-linux-gnu.tar.gz` — and extract the two binaries.

### Build from source

Requires a stable Rust toolchain.

```sh
git clone https://github.com/tpjg/tokei-dedup
cd tokei-dedup
cargo build --release
# Produces ./target/release/dupe and ./target/release/dupe-lsp
```

> A `cargo install tokei-dedup-cli` path is not wired up yet; the crates aren't published to crates.io.

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
| `--granularity {file,function}` | `file` | `function` slices each file via tree-sitter; see [Language support](#language-support) for the eleven covered languages |
| `--blind {strict,mild,aggressive}` | `mild` | `mild` blinds literals; `aggressive` also blinds identifiers (catches renamed Type-2 clones) |
| `--min-jaccard <F>` | `0.5` | Threshold for the LSH path (default backend) |
| `--top <N>` | `20` | Number of pairs to print in the terminal report |
| `--html <PATH>` | — | Write a standalone HTML report (works with `--json`) |
| `--json` | off | Emit JSON to stdout instead of the human-readable summary |
| `--use-naive` | off | Fall back to the exact all-pairs index (slow on > 5k files; useful as an oracle) |
| `--k <N>` / `--window <N>` | `5`/`4` | MOSS winnowing parameters |
| `--only-lang <KEY>` | all | Restrict to one tokei language key (e.g. `Rust`, `Python`) |
| `--exclude <PATTERN>` | — | Skip gitignore-style pattern (repeatable). E.g. `--exclude target --exclude '**/test_data/**'` |
| `--no-gitignore` | off | Don't read `.gitignore` / `.ignore` files or skip hidden files |
| `--no-default-excludes` | off | Don't apply the built-in exclude list below |

### What gets skipped by default

The walker uses ripgrep's `ignore` crate, composing three layers (all on by default):

1. **`.gitignore` / `.ignore` / `.git/info/exclude`** plus the user-global gitignore. Hidden files (`.foo`) are also skipped. Disable with `--no-gitignore`.
2. **Built-in directory blocklist** — matched as gitignore-style names at any depth: `.git`, `.svn`, `.hg`, `node_modules`, `bower_components`, `.next`, `.nuxt`, `target`, `dist`, `build`, `out`, `bin`, `obj`, `coverage`, `vendor`, `.venv`, `venv`, `__pycache__`, `.tox`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.idea`, `.vscode`. Disable with `--no-default-excludes`.
3. **Custom `--exclude PATTERN`** — gitignore-style globs (`target`, `**/test_data/**`, `*.generated.*`). Repeat the flag.

This means `dupe scan .` in a real project does the right thing without any boilerplate — build outputs, dependency dirs, and virtualenvs are gone. If you actually want to scan them, pass `--no-default-excludes --no-gitignore`.

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

`dupe-lsp` runs over stdio. On `initialize`, it scans the workspace once and **eagerly publishes diagnostics for every affected file** — so the editor's "Problems" / "Project Diagnostics" panel populates immediately, without needing to open each file first.

On save / create / rename / delete, it runs a **debounced (500 ms) incremental update**: only the changed files are re-fingerprinted, their old entries in the LSH index are tombstoned, and just the pair set touching those files is re-verified. The publish step pushes diagnostics only to URIs whose diagnostic list actually changed. Save-to-feedback latency stays sub-second even on multi-million-LOC workspaces. Content hashing means redundant saves (no content change) are no-ops. Set `"incremental": false` to fall back to full rescans on each save.

Diagnostics are tiered by score: the top 20 findings (configurable) get `WARNING` severity so they surface in editor "Problems" panels (Zed and some other editors hide `INFORMATION` from the project view by default, so `WARNING` is the safest visible default); the long tail gets `HINT` (faint inline only). Each diagnostic links to the other endpoint via LSP related-information.

### Configuration (`initializationOptions`)

The defaults are intentionally strict — function-granularity, aggressive blinding, Jaccard floor of **0.8**, top-20 highlighted, rescan on save. Widen via standard LSP `initializationOptions`:

| Key | Type | Default | Notes |
|---|---|---|---|
| `granularity` | `"file" \| "function"` | `"function"` | Coarser file-mode is faster but noisier |
| `blind` | `"strict" \| "mild" \| "aggressive"` | `"aggressive"` | `aggressive` catches renamed Type-2 clones |
| `minJaccard` | number in `[0, 1]` | `0.8` | Lower → more findings, more false positives |
| `exclude` | string[] | `[]` | Gitignore-style globs added on top of the built-in excludes |
| `highlightTop` | integer | `20` | Findings ranked 1..N (by score) get the highlight severity |
| `highlightSeverity` | `"hint" \| "information" \| "warning"` | `"warning"` | What the top-N diagnostics surface as. **Use `"warning"` (the default) for Zed** — Zed filters `INFORMATION`-level diagnostics out of the project panel |
| `tailSeverity` | `"hint" \| "information" \| "warning" \| "off"` | `"hint"` | Severity for findings outside the top; `"off"` drops them entirely |
| `rescanOnSave` | boolean | `true` | Re-process the workspace on every save (debounced 500 ms). Turn off if you'd rather restart the LSP manually |
| `incremental` | boolean | `true` | Use the incremental engine (M6): re-fingerprint only changed files. Set `false` to fall back to a full workspace rescan on every save — same behavior as v0.1.0 |

Unknown keys are tolerated (forward-compat); malformed values produce a `WARNING` log on the client and fall back to defaults.

### VS Code

Add to `.vscode/settings.json` or a small extension wrapper:

```jsonc
{
  "languageServerExample.serverPath": "/path/to/dupe-lsp",
  // pass through your generic-LSP-client extension's initializationOptions hook:
  "languageServerExample.initializationOptions": {
    "minJaccard": 0.7,
    "exclude": ["**/generated/**"]
  }
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
lspconfig.dupe_lsp.setup({
  init_options = {
    granularity = "function",
    blind = "aggressive",
    minJaccard = 0.7,
    exclude = { "**/generated/**" },
  },
})
```

### Helix (`.config/helix/languages.toml`)

```toml
[language-server.dupe-lsp]
command = "dupe-lsp"
config = { granularity = "function", blind = "aggressive", minJaccard = 0.7, exclude = ["**/generated/**"] }

[[language]]
name = "rust"
language-servers = [ "rust-analyzer", "dupe-lsp" ]
```

### Zed

See the [`editors/zed/`](./editors/zed/) extension for a ready-to-install Zed integration. `initialization_options` are passed via `~/.config/zed/settings.json` under `lsp.dupe-lsp.initialization_options`.

**Caveats:** the LSP listens for `didSave`, `didCreateFiles`, `didRenameFiles`, and `didDeleteFiles`. Editors differ in how reliably they emit the last three — Helix and older Neovim versions notably skip some. If the editor misses a delete or rename the index keeps a stale entry until the file is touched again or the LSP is restarted (`editor: restart language server`). Save events always go through, which covers 99 % of update paths.

## For Claude Code (and other AI agents)

Use the **CLI in `--json` mode**. The LSP is designed for interactive editors and doesn't suit a one-shot analysis. See [`SKILL.md`](./SKILL.md).

## Language support

Two layers, with different coverage:

- **Tokenizer** (file-mode counts, comment/string FSM, fingerprints): **all 332 languages** from the vendored tokei `languages.json`. Any file with a known extension is fingerprinted at file granularity.
- **Function slicer** (tree-sitter, required for `--granularity function`):

  | Tokei key(s) | Captures |
  |---|---|
  | `Rust` | `fn` items at module level and inside `impl` blocks |
  | `Python` | `def` at module level and inside classes |
  | `JavaScript` | `function` declarations, `class` methods |
  | `TypeScript` | `function` declarations, `class` methods (anonymous arrows skipped) |
  | `Go` | `func` declarations and method receivers |
  | `Java` | methods and constructors |
  | `C`, `CHeader` | function definitions |
  | `Cpp`, `CppHeader`, `CppModule`, `ObjectiveCpp` | free functions, class methods, `Foo::bar` qualified methods |
  | `Ruby` | `def` methods, `singleton_method` (`def self.x`) |
  | `CSharp` | methods, constructors, destructors |
  | `Gleam` | top-level `fn` declarations |

Any other language falls back to file granularity automatically when `--granularity function` is passed — no error, just no functions extracted.

Adding a language is one new query string and one `bundles.insert(...)` line in `crates/slicer/src/lib.rs`. Strong runner-ups for the next batch: PHP, Kotlin, Swift, Scala, Bash, TSX.

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

## Accuracy testing

Two layers:

1. **Hand-crafted fixtures** under `tests/fixtures/copy-paste*` plant known Type-1/2 clones in small Python files. The default `cargo test` suite asserts the engine surfaces those plants as the top finding under both naive and LSH backends, and that function-mode beats file-mode on function-level clones.
2. **Real-world corpus** — `scripts/fetch-corpora.sh` clones known-dirty repos (TheAlgorithms/Python, Salt, Hadoop, WordPress) into `tests/corpora/`. An opt-in integration test in `crates/cli/tests/end_to_end_real_corpus.rs` points the engine at TheAlgorithms/Python and asserts (a) finding-volume floors, (b) the presence of specific clones humans have flagged before in that repo (`extended_gcd` shared between `modular_division.py` and `diophantine_equation.py`, `binary_search_by_recursion` shared between `binary_search.py` and `exponential_search.py`, etc.), and (c) the cross-module classifier tag firing. Run it with:

   ```sh
   scripts/fetch-corpora.sh the-algorithms-python
   cargo test --release -p tokei-dedup-cli -- --ignored real_corpus
   ```

   On commit `456d644c23` of TheAlgorithms/Python: 1,485 files scanned in ~0.25s, 2,010 findings at j ≥ 0.7, all four asserted known-clone pairs present.

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
