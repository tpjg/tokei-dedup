# tokei-dedup

Language-agnostic duplicate code detection, built on a [tokei](https://github.com/XAMPPRocky/tokei)-style single-pass FSM tokenizer.

**Status: pre-alpha, milestone 0 (scaffold + normalizer).** See [`DESIGN.md`](./DESIGN.md) for the full architecture and roadmap.

## What it is

A pipeline that takes a directory of source code in ~330 languages, normalizes it through a fast byte-level state machine (sharing tokei's language definitions), and detects duplicate code at two granularities:

- **Whole-function duplicates** (Type 1–3 clones, via tree-sitter slicing + AST-shape hashing)
- **Recurring patterns** (sliding-window token streams, via winnowing + MinHash LSH + suffix arrays)

No LLM in the loop. Designed to scan monorepos in seconds, not minutes.

## Why

The state of the art is fragmented: tree-sitter-based tools (nwave-dedup, CCDetect-lsp) are slow for large corpora; token-based tools (jscpd, CPD) are language-specific or have weak fuzzy matching; commercial tools (SonarQube) bury duplication as a sub-feature. None combine tokei-class tokenizer speed with modern clone-detection algorithms, and none use LSP as a *semantic overlay* to distinguish "looks alike but does different things."

See [`DESIGN.md`](./DESIGN.md) for the long version.

## License

MIT. Vendors tokei's `languages.json` under tokei's MIT license (see `vendor/tokei-LICENSE-MIT`).
