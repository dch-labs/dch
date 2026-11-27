//! The config document.
//!
//! [`DchConfig`] is the root table; this module loads and validates
//! it and maps it onto the loop's own configuration.

use std::path::Path;
use std::path::PathBuf;

pub(crate) mod api;
pub(crate) mod display;
pub(crate) mod mcp;
pub(crate) mod models;
pub(crate) mod permissions;
pub(crate) mod project;
pub(crate) mod runner;
pub(crate) mod save;
pub(crate) mod security;
pub(crate) mod telemetry;
pub(crate) mod test_support;

use self::api::{
    ApiConfig, FALLBACK_CONTEXT_WINDOW, REASONING_EFFORT_WIRE_NAMES, is_reasoning_effort,
};
use self::display::DisplayConfig;
use self::mcp::McpConfig;
use self::models::ModelProfile;
use self::permissions::PermissionsConfig;
use self::project::ProjectConfig;
use self::runner::{RunnerConfig, TURN_TIMEOUT_FLOOR_SECS};
use self::security::SecurityConfig;
use self::telemetry::TelemetryConfig;

/// Errors arising while loading or saving configuration.
///
/// Returned by [`DchConfig::load`] and [`DchConfig::load_from_dir`] — a
/// missing config file is not an error, defaults are used, so a load
/// fails only when a present file cannot be read or parsed — and by
/// [`DchConfig::save_to_dir`] and [`DchConfig::save`], which add the
/// validation, serialization, and write failures.
#[derive(Debug, thiserror::Error)]
pub enum DchConfigError {
    /// I/O failure reading a present config file or persisting one.
    ///
    /// Wraps the underlying [`std::io::Error`] from a filesystem read on
    /// the load path, or from a create, write, sync, or rename on the
    /// save path, such as a permission denial.
    #[error("config I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// A save refused at a symbolic link whose target is missing.
    ///
    /// The link stands and nothing is staged: writing at the link's own
    /// path would replace it with a plain file and leave the managed
    /// copy missing for good.
    #[error("{path} is a symbolic link whose target is missing; the link was left in place")]
    DanglingLink {
        /// The link the save refused at.
        ///
        /// The config file, or the config directory itself, as the user
        /// named it — the path behind which the managed copy has to be
        /// restored before a save can land.
        path: std::path::PathBuf,
    },

