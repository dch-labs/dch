//! TOML configuration loading for `dch`.

#![warn(missing_docs)]

use std::path::Path;
use std::path::PathBuf;

/// Which LLM provider to target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiType {
    /// `OpenAI`-compatible API (also used by `DeepSeek`, `Grok`, `vLLM`).
    OpenAi,
    /// `Anthropic` Messages API (also used by Z.AI).
    Anthropic,
    /// Google `Gemini`.
    Gemini,
    /// Local `Ollama` server — no API key needed.
    #[default]
    Ollama,
    /// `DeepSeek` API.
    DeepSeek,
    /// xAI `Grok`.
    Grok,
    /// `Z.AI` API.
    Zai,
}

impl ApiType {
    /// The default `base_url` for this provider.
    #[must_use]
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Gemini => "https://generativelanguage.googleapis.com",
            Self::Ollama => "http://localhost:11434/v1",
            Self::DeepSeek => "https://api.deepseek.com",
            Self::Grok => "https://api.x.ai/v1",
            Self::Zai => "https://api.z.ai/api",
        }
    }
}

/// Console output verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// Errors only.
    Quiet,
    /// Default informational output.
    #[default]
    Normal,
    /// Debug-level detail.
    Verbose,
}

/// When the runner prompts for permission before side-effecting actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Never prompts.
    #[default]
    Auto,
    /// Proposes but never executes.
    Plan,
    /// Auto-applies file edits; prompts for everything else.
    AcceptEdits,
    /// Prompts before every side-effecting action.
    Interactive,
}

/// Errors arising while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum DchConfigError {
    /// I/O failure reading a present config file.
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    /// Malformed TOML or schema mismatch in a present file.
    #[error("failed to parse config TOML: {0}")]
    Parse(#[from] toml::de::Error),
}

/// Top-level configuration loaded from `~/.dch/config.toml`.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct DchConfig {
    /// Provider connection settings.
    #[serde(default)]
    pub api: ApiConfig,
    /// Display / rendering preferences.
    #[serde(default)]
    pub display: DisplayConfig,
    /// Runner runtime behavior.
    #[serde(default)]
    pub runner: RunnerConfig,
    /// Telemetry / logging settings.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

/// Provider connection settings.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Primary model identifier.
    pub model: String,
    /// Base URL of the provider endpoint.
    pub base_url: String,
    /// Which provider to speak to.
    pub api_type: ApiType,
    /// Optional API key; may also come from an env var.
    pub api_key: Option<String>,
    /// Max response tokens per turn.
    pub max_tokens: u32,
    /// Context window size of the primary model, in tokens.
    pub context_window: u64,
    /// Per-request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Secondary model used if the primary errors out.
    pub fallback_model: Option<String>,
}

/// Display / rendering preferences.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Disable ANSI color output.
    pub no_color: bool,
    /// How much to print.
    pub verbosity: Verbosity,
    /// Theme name (resolved by the TUI).
    pub theme: String,
}

/// Runner runtime behavior.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct RunnerConfig {
    /// Hard ceiling on turns per run.
    pub max_turns: usize,
    /// Whether to auto-compact the conversation when it grows large.
    pub auto_compact: bool,
    /// Fraction (0.0–1.0) of the context window that triggers compaction.
    pub compact_threshold: f64,
    /// When to prompt the user before side-effecting actions.
    pub permission_mode: PermissionMode,
    /// Optional override for the generated system prompt.
    pub system_prompt: Option<String>,
}

/// Telemetry / logging settings.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Log level string, e.g. `"info"`.
    pub level: String,
    /// Emit structured JSON logs instead of human-readable text.
    pub json_logs: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            base_url: String::new(),
            api_type: ApiType::default(),
            api_key: None,
            max_tokens: 32_000,
            context_window: 200_000,
            request_timeout_secs: 120,
            fallback_model: None,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            no_color: false,
            verbosity: Verbosity::default(),
            theme: "default".to_string(),
        }
    }
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            max_turns: 200,
            auto_compact: true,
            compact_threshold: 0.80,
            permission_mode: PermissionMode::default(),
            system_prompt: None,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            json_logs: false,
        }
    }
}

impl DchConfig {
    /// Load from the default config dir (`~/.dch`).
    ///
    /// # Errors
    ///
    /// Returns an error only if a present config file is unreadable or
    /// malformed. A missing config directory yields defaults.
    pub fn load() -> Result<Self, DchConfigError> {
        Self::load_from_dir(&config_dir())
    }

