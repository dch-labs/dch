//! The `[telemetry]` section: the logging level and output shape.
//!
//! [`TelemetryConfig`] names the level the logging frontend reads and
//! whether logs are emitted as JSON records.

/// Telemetry / logging settings.
///
/// Configures the logging layer. All fields default via the manual [`Default`]
/// impl below.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Log level string, e.g. `"info"`.
    ///
    /// A level name understood by the logging frontend. Defaults to `"info"`.
    pub level: String,

    /// Emit structured JSON logs instead of human-readable text.
    ///
    /// When true, logs are emitted as JSON records rather than formatted text.
    /// Defaults to `false` (human-readable).
    pub json_logs: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            json_logs: false,
        }
    }
}