    /// A save refused at a symbolic link it could not resolve.
    ///
    /// The link stands and nothing is staged. The target exists but the
    /// filesystem would not resolve the link to it — a permission
    /// denial, a stale mount, a loop — and a save at the link's own
    /// path would replace the link with a plain file just as it would
    /// for a dangling one.
    #[error(
        "{path} is a symbolic link the save could not resolve ({source}); the link was left in place"
    )]
    UnresolvedLink {
        /// The link the save could not resolve.
        ///
        /// The config file as the user named it, behind which the
        /// managed copy sits.
        path: std::path::PathBuf,

        /// The filesystem's reason the link did not resolve.
        ///
        /// Kept as the error's source, so a caller that reports chains
        /// shows the cause beside the link.
        #[source]
        source: std::io::Error,
    },

    /// Malformed TOML or schema mismatch in a present file.
    ///
    /// Wraps the [`toml::de::Error`] produced when the file's contents fail to
    /// deserialize into [`DchConfig`].
    #[error("failed to parse config TOML: {0}")]
    Parse(#[from] toml::de::Error),

    /// Serialization failure while persisting a configuration.
    ///
    /// Wraps the [`toml::ser::Error`] raised when a value in the
    /// configuration cannot be rendered into a TOML document — the
    /// save stops before any file is touched.
    #[error("failed to serialize config TOML: {0}")]
    Serialize(#[from] toml::ser::Error),

    /// A present config file carried a value the loader rejects.
    ///
    /// Raised by post-parse validation for values whose silently-substituted
    /// behavior would contradict the documented contract — the message names
    /// the field, the rejected value, and the bound it violated.
    #[error("invalid config: {0}")]
    Invalid(String),

    /// A requested model names no `[[models]]` entry the config declares.
    ///
    /// Raised only where the selectable set is closed — a config that
    /// declares entries — because that is the one case where a name can be
    /// checked before a request is spent on it. With no entries declared
    /// there is nothing to choose among and the name passes through to the
    /// `[api]` provider settings, as it always has. The message lists
    /// [`available`](Self::UnknownModel::available) in file order.
    #[error("unknown model {requested:?}; configured models: {available}")]
    UnknownModel {
        /// The name that was asked for, as it was typed.
        ///
        /// Kept verbatim — the operator's typo is the thing to show
        /// back, not a normalized guess at it.
        requested: String,

        /// The declared entry names, comma-separated, in file order.
        ///
        /// File order because that is the order the operator reads
        /// the config in: the message names the choices where the
        /// hand that wrote them finds them.
        available: String,
    },
}

/// Top-level configuration loaded from `~/.dch/config.toml`.
///
/// The root of the config TOML; every section maps onto one of its fields, each
/// of which has its own [`Default`] implementation. When a file or section is
/// absent, [`DchConfig::default`] is used instead.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct DchConfig {
    /// Provider connection settings.
    ///
    /// Model identifier, endpoint, credentials, and token/timeout limits. See
    /// [`ApiConfig`].
    #[serde(default)]
    pub api: ApiConfig,

    /// Selectable models beyond the `[api]` default, in file order.
    ///
    /// The `[[models]]` entries: each names one model and the provider
    /// settings that reach it, and `dch --model` chooses among them — see
    /// [`ModelProfile`] and [`DchConfig::select_model`]. The list is ordered
    /// and **the first entry is the default run**; `[api]` supplies the
    /// settings an entry leaves unstated, and stays the default itself only
    /// while this list is empty — which is every config written before entries
    /// existed.
    #[serde(default)]
    pub models: Vec<ModelProfile>,

    /// Display / rendering preferences.
    ///
    /// Color, verbosity, and theme controls for the console output. See
    /// [`DisplayConfig`].
    #[serde(default)]
    pub display: DisplayConfig,

    /// Runner runtime behavior.
    ///
    /// Turn budget, compaction, permission mode, and prompt overrides. See
    /// [`RunnerConfig`].
    #[serde(default)]
    pub runner: RunnerConfig,

    /// Project / tech-stack context.
    ///
    /// Optional overrides for the auto-detected tech profile (language, build
    /// and test commands, conventions). When a field is `None`, the runner
    /// uses the value it detected from the repo. See [`ProjectConfig`].
    #[serde(default)]
    pub project: ProjectConfig,

    /// Telemetry / logging settings.
    ///
    /// Log level and output format. See [`TelemetryConfig`].
    #[serde(default)]
    pub telemetry: TelemetryConfig,

    /// Security settings.
    ///
    /// Controls how tool output is sanitized before it re-enters the
    /// conversation. See [`SecurityConfig`].
    #[serde(default)]
    pub security: SecurityConfig,

    /// External MCP tool servers.
    ///
    /// Servers to spawn and adapt at startup. See [`McpConfig`].
    #[serde(default)]
    pub mcp: McpConfig,

    /// Permission rules configured ahead of any session.
    ///
    /// The `[permissions]` section: user-authored allow/ask/deny rules that
    /// outrank remembered approvals and the built-in matrix. See
    /// [`PermissionsConfig`].
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

impl DchConfig {
    /// Load from the default config dir (`~/.dch`).
    ///
    /// Reads configuration from the standard location resolved by
    /// [`config_dir`], delegating the actual file loading to
    /// [`DchConfig::load_from_dir`]. When no config file is present it returns
    /// [`DchConfig::default`] unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error only if a present config file is unreadable,
    /// malformed, or carries a value the loader rejects. A missing config
    /// directory yields defaults.
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
            return Self::from_toml_str(&content);
        }
        let main = dir.join("config.toml");
        if main.exists() {
            let content = std::fs::read_to_string(&main)?;
            return Self::from_toml_str(&content);
        }
        Ok(Self::default())
    }

    /// Serialize the whole configuration to TOML text.
    ///
    /// The mirror of [`from_toml_str`](Self::from_toml_str): every field
    /// in effect — defaults where nothing was configured — rendered as
    /// the document a later load parses back to this same
    /// configuration. [`save_to_dir`](Self::save_to_dir) writes exactly
    /// this text.
    ///
    /// # Errors
    ///
    /// Returns the [`toml::ser::Error`] serialization raises, such as a
    /// value no TOML document can carry.
    pub fn to_toml_string(&self) -> Result<String, DchConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Parse configuration from TOML text, rejecting values the runtime
    /// would silently rewrite.
    ///
    /// The one entry from configuration text to a usable config: parsing
    /// and validation are a single operation, so no loader can deserialize
    /// a file and then forget to check it. Every route to a config — the
    /// default directory search, an explicit `--config` file — arrives
    /// here. The defaults and every accepted value pass through untouched,
    /// model names alone excepted: they arrive trimmed — the loader trims
    /// `api.model` and every `[[models]]` entry's name — the one rewrite
    /// it makes.
    ///
    /// # Errors
    ///
    /// Returns [`DchConfigError::Parse`] for malformed TOML and
    /// [`DchConfigError::Invalid`] for a field below the bound its consumer
    /// enforces.
    ///
    /// # Examples
    ///
    /// ```
    /// use dch_config::DchConfig;
    ///
    /// let config = DchConfig::from_toml_str("[api]\nmodel = \"m\"\n").unwrap();
    /// assert_eq!(config.api.model, "m");
    /// ```
    pub fn from_toml_str(text: &str) -> Result<Self, DchConfigError> {
        let mut config: Self = toml::from_str(text)?;
        config.trim_model_names();
        config.validate()?;
        Ok(config)
    }

    /// Reject values whose silently-substituted behavior would contradict
    /// the documented contract.
    ///
    /// Runs after deserialization on every load, so a value the runtime
    /// would quietly rewrite surfaces as a startup error naming the field,
    /// the value, and the bound it violated. Reached through
    /// [`from_toml_str`](Self::from_toml_str) on every load and from
    /// [`save_to_dir`](Self::save_to_dir) before anything is staged, which
    /// is what makes the guarantee hold on every route a file takes to
    /// reach the runner.
    ///
    /// # Errors
    ///
    /// Returns [`DchConfigError::Invalid`] when a runner value sits below
    /// the floor its consumer enforces or outside the repetition guard's
    /// scannable range, when the read ceiling exceeds the
    /// dispatch output cap, when `reasoning_effort` is not a rung of the
    /// ladder, when a model entry names no model or repeats a name
    /// ([`validate_models`](Self::validate_models)), or when a permission
    /// rule is malformed
    /// ([`validate_permissions`](Self::validate_permissions)).
    fn validate(&self) -> Result<(), DchConfigError> {
        if self.runner.turn_timeout_secs < TURN_TIMEOUT_FLOOR_SECS {
            return Err(DchConfigError::Invalid(format!(
                "turn_timeout_secs = {} is below the {}-second floor: the stream handler \
                 replaces a smaller whole-stream budget with its first-event window, so the \
                 configured value would never run",
                self.runner.turn_timeout_secs, TURN_TIMEOUT_FLOOR_SECS
            )));
        }
        if self.runner.rate_limit_delay_secs < 1 {
            return Err(DchConfigError::Invalid(format!(
                "rate_limit_delay_secs = {} is below the 1-second floor: a zero floor \
                 would retry a throttled endpoint immediately, before the burst the \
                 provider is throttling has begun to clear",
                self.runner.rate_limit_delay_secs
            )));
        }
        let period = self.runner.repetition_min_period_bytes;
        let library = loopctl::stream::repetition::RepetitionGuardConfig::default();
        if period != 0 && period < library.min_period_bytes {
            return Err(DchConfigError::Invalid(format!(
                "repetition_min_period_bytes = {period} is below the library's {}-byte floor (0 disarms the \
                 guard): four copies of a shorter window fit inside ordinary text, so the guard would abort \
                 legitimate generation",
                library.min_period_bytes
            )));
        }
        if period > library.max_period_bytes {
            return Err(DchConfigError::Invalid(format!(
                "repetition_min_period_bytes = {period} is above the library's {}-byte period cap: the guard \
                 never scans a window that long, so it would silently never fire — disarm it with 0 instead",
                library.max_period_bytes
            )));
        }
        let cap = self.runner.max_tool_output_chars;
        if cap != 0 && self.runner.read_max_bytes > cap {
            return Err(DchConfigError::Invalid(format!(
                "read_max_bytes = {} exceeds max_tool_output_chars = {cap}: a whole file view \
                 could never be delivered, so the dispatch cap would always cut it before the \
                 read's own continuation marker could name where reading continues",
                self.runner.read_max_bytes
            )));
        }
        if let Some(effort) = &self.api.reasoning_effort
            && !is_reasoning_effort(effort)
        {
            return Err(DchConfigError::Invalid(format!(
                "reasoning_effort = {effort:?} is not a rung of the ladder: the client \
                 refuses an unknown level at construction, so the loader names it here — \
                 expected one of {}",
                REASONING_EFFORT_WIRE_NAMES.join(", ")
            )));
        }
        self.validate_models()?;
        self.validate_permissions()?;
        Ok(())
    }

    /// Map the session-scoped fields to a [`loopctl::config::SessionConfig`].
    ///
    /// Carries the context window, compaction threshold, and auto-compact
    /// flag — the settings that are stable across `run()` calls on the same
    /// agent. The `system_prompt` is **not** carried here (it is set to
    /// `None`); the runner composes it from the selected role, detected tech
    /// stack, and per-tool fragments via the prompt builder and installs it
    /// before the loop reads it. Provider-specific fields (`model`,
    /// `max_tokens`) are not session-config concerns; they are consumed by
    /// the API client via [`ApiConfig`] directly. The session id is minted
    /// at runtime by loopctl, not carried in config.
    ///
    /// `compact_threshold` is clamped to `100` here (the struct-literal
    /// bypasses `SessionConfig`'s own construction clamp), so an out-of-range
    /// config value cannot overflow the `u8` domain.
    ///
    /// # Examples
    ///
    /// ```
    /// use dch_config::DchConfig;
    ///
    /// let mut c = DchConfig::default();
    /// c.api.context_window = Some(128_000);
    /// let sc = c.to_session_config();
    /// assert_eq!(sc.context_window, 128_000);
    /// ```
    #[must_use]
    pub fn to_session_config(&self) -> loopctl::config::SessionConfig {
        loopctl::config::SessionConfig {
            system_prompt: None,
            context_window: self.api.context_window.unwrap_or(FALLBACK_CONTEXT_WINDOW),
            compact_threshold: self.runner.compact_threshold.min(100),
            auto_compact: self.runner.auto_compact,
        }
    }

    /// Map the per-run fields to a [`loopctl::engine::RunConfig`].
    ///
    /// Carries `max_turns` — the per-run turn budget — and the dispatch
    /// mode from [`RunnerConfig::parallel_tool_dispatch`] (mode only; the
    /// concurrency cap stays loopctl's default). The remaining
    /// `RunConfig` fields (manager reset) are left at loopctl's defaults;
    /// they have no equivalent in [`RunnerConfig`] today.
    ///
    /// # Examples
    ///
    /// ```
    /// use dch_config::DchConfig;
    ///
    /// let mut c = DchConfig::default();
    /// c.runner.max_turns = 50;
    /// let rc = c.to_run_config();
    /// assert_eq!(rc.max_turns, 50);
    /// assert!(matches!(
    ///     rc.parallel_tool_dispatch.mode,
    ///     loopctl::config::ParallelMode::Parallel
    /// ));
    /// ```
    #[must_use]
    pub fn to_run_config(&self) -> loopctl::engine::RunConfig {
        let mut rc = loopctl::engine::RunConfig::default();
        rc.max_turns = self.runner.max_turns;
        rc.parallel_tool_dispatch.mode = if self.runner.parallel_tool_dispatch {
            loopctl::config::ParallelMode::Parallel
        } else {
            loopctl::config::ParallelMode::Sequential
        };
        rc
    }
}

