# tokei-dedup — design

Language-agnostic duplicate code detection built on a tokei-style FSM tokenizer.

## Pipeline

```
files → [normalize] → token streams → [slice] → granules
                                                   ↓
                                              [fingerprint]
                                                   ↓
                                              sketches
                                                   ↓
                                              [index/LSH] → candidate pairs
                                                              ↓
                                                         [verify] → confirmed pairs
                                                                       ↓
                                                                  [classify] → ranked report
                                                                       ↓
                                                                  [emit] CLI / LSP / HTML
```

Optional Layer 8: **semantic enrichment** via real LSP servers — the unbuilt-in-OSS angle.

---

## Layer 1 — Normalization (the "tokei layer")

Take any source file, emit a normalized token stream with byte offsets, embedded-language markers, and three configurable blindness levels.

### On using tokei as a library

Tokei's public API gives you *counts*, not the underlying token stream. The FSM inside `tokei::Languages` consumes tokens and discards them. Three options were considered:

| Option | Pros | Cons |
|---|---|---|
| **(A) Vendor `languages.json`, reimplement FSM** | Full control of token stream; track upstream by re-importing JSON | Maintain ~200-line FSM |
| **(B) Fork tokei, expose `tokenize_stream`** | Inherit all of tokei's file walking, gitignore, binary detection | Fork drift; must rebase forever |
| **(C) Upstream a `Visitor` trait PR** | Cleanest long-term | Months of negotiation; may be rejected |

**Chosen: (A) now, with (C) as an eventual long-shot.** The hard-to-maintain part of tokei is its corpus of language definitions (comment markers, string rules, nesting flags, embedded-language hooks). Vendor that as a build-time artifact, write our own FSM. The FSM is mechanical — it's the definitions that drift. Re-import `languages.json` periodically to track upstream.

### Normalized token emission, three modes

Mirrors CCDetect-lsp's "blind nodes" concept:

- **strict** — keep identifiers and literals (Type 1 detection only)
- **mild** — blind literals: `42` → `LIT_NUM`, `"foo"` → `LIT_STR` (Type 1 + literal-renamed Type 2)
- **aggressive** — blind identifiers too: `foo_bar` → `IDENT` (full Type 2)

Each emitted token: `{kind, text_if_kept, byte_start, byte_end, line, col, embedded_lang_stack}`.

### Embedded languages

Same stack pattern as tokei. When the Markdown FSM sees ` ```rust `, push a Rust FSM until ` ``` `; emitted tokens carry the language stack so a duplicate found inside a fenced block is matched as Rust, not Markdown. Same for `<script>` in HTML, code cells in Jupyter (.ipynb).

---

## Layer 2 — Granularity slicing

Two slice modes, both consume Layer 1 output:

- **Sliding window** (cheap, no extra deps): slide an N-token window across the stream. Default N=50. For "recurring pattern" detection.
- **Function-bounded** (needs tree-sitter): one `(function_definition)`-style query per language, slice the normalized stream by returned byte ranges. Tree-sitter loaded only when this mode is requested (feature-gated).

---

## Layer 3 — Fingerprinting

For each granule, compute *all of the following in a single O(n) pass*:

1. **Winnowing fingerprints** (MOSS-style) — robust to small edits.
2. **MinHash sketch** (128 hash functions) — for LSH bucketing.
3. **SimHash** (64-bit) — for very-near-duplicate clustering.
4. **Token shingle bag** (3-grams) — for precise Jaccard in verification.
5. **Optional AST-shape hash** (only in function-bounded mode) — Merkle subtree hash of the tree-sitter node-kind tree.

Multiple signals: cheap recall (LSH) at scale, precise re-checks (Jaccard, AST-shape) on the shortlist.

Granule output: `{file, byte_range, lang_stack, winnowing[], minhash[128], simhash, shingles, ast_hash?}`. Source text not retained.

---

## Layer 4 — Indexing & candidate retrieval

- **LSH over MinHash sketches**: 16 bands × 8 rows. Granules sharing any band are candidate pairs. Sub-linear in pairs. SourcererCC's trick.
- **Suffix array over concatenated normalized stream**: with file-boundary sentinels, finds maximal repeated substrings in O(n log n). CCDetect-lsp's algorithm. Best for sliding-window mode.

Function-bounded mode → LSH. Sliding-window mode → suffix array.

---

## Layer 5 — Verification

LSH gives candidate pairs with ~10–30% false positives. For each candidate:

