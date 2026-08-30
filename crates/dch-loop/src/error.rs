//! Errors raised while constructing or running a runner.
//!
//! Each variant carries a string rather than the upstream error type, because
//! `dch_config::DchConfigError` wraps non-`Clone` errors while this enum must
//! remain `Clone`.
/// Error type for runner construction and execution failures.
///
/// Every variant carries its detail as a [`String`] so the enum stays
/// [`Clone`], which the observer plumbing requires; the upstream error types
/// are rendered to text at the conversion site.
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
}
