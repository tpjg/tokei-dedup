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

Put your settings in `~/.config/zed/settings.json`. The defaults are intentionally strict (Jaccard floor 0.8, function granularity, aggressive blinding) — widen them here if you want more findings:

```jsonc
{
  "lsp": {
    "dupe-lsp": {
      "initialization_options": {
        "granularity": "function",
        "blind": "aggressive",
        "minJaccard": 0.7,
        "exclude": ["**/generated/**", "vendor/**"]
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

See the [main README's LSP section](../../README.md#configuration-initializationoptions) for the full `initialization_options` schema.

## Languages covered

The extension attaches `dupe-lsp` to: Rust, Python, JavaScript, TypeScript, Go, Java, C, C++, Ruby, C#, Gleam — the languages where `dupe-lsp` does per-function slicing. Other languages still get fingerprinted at file granularity by the underlying scan; add their Zed language IDs to `extension.toml` if you want diagnostics in them too.

## Caveats

- No incremental re-indexing in v1; restart the Zed server to pick up new clones (`zed: restart language server`).
- Diagnostics publish on `didOpen` and `didSave` — they appear when you open or save a file, not before.
