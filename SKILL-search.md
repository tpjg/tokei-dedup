# Skill: dupe-search (tokei-dedup)

Use BEFORE implementing a new function to check whether similar code
already exists in the codebase. Prevents re-inventing existing utilities.

Two modes: snippet search (structural match) and keyword search (identifier match).

## Quick start

```sh
# Snippet search: pipe a rough sketch of what you plan to write
cat <<'SKETCH' | dupe search --snippet - --in <DIR> --json
def validate_email(email):
    if '@' not in email:
        return False
    local, domain = email.split('@', 1)
    return '.' in domain
SKETCH

# Keyword search: space-separated identifier fragments
dupe search --keywords "email validate address" --in <DIR> --json
```

## Pre-built index (faster for repeated searches on large codebases)

```sh
# Build once (or on file changes)
dupe index <DIR> --out .dupe-index.json

# Query against the saved index (instant)
cat <<'SKETCH' | dupe search --snippet - --index .dupe-index.json --json
...
SKETCH

dupe search --keywords "retry backoff" --index .dupe-index.json --json
```

## Picking flags

- **Snippet search** defaults are tuned for the common case:
  `--blind aggressive --granularity function --min-jaccard 0.3`
- Lower `--min-jaccard` to 0.2 to cast a wider net (more noise).
- Raise to 0.5 for higher-confidence matches only.
- Add `--lang Python` (or `Rust`, `JavaScript`, etc.) if the auto-detect
  guesses wrong.
- `--top 5` is usually enough. Raise for exploratory searches.

## Interpreting results

### Snippet search (Jaccard score)

```json
{"rank": 1, "score": 0.61, "path": "src/utils/fmt.py",
 "fn_name": "slugify", "line_start": 8, "line_end": 15, "jaccard": 0.61}
```

- **jaccard >= 0.5**: Very likely does the same thing. Read it before writing.
- **jaccard 0.25-0.5**: Related code. Might be a partial match or different
  approach to same problem. Still worth reading.
- **jaccard < 0.25**: Probably coincidence unless the function name is relevant.

When unsure, run both snippet AND keyword search. If both point to the same
function, it's almost certainly relevant.

### Keyword search (BM25 score)

```json
{"rank": 1, "score": 5.93, "path": "src/utils/validators.py",
 "fn_name": "verify_email_format", "line_start": 7, "line_end": 29, "bm25": 5.93}
```

BM25 scores are relative. A clear gap between #1 and #2 suggests a strong match.
The function name + path are often enough to decide.

## When to use which mode

| Situation | Mode | Why |
|-----------|------|-----|
| You have rough code in mind | `--snippet` | Matches structure and patterns |
| You only have a concept name | `--keywords` | Matches identifier fragments |
| You want both signals | Run both | Results complement each other |

## When NOT to use

- Modifying an existing function (you already found it)
- Writing project-specific business logic obviously unique to this feature
- Codebase is < 5 kLOC (just grep or read the directory tree)
- The user explicitly said "write a new X from scratch"

## What it catches and what it misses

**Catches:** Functions with similar structure (same API calls, control flow,
token patterns) even if names differ completely. Catches renamed-variable
variants (Type-2 clones) thanks to `--blind aggressive`.

**Misses:** Functions solving the same problem with a fundamentally different
algorithm (e.g., regex-based vs. parser-based validation). Also misses
cross-language matches.

## Function-mode language support

Snippet + keyword search with `--granularity function` works for:
Rust, Python, JavaScript, TypeScript, Go, Java, C, C++, Ruby, C#, Gleam.
Other languages fall back to file-level granularity.