/// Resolve the config directory (`~/.dch`).
///
/// Joins `.dch` onto the user's home directory to locate configuration files.
/// When the home directory cannot be determined it falls back to `.` so that
/// loading never panics.
#[must_use]
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::write_config;
    use crate::{ApiType, PermissionMode, Role, Verbosity};

    const FULL_FIXTURE: &str = r#"
[api]
model = "glm-4.7"
base_url = "http://localhost:11434/v1"
api_type = "ollama"
max_tokens = 8192
context_window = 128000
request_timeout_secs = 60
fallback_model = "glm-4.7-flash"
azure_resource = "my-resource"
reasoning_effort = "low"

[[mcp.servers]]
name = "docs"
command = "npx"
args = ["-y", "@example/mcp-docs"]
tools = ["search", "fetch"]

[mcp.servers.env]
DOCS_KEY = "secret"

[display]
no_color = false
verbosity = "verbose"
theme = "dracula"

[runner]
max_turns = 100
turn_timeout_secs = 600
auto_compact = false
compact_threshold = 75
permission_mode = "accept_edits"
role = "coding"

[[runner.role_overrides]]
role = "coding"
prompt = "You are a careful coding assistant."

[telemetry]
level = "debug"
json_logs = true

[security]
redact_secrets = false
"#;

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
        assert_eq!(c.api.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(c.api.azure_resource.as_deref(), Some("my-resource"));
        assert_eq!(c.mcp.servers.len(), 1);
        assert_eq!(c.mcp.servers[0].name, "docs");
        assert_eq!(c.mcp.servers[0].command, "npx");
        assert_eq!(c.mcp.servers[0].args, vec!["-y", "@example/mcp-docs"]);
        assert_eq!(
            c.mcp.servers[0].env.get("DOCS_KEY").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            c.mcp.servers[0].tools,
            Some(vec!["search".to_string(), "fetch".to_string()])
        );

        assert_eq!(c.display.verbosity, Verbosity::Verbose);
        assert_eq!(c.display.theme, "dracula");

        assert_eq!(c.runner.max_turns, 100);
        assert_eq!(c.runner.turn_timeout_secs, 600);
        assert!(!c.runner.auto_compact);
        assert_eq!(c.runner.compact_threshold, 75);
        // accept_edits migrated to interactive on load (with a warning)
        assert_eq!(c.runner.permission_mode, PermissionMode::Interactive);
        assert_eq!(c.runner.role, Role::Coding);
        assert_eq!(
            c.runner.role_override(Role::Coding),
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
        assert_eq!(c.display.theme, "dch");
    }

    #[test]
    fn test_malformed_toml_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", "[api model = \"x\"\n");
        let err = DchConfig::load_from_dir(tmp.path()).unwrap_err();
        assert!(matches!(err, DchConfigError::Parse(_)));
    }

    #[test]
    fn test_to_session_config_mapping() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", FULL_FIXTURE);
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        let sc = c.to_session_config();

        assert!(!sc.auto_compact);
        assert_eq!(sc.compact_threshold, 75);
        assert!(sc.system_prompt.is_none());
        assert_eq!(sc.context_window, 128_000);
    }

    #[test]
    fn test_to_run_config_mapping() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", FULL_FIXTURE);
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        let rc = c.to_run_config();
        assert_eq!(rc.max_turns, 100);
    }

    #[test]
    fn parallel_dispatch_defaults_to_parallel_at_loopctl_concurrency() {
        let rc = DchConfig::default().to_run_config();
        assert!(
            matches!(
                rc.parallel_tool_dispatch.mode,
                loopctl::config::ParallelMode::Parallel
            ),
            "the default config must turn the wave planner on"
        );
        assert_eq!(
            rc.parallel_tool_dispatch.max_concurrency, 8,
            "the concurrency cap stays loopctl's default"
        );
    }

    #[test]
    fn a_parallel_dispatch_opt_out_parses_and_maps_to_sequential() {
        let c: DchConfig = toml::from_str("[runner]\nparallel_tool_dispatch = false\n").unwrap();
        assert!(!c.runner.parallel_tool_dispatch);
        let rc = c.to_run_config();
        assert!(matches!(
            rc.parallel_tool_dispatch.mode,
            loopctl::config::ParallelMode::Sequential
        ));
    }

    #[test]
    fn test_to_session_config_borrows_not_consumes() {
        let c = DchConfig::default();
        let _sc = c.to_session_config();
        assert_eq!(c.api.base_url, "");
        assert_eq!(c.display.theme, "dch");
        assert_eq!(c.runner.permission_mode, PermissionMode::Auto);
        assert_eq!(c.telemetry.level, "info");
    }

    #[test]
    fn test_to_run_config_max_turns_name_guard() {
        let mut c = DchConfig::default();
        c.runner.max_turns = 42;
        let rc = c.to_run_config();
        assert_eq!(rc.max_turns, 42);
    }

    #[test]
    fn test_to_session_config_passes_compact_threshold_through() {
        // compact_threshold is u8 on both RunnerConfig and SessionConfig. The
        // struct-literal bypasses SessionConfig's own clamp, so we clamp here.
        let mut c = DchConfig::default();
        c.runner.compact_threshold = 0;
        assert_eq!(c.to_session_config().compact_threshold, 0);
        c.runner.compact_threshold = 100;
        assert_eq!(c.to_session_config().compact_threshold, 100);
        c.runner.compact_threshold = 50;
        assert_eq!(c.to_session_config().compact_threshold, 50);
        // Out-of-range values are clamped to 100.
        c.runner.compact_threshold = 101;
        assert_eq!(c.to_session_config().compact_threshold, 100);
        c.runner.compact_threshold = 255;
        assert_eq!(c.to_session_config().compact_threshold, 100);
    }

    #[test]
    fn default_has_no_role_overrides_and_session_prompt_is_none() {
        let c = DchConfig::default();
        assert!(
            c.runner.role_overrides.is_empty(),
            "no overrides by default"
        );
        assert!(c.runner.role_override(Role::Coding).is_none());
        assert!(c.to_session_config().system_prompt.is_none());
    }
}
