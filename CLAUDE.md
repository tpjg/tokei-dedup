# tokei-dedup

`cargo build --workspace` / `cargo test --workspace`

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

### Step 3: Evaluate results

- **jaccard >= 0.5** (snippet): Very likely the same thing. Read it first.
- **jaccard 0.25-0.5** (snippet): Related code, worth reading.
- **No matches**: Proceed with your implementation.
