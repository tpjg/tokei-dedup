use zed_extension_api as zed;

/// Bridges Zed's extension API to a user-installed `dupe-lsp` binary.
///
/// The extension does not bundle or download a server; users install
/// `dupe-lsp` themselves (e.g. via the curl-pipe installer in the project
/// README) and the extension simply locates it on `$PATH`. Falls back to
/// the literal name `dupe-lsp` if `which` fails, so a user-supplied
/// `lsp.dupe-lsp.binary.path` override in `settings.json` can still take
/// precedence.
///
/// Init options (`granularity`, `blind`, `minJaccard`, `exclude`) flow
/// through to the server via the standard LSP `initialize` request —
/// Zed merges `lsp.dupe-lsp.initialization_options` from settings.json
/// into the request body for us, so the extension itself doesn't need
/// to know about the schema.
struct TokeiDedupExtension;

impl zed::Extension for TokeiDedupExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        if language_server_id.as_ref() != "dupe-lsp" {
            return Err(format!("unknown language server: {language_server_id}"));
        }
        let command = worktree
            .which("dupe-lsp")
            .unwrap_or_else(|| "dupe-lsp".to_string());
        Ok(zed::Command {
            command,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(TokeiDedupExtension);