    /// Load from a specific directory.
    ///
    /// If `config.local.toml` exists it is loaded as a **complete replacement**
    /// for `config.toml` (not a field-level merge). If neither file exists,
    /// returns defaults.
    ///
    /// # Errors
    ///
    /// See [`load`](Self::load).
    pub fn load_from_dir(dir: &Path) -> Result<Self, DchConfigError> {
        let local = dir.join("config.local.toml");
        if local.exists() {
            let content = std::fs::read_to_string(&local)?;
            return Ok(toml::from_str(&content)?);
        }
        let main = dir.join("config.toml");
        if main.exists() {
            let content = std::fs::read_to_string(&main)?;
            return Ok(toml::from_str(&content)?);
        }
        Ok(Self::default())
    }

    /// Map to a [`loopctl::config::LoopConfig`]. Does not validate.
    ///
    /// # Examples
    ///
    /// ```
    /// use dch_config::DchConfig;
    ///
    /// let mut c = DchConfig::default();
    /// c.api.model = "glm-4.7".to_string();
    /// let lc = c.to_loop_config();
    /// assert_eq!(lc.model, "glm-4.7");
    /// ```
    #[must_use]
    pub fn to_loop_config(&self) -> loopctl::config::LoopConfig {
        let defaults = loopctl::config::LoopConfig::default();
        loopctl::config::LoopConfig {
            model: self.api.model.clone(),
            max_tokens: self.api.max_tokens,
            system_prompt: self.runner.system_prompt.clone(),
            max_turns: self.runner.max_turns,
            compact_threshold: self.runner.compact_threshold,
            auto_compact: self.runner.auto_compact,
            session_id: defaults.session_id,
            context_window: self.api.context_window,
        }
    }
}

