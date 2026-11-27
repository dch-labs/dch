//! The `[security]` section.
//!
//! [`SecurityConfig`] decides whether tool output is scrubbed of
//! credential-shaped content before it re-enters the conversation.

/// Secrets-redaction settings, in the `[security]` config section.
///
/// Controls whether tool output is scrubbed of credential-shaped content
/// (bearer tokens, API keys, PEM blocks, …) before it re-enters the model's
/// context. The scrubbing itself is performed by loopctl's redaction
/// middleware; this section only decides whether dch installs it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Scrub credential-shaped content from tool output.
    ///
    /// When true (the default), every tool result passes through a
    /// redaction middleware that replaces detected secrets with
    /// `[REDACTED:<kind>]` markers. Disable only for trusted, isolated
    /// environments where raw output must be preserved verbatim.
    pub redact_secrets: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
        }
    }
}
