//! Errors raised while constructing or running a runner.

/// Each variant carries a string rather than the upstream error type, because
/// both `loopctl::ApiError` and `dch_config::DchConfigError` wrap non-`Clone`
/// errors while this enum must remain `Clone`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RunnerError {
    /// Failed to construct the API client.
    ///
    /// Common causes: a missing API key, an unsupported provider, or a failure
    /// to build the HTTP client.
    #[error("failed to create API client: {0}")]
    Client(String),

    /// Configuration loading or parsing error.
    ///
    /// Carries the underlying message as a string because
    /// `dch_config::DchConfigError` is not `Clone`. Common causes: a missing
    /// config file, unreadable config file, or malformed TOML.
    #[error("configuration error: {0}")]
    Config(String),

    /// Error returned by the agent loop.
    ///
    /// Carries the underlying message as a string because `loopctl::LoopError`
    /// wraps errors that are not always `Clone`. Common causes: a provider
    /// request failure, compaction error, or turn-limit exhaustion.
    #[error("loop error: {0}")]
    Loop(String),
}
