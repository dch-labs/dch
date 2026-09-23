//! LSP server registry and configuration.
//!
//! Maps file extensions to the language-server command that serves them.
//! Only Rust — via `rust-analyzer` — is configured; every other extension
//! maps to `None` and the tool surfaces that as an error.

use std::path::Path;

/// Configuration for one language-server process.
///
/// Describes how to start the server and which file extensions it owns.
/// A config is pure data: the pool owns any process built from it.
#[derive(Debug, Clone)]
pub struct LspServerConfig {
    /// The server binary to execute.
    ///
    /// Resolved through `PATH` at spawn time; absence surfaces as the
    /// tool's "server not found" error.
    pub command: String,

    /// Arguments passed to the server binary.
    ///
    /// Empty for servers whose stdio transport needs no flags.
    pub args: Vec<String>,

    /// The file extensions this server handles, without the leading dot.
    ///
    /// Populated by [`get_server_for_file`] alongside the routing decision
    /// that selected the server, so a caller inspecting a returned config
    /// sees exactly which extensions the server serves. The field is
    /// descriptive: routing reads the extension off the path, not this
    /// list.
    pub file_extensions: Vec<String>,
}

/// The language server for `path`, if one is configured.
///
/// Routes on the path's extension: `.rs` maps to `rust-analyzer`, and
/// every other extension maps to `None` — the caller surfaces that as the
/// "no LSP server configured" error rather than guessing a server.
#[must_use]
pub fn get_server_for_file(path: &Path) -> Option<LspServerConfig> {
    match path.extension()?.to_str()? {
        "rs" => Some(LspServerConfig {
            command: "rust-analyzer".to_string(),
            args: Vec::new(),
            file_extensions: vec!["rs".to_string()],
        }),
        _ => None,
    }
}

/// The file extensions an LSP server is configured for.
///
/// Exactly `["rs"]` — the extensions
/// [`get_server_for_file`] answers for.
#[must_use]
pub fn supported_extensions() -> Vec<&'static str> {
    vec!["rs"]
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rust_files_map_to_rust_analyzer() {
        let config = get_server_for_file(Path::new("src/lib.rs")).expect("rs configured");
        assert_eq!(config.command, "rust-analyzer");
        assert!(config.args.is_empty());
    }

    #[test]
    fn every_other_extension_is_unconfigured() {
        for path in [
            "x.ts", "x.tsx", "x.py", "x.go", "x.c", "x.java", "x.rb", "x.php", "x.xyz",
        ] {
            assert!(
                get_server_for_file(Path::new(path)).is_none(),
                "{path} must be unconfigured"
            );
        }
    }

    #[test]
    fn supported_extensions_is_rust_only() {
        assert_eq!(supported_extensions(), vec!["rs"]);
    }
}