/// Resolve the config directory (`~/.dch`).
#[must_use]
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dch")
}

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod tests {
    use super::*;
    use std::io::Write;

    const FULL_FIXTURE: &str = r#"
[api]
model = "glm-4.7"
base_url = "http://localhost:11434/v1"
api_type = "ollama"
max_tokens = 8192
context_window = 128000
request_timeout_secs = 60
fallback_model = "glm-4.7-flash"

[display]
no_color = false
verbosity = "verbose"
theme = "dracula"

[runner]
max_turns = 100
auto_compact = false
compact_threshold = 0.75
permission_mode = "accept_edits"
system_prompt = "You are a careful coding assistant."

[telemetry]
level = "debug"
json_logs = true
"#;

    fn write_config(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn test_default_config() {
        let c = DchConfig::default();
        assert_eq!(c.api.model, "");
        assert_eq!(c.api.api_type, ApiType::Ollama);
        assert_eq!(c.api.max_tokens, 32_000);
        assert_eq!(c.api.request_timeout_secs, 120);
        assert_eq!(c.display.verbosity, Verbosity::Normal);
        assert_eq!(c.display.theme, "default");
        assert_eq!(c.runner.max_turns, 200);
        assert!((c.runner.compact_threshold - 0.80).abs() < 1e-9);
        assert_eq!(c.runner.permission_mode, PermissionMode::Auto);
        assert_eq!(c.telemetry.level, "info");
    }

    #[test]
    fn test_load_from_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", FULL_FIXTURE);
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();

        assert_eq!(c.api.model, "glm-4.7");
        assert_eq!(c.api.base_url, "http://localhost:11434/v1");
        assert_eq!(c.api.api_type, ApiType::Ollama);
        assert_eq!(c.api.api_key, None);
        assert_eq!(c.api.max_tokens, 8192);
        assert_eq!(c.api.request_timeout_secs, 60);
        assert_eq!(c.api.fallback_model.as_deref(), Some("glm-4.7-flash"));

        assert_eq!(c.display.verbosity, Verbosity::Verbose);
        assert_eq!(c.display.theme, "dracula");

        assert_eq!(c.runner.max_turns, 100);
        assert!(!c.runner.auto_compact);
        assert!((c.runner.compact_threshold - 0.75).abs() < 1e-9);
        assert_eq!(c.runner.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(
            c.runner.system_prompt.as_deref(),
            Some("You are a careful coding assistant.")
        );

        assert_eq!(c.telemetry.level, "debug");
        assert!(c.telemetry.json_logs);
    }

    #[test]
    fn test_load_from_toml_minimal() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(
            tmp.path(),
            "config.toml",
            "[api]\nmodel = \"x\"\nbase_url = \"y\"\n",
        );
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();

        assert_eq!(c.api.model, "x");
        assert_eq!(c.api.base_url, "y");
        let d = DchConfig::default();
        assert_eq!(c.api.api_type, d.api.api_type);
        assert_eq!(c.api.max_tokens, d.api.max_tokens);
        assert_eq!(c.display.theme, d.display.theme);
        assert_eq!(c.runner.max_turns, d.runner.max_turns);
        assert_eq!(c.telemetry.level, d.telemetry.level);
    }

    #[test]
    fn test_missing_dir_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("absent");
        let got = DchConfig::load_from_dir(&path).unwrap();
        assert_eq!(got.api.max_tokens, 32_000);
    }

    #[test]
    fn test_local_replaces_main() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(
            tmp.path(),
            "config.toml",
            "[api]\nmodel = \"A\"\n[display]\ntheme = \"dracula\"\n",
        );
        write_config(tmp.path(), "config.local.toml", "[api]\nmodel = \"B\"\n");
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        assert_eq!(c.api.model, "B");
        assert_eq!(c.display.theme, "default");
    }

    #[test]
    fn test_malformed_toml_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", "[api model = \"x\"\n");
        let err = DchConfig::load_from_dir(tmp.path()).unwrap_err();
        assert!(matches!(err, DchConfigError::Parse(_)));
    }

    #[test]
    fn test_to_loop_config_mapping() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", FULL_FIXTURE);
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        let lc = c.to_loop_config();

        assert_eq!(lc.model, "glm-4.7");
        assert_eq!(lc.max_tokens, 8192);
        assert_eq!(lc.max_turns, 100);
        assert!(!lc.auto_compact);
        assert!((lc.compact_threshold - 0.75).abs() < 1e-9);
        assert_eq!(
            lc.system_prompt.as_deref(),
            Some("You are a careful coding assistant.")
        );
        assert_eq!(lc.context_window, 128_000);
        assert_ne!(lc.session_id, uuid::Uuid::nil());

        let lc2 = c.to_loop_config();
        assert_ne!(lc.session_id, lc2.session_id);
    }

    #[test]
    fn test_api_type_serde_roundtrip() {
        let openai: ApiConfig = toml::from_str("api_type = \"openai\"\n").unwrap();
        assert_eq!(openai.api_type, ApiType::OpenAi);

        let anthropic: ApiConfig = toml::from_str("api_type = \"anthropic\"\n").unwrap();
        assert_eq!(anthropic.api_type, ApiType::Anthropic);

        let gemini: ApiConfig = toml::from_str("api_type = \"gemini\"\n").unwrap();
        assert_eq!(gemini.api_type, ApiType::Gemini);

        let ollama: ApiConfig = toml::from_str("api_type = \"ollama\"\n").unwrap();
        assert_eq!(ollama.api_type, ApiType::Ollama);

        let deepseek: ApiConfig = toml::from_str("api_type = \"deepseek\"\n").unwrap();
        assert_eq!(deepseek.api_type, ApiType::DeepSeek);

        let grok: ApiConfig = toml::from_str("api_type = \"grok\"\n").unwrap();
        assert_eq!(grok.api_type, ApiType::Grok);

        let zai: ApiConfig = toml::from_str("api_type = \"zai\"\n").unwrap();
        assert_eq!(zai.api_type, ApiType::Zai);

        assert_eq!(
            ApiType::Ollama.default_base_url(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            ApiType::OpenAi.default_base_url(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            ApiType::Gemini.default_base_url(),
            "https://generativelanguage.googleapis.com"
        );
    }

    #[test]
    fn test_to_loop_config_borrows_not_consumes() {
        let c = DchConfig::default();
        let _lc = c.to_loop_config();
        assert_eq!(c.api.base_url, "");
        assert_eq!(c.display.theme, "default");
        assert_eq!(c.runner.permission_mode, PermissionMode::Auto);
        assert_eq!(c.telemetry.level, "info");
    }

    #[test]
    fn test_to_loop_config_max_turns_name_guard() {
        let mut c = DchConfig::default();
        c.runner.max_turns = 42;
        let lc = c.to_loop_config();
        assert_eq!(lc.max_turns, 42);
    }

    #[test]
    fn test_to_loop_config_validate_edge_values() {
        let mut c = DchConfig::default();
        c.api.model = "m".to_string();
        c.runner.compact_threshold = 0.0;
        assert!(c.to_loop_config().validate().is_ok());
        c.runner.compact_threshold = 1.0;
        assert!(c.to_loop_config().validate().is_ok());
        c.runner.compact_threshold = 1.5;
        let lc = c.to_loop_config();
        assert!((lc.compact_threshold - 1.5).abs() < 1e-9);
        assert!(lc.validate().is_err());
    }

    #[test]
    fn test_to_loop_config_system_prompt_none_round_trips() {
        let c = DchConfig::default();
        assert!(c.runner.system_prompt.is_none());
        assert!(c.to_loop_config().system_prompt.is_none());
    }
}
