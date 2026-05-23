# Zed extension: tokei-dedup

A Zed integration for [tokei-dedup](https://github.com/tpjg/tokei-dedup) — duplicate-code diagnostics in the editor.

## Prerequisites

Install the `dupe-lsp` binary first (the extension shells out to it; it does **not** bundle it):

```sh
curl -fsSL https://github.com/tpjg/tokei-dedup/releases/latest/download/install.sh | sh
```

Confirm it's on your `$PATH`:

```sh
which dupe-lsp
```

## Install (dev / sideload)

Until this extension is in Zed's official extension index:

1. Clone the tokei-dedup repository.
2. In Zed, open the command palette and run `zed: install dev extension`.
3. Pick the `editors/zed/` directory.

Zed will compile the extension to WebAssembly and load it.

## Configure

Defaults work out of the box: Jaccard floor 0.8, function granularity, aggressive blinding, top-20 findings at `WARNING` severity (so they appear in Zed's "Project Diagnostics" panel), long tail at `HINT`, rescan-on-save on.

> **Why `WARNING` and not `INFORMATION`?** Zed (and several other editors) hide `INFORMATION`-level diagnostics from the project diagnostics panel by default. The default is `WARNING` so findings surface without further configuration.

A field-tested config that's a bit stricter than the defaults (fewer, higher-confidence findings):

```jsonc
{
  "lsp": {
    "dupe-lsp": {
      "initialization_options": {
        "minJaccard": 0.9,
        "highlightTop": 10,
        "highlightSeverity": "warning",
        "tailSeverity": "hint",
        "rescanOnSave": true
      }
    }
  }
}
```

If `dupe-lsp` isn't on `$PATH`, pin its location:

```jsonc
{
  "lsp": {
    "dupe-lsp": {
      "binary": { "path": "/home/me/.local/bin/dupe-lsp" }
    }
  }
}
```

## Schema

See the [main README's LSP section](../../README.md#configuration-initializationoptions) for the full `initialization_options` schema and every supported key.

## Languages covered

The extension attaches `dupe-lsp` to: Rust, Python, JavaScript, TypeScript, Go, Java, C, C++, Ruby, C#, Gleam — the languages where `dupe-lsp` does per-function slicing. Other languages still get fingerprinted at file granularity by the underlying scan; add their Zed language IDs to `extension.toml` if you want diagnostics in them too.

## Caveats

- Save / create / rename / delete trigger a debounced (500 ms) **incremental** update: only the changed files are re-fingerprinted. Save-to-feedback is sub-second on multi-MLOC workspaces. Fall back to the v0.1 full-rescan behavior with `"incremental": false`.
- Editor differences around create / rename / delete events: Zed sends them. If a delete is somehow missed, the index keeps a stale entry until the file is touched again or you restart the LSP (`editor: restart language server`).
