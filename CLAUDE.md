# tokei-dedup

Language-agnostic duplicate code detection and pre-duplication search.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
```

The main binaries are `dupe` (CLI) and `dupe-lsp` (LSP server).

## Before writing new utility functions

Before implementing a new function that could plausibly already exist in the
codebase (validation, parsing, formatting, data transformation, API helpers,
retry/resilience patterns, config loading, serialization), check for existing
implementations first.

### Step 1: Write a rough sketch (5-15 lines)

Write the function you're about to implement as a rough sketch. It doesn't
need to be complete or correct — just capture the structural pattern (control
flow, API calls, variable names).

### Step 2: Search

```sh
# Snippet search (structural match — preferred when you have rough code):
cat <<'SKETCH' | dupe search --snippet - --in <project-root> --json --top 5 --min-jaccard 0.2
<your rough sketch here>
SKETCH

# Keyword search (when you only have a concept):
dupe search --keywords "email validate format" --in <project-root> --json --top 5
```

For large codebases (>50k LOC), build the index first for speed:
```sh
dupe index <project-root> --out .dupe-index.json
cat <<'SKETCH' | dupe search --snippet - --index .dupe-index.json --json
...
SKETCH
```

### Step 3: Evaluate results

- **jaccard >= 0.5** (snippet): Very likely the same thing. Read it first.
- **jaccard 0.25-0.5** (snippet): Related code, worth reading. The existing
  function may solve your problem even if the approach differs.
- **Clear BM25 lead** (keywords): When the top result's BM25 score is 2x+
  higher than #2, it's almost certainly relevant. Read it.
- **No matches**: Proceed with your implementation.

When both snippet and keyword search return the same function, that's a
strong signal regardless of the individual scores.

### When to skip the search

- You're modifying an existing function (you already found it).
- The function is project-specific business logic that couldn't exist elsewhere.
- The codebase is small enough that you've already read the relevant modules.
- The user explicitly asked to write something new from scratch.

## Crate layout

```
crates/
├── core/          # Shared types (NormalizedToken, BlindMode)
├── lang-config/   # Vendored tokei languages.json (332 languages)
├── normalizer/    # FSM tokenizer
├── slicer/        # Tree-sitter function extraction (11 languages)
├── fingerprinter/ # Winnowing + MinHash
├── index/         # LSH + naive inverted index
├── verifier/      # Exact Jaccard
├── classifier/    # Heuristic ranking
├── engine/        # Scan + search pipeline
├── cli/           # `dupe` binary
├── lsp-server/    # `dupe-lsp` binary
└── semantic/      # (Planned) LSP client enrichment
```