1. Precise Jaccard on shingle bags.
2. Threshold filter (default 0.7, configurable).
3. AST-shape check if both granules have an `ast_hash` and structural mode is on.
4. Token edit distance on a sample for borderline pairs.

---

## Layer 6 — Classification (nwave-dedup-style)

Heuristic rules over confirmed pairs, no ML:

- Both granules in test trees → `TEST_FIXTURE`, demote.
- Generic names (`init`, `setUp`, `handle`, `new`) → `LOW_SIGNAL`.
- Different top-level modules/packages → `CROSS_MODULE`, promote.
- AST-shape match but operator tokens differ → `SHAPE_ONLY`, flag.
- One granule is substring of another at >0.95 → `SUBSET`.
- Confidence × cross-module bonus × demotions → final rank.

---

## Layer 7 — Emit

- **CLI** — human + JSON, exit code reflects threshold breach (for CI).
- **LSP server** — `publishDiagnostics` with related-information. Incremental: re-fingerprint changed files, update LSH in-place.
- **HTML report** — side-by-side diff, ranked by classifier, click-to-jump.

---

## Layer 8 — Semantic enrichment (stretch / novel)

For ranked candidates above threshold, optionally fire up a real language server (rust-analyzer, gopls, pyright) and:

- Query `textDocument/references` on calls inside each granule.
- Compare external symbol sets — same externals → semantically related; disjoint externals → likely independent.
- Use as a confidence multiplier, not a filter.

The differentiator. No OSS tool combines tree-sitter structural fingerprints with LSP semantic enrichment this way.

---

## Tech stack

- **Rust** (Rayon parallelism, consume tokei's JSON natively, fast)
- **Vendored tokei `languages.json`** + custom FSM
- **Optional tree-sitter** for function extraction (feature-gated)
- **xxhash-rust / probminhash** for fingerprinting
- **suffix** crate for suffix arrays
- **tower-lsp** for LSP server
- **clap** for CLI

---

## Crate layout

```
crates/
├── core/             # shared types (NormalizedToken, Granule, Finding)
├── lang-config/      # vendored tokei JSON parser + types
├── normalizer/       # the FSM, emits tokens
├── slicer/           # window + tree-sitter function extraction
├── fingerprinter/    # winnowing, MinHash, SimHash, shingles
├── index/            # LSH + suffix array
├── verifier/         # Jaccard, AST-shape compare
├── classifier/       # heuristic ranking
├── semantic/         # LSP client for Layer 8 (stretch)
├── cli/              # `dupe` binary
└── lsp-server/       # `dupe-lsp` binary
```

---

## Milestones

| # | Milestone | Deliverable |
|---|---|---|
| 0 | Vendor tokei config, FSM passes tokei's own count tests | `lang-config` + `normalizer`, validated |
| 1 | Sliding-window detector + winnowing + naive match | Run on dirty corpus, see real dupes |
| 2 | MinHash + LSH index | Scale test on ~1M LOC |
| 3 | Tree-sitter function slicing (~10 langs) | Function-granularity dupes |
| 4 | Verifier + classifier + HTML report | First "shippable" |
| 5 | LSP server mode | Editor diagnostics |
| 6 | Incremental updates, watch mode | Real-time |
| 7 | Upstream `Visitor` PR to tokei | Drop vendored config |
| 8 | Semantic enrichment via LSP | Research-paper material |

Milestone 4 is the first useful demo (chosen target). 5–8 are independent.

---

## Validation strategy

- **Correctness**: BigCloneBench has labeled clone pairs across 25K Java projects — academic recall/precision standard.
- **Speed**: criterion benches vs jscpd, PMD CPD, Simian, CCDetect-lsp on Linux kernel, CPython, the Rust compiler. Tokei ceiling ~140 MLOC/s; ours should be MLOC/s, not KLOC/s.
- **Real-world**: known-dirty corpora — SaltStack, WordPress, Apache Hadoop. Fetched on demand via `scripts/fetch-corpora.sh`.

---

## Test corpora (milestone 0)

Embedded fixtures: small multi-language files in `tests/fixtures/multi-lang/` for unit tests against tokei's library.

End-to-end (later milestones): pulled by `scripts/fetch-corpora.sh`:

- **SaltStack** — Python, known organic growth
- **WordPress** — PHP, infamous duplication
- **Apache Hadoop** — Java, cross-module duplication
- **BigCloneBench** — academic gold standard
- **TheAlgorithms** monorepos — same algorithms in 10+ languages = guaranteed cross-language clones

Explicitly *not* tested on hand-curated codebases — they're too clean to find interesting findings.
