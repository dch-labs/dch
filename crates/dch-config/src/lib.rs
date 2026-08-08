//! TOML configuration loading for `dch`.

#![warn(missing_docs)]

use std::path::Path;
use std::path::PathBuf;

/// Which LLM provider to target.
///
/// Selects the wire protocol and default [`ApiConfig::base_url`] used when
/// talking to a model. The variant also gates provider-specific request shaping
/// elsewhere in the application. Serde (de)serializes values as their lowercase
/// name (e.g. `ollama`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiType {
    /// `OpenAI`-compatible API (also used by `DeepSeek`, `Grok`, `vLLM`).
    ///
    /// The Chat Completions schema that several other providers mirror, so they
    /// reuse this variant rather than getting one of their own.
    OpenAi,

    /// `Anthropic` Messages API (also used by Z.AI).
    ///
    /// Targets the Messages-style request/response shape rather than the
    /// OpenAI-compatible one.
    Anthropic,

    /// Google `Gemini`.
    ///
    /// Targets Google's Generative Language endpoint, which has its own request
    /// schema distinct from the OpenAI-compatible family.
    Gemini,

    /// Local `Ollama` server — no API key needed.
    ///
    /// The default variant, reflecting that a freshly configured `dch` assumes
    /// a locally running server reachable without credentials.
    #[default]
    Ollama,

    /// `DeepSeek` API.
    ///
    /// A standalone variant so the default [`ApiType::default_base_url`] resolves
    /// to the correct `DeepSeek` host.
    DeepSeek,

    /// xAI `Grok`.
    ///
    /// A standalone variant so the default [`ApiType::default_base_url`] resolves
    /// to the correct xAI host.
    Grok,

    /// `Z.AI` API.
    ///
    /// A standalone variant so the default [`ApiType::default_base_url`] resolves
    /// to the correct Z.AI host.
    Zai,
}

impl ApiType {
    /// The default `base_url` for this provider.
    ///
    /// Returns a sensible public or local endpoint for each variant, used to
    /// populate [`ApiConfig::base_url`] when the user has not set one. These are
    /// the canonical host roots; provider clients may append their own path
    /// suffixes on top.
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
///
/// Controls how chatty the display layer is at runtime. It feeds into
/// [`DisplayConfig::verbosity`] and is (de)serialized as its lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// Errors only.
    ///
    /// Suppresses informational output so the console shows failures and nothing
    /// else.
    Quiet,

    /// Default informational output.
    ///
    /// The standard level a user sees without opting into extra detail.
    #[default]
    Normal,

    /// Debug-level detail.
    ///
    /// Adds lower-level diagnostic output on top of the normal level.
    Verbose,
}

/// When the runner prompts for permission before side-effecting actions.
///
/// Sets how aggressively the agent may act without an explicit user decision.
/// It is consumed from [`RunnerConfig::permission_mode`] and (de)serialized as
/// its `snake_case` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Never prompts.
    ///
    /// Lets every side-effecting action run without confirmation. The most
    /// autonomous mode.
    #[default]
    Auto,

    /// Proposes but never executes.
    ///
    /// Produces a plan of intended actions without performing any of them.
    Plan,

    /// Auto-applies file edits; prompts for everything else.
    ///
    /// A middle ground that trusts edits but still asks before other
    /// side-effecting work.
    AcceptEdits,

    /// Prompts before every side-effecting action.
    ///
    /// The most conservative mode, confirming each action individually.
    Interactive,
}

/// Which role the agent takes on — *who* it acts as for this session.
///
/// Each variant selects a distinct body of guidance that shapes how the agent
/// approaches the work — what it optimizes for, what it may edit, and how it
/// sequences exploration and action. It is consumed from
/// [`RunnerConfig::role`] and (de)serialized as its `snake_case` name,
/// matching [`PermissionMode`]'s convention.
///
/// The role is the *instruction* axis (what the agent is told to do and how to
/// think about the task); it is orthogonal to [`PermissionMode`], which is the
/// *enforcement* axis (whether a side effect is allowed to run), and to the
/// project/tech context, which is the *subject* axis (what stack the agent
/// works on — detected from the repo, overridable in `[project]`). Enforcement
/// is handled by the permission layer at runtime regardless of which role is
/// selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// General assistance — PC help, sysadmin, app configuration (the default).
    ///
    /// Directs the agent to diagnose and act on the user's machine: read
    /// logs (`journalctl`, `dmesg`), configure applications, inspect services
    /// and processes, run shell commands, and edit config files. Not
    /// repo-centric; the explore-the-codebase framing of the coding roles
    /// does not apply. dch's primary mode for non-programming work.
    #[default]
    General,

    /// Implement features and fixes end-to-end.
    ///
    /// Directs the agent to read enough of the surrounding code to make a
    /// correct, idiomatic change, apply it, and verify it with the build or
    /// tests. Exploration and reads proceed freely; sizable writes are
    /// surfaced as a plan before being applied.
    Coding,

    /// Improve structure without changing behavior.
    ///
    /// Directs the agent to restructure code while preserving observable
    /// behavior, re-running the tests to prove nothing regressed. A behavior
    /// change forced by the refactor is called out explicitly rather than
    /// folded in silently.
    Refactor,

    /// Reproduce, isolate, then fix.
    ///
    /// Directs the agent to reproduce the failure first, form a hypothesis,
    /// probe to narrow the cause, and only then apply the smallest correct
    /// fix — distinguishing root cause from symptom. Verification confirms the
    /// fix and checks for regressions.
    Debug,

    /// Read-only critical pass over a diff or area.
    ///
    /// Directs the agent to inspect and report findings — bugs, smells, risks
    /// — without editing source. Suggested fixes are offered as proposals,
    /// never applied. No side effects on the codebase.
    Review,

    /// Author or revise documentation.
    ///
    /// Directs the agent to match existing voice and structure, write
    /// substantive content rather than filler, and keep code examples
    /// runnable. May edit documentation and doc-comments, not source logic.
    Docs,

    /// Write and improve tests.
    ///
    /// Directs the agent to cover behavior rather than implementation, prefer
    /// meaningful cases over rote enumeration, and run the suite to confirm
    /// green. May edit test files; treats the code under test as read-only
    /// context.
    Tests,
}

/// Role body for [`Role::General`]: general assistance and sysadmin work.
pub(crate) const GENERAL_ROLE: &str = "\
YOUR ROLE: GENERAL ASSISTANCE
- Help with the user's machine: diagnose issues, configure applications, inspect
  services and processes, read logs and status, run shell commands.
- Read the evidence before theorizing: check logs (`journalctl`, `dmesg`,
  app logs), service status, and config files. Form a hypothesis from what you
  observe, then act.
- Prefer the least-invasive change that resolves the issue. Editing a config
  line or restarting a service beats reinstalling a package.
- Confirm before destructive or system-wide actions (package removals, force
  reloads, edits under `/etc`). State what you intend and why, then act.
- For commands you are unsure of, check `--help` or the man page before
  running; a wrong flag on a system tool can be costly.";

/// Role body for [`Role::Coding`]: implement features end-to-end.
pub(crate) const CODING_ROLE: &str = "\
YOUR ROLE: IMPLEMENT FEATURES AND FIXES
- Read enough of the surrounding code to make a correct, idiomatic change.
- Make the change with Edit or MultiEdit so the diff is visible and reviewable;
  never mutate files with shell scripting (sed, awk, inline python) — those
  edits are invisible in review.
- Verify the change: run the build and the relevant tests (the detected
  commands, if any, are listed in the prompt). Treat a green check as the
  signal the task is done, not the edit itself.
- Keep edits targeted. A single Edit should carry one intent; split work that
  does several things into several edits.
- For unfamiliar or complex operations, look up the established pattern in the
  repo (or via WebFetch) before inventing a new one.";

/// Role body for [`Role::Refactor`]: restructure without behavior change.
pub(crate) const REFACTOR_ROLE: &str = "\
YOUR ROLE: IMPROVE STRUCTURE WITHOUT CHANGING BEHAVIOR
- First characterize the behavior you must preserve: read the code and its
  tests. The tests are the contract — a successful refactor leaves them green.
- Restructure in reviewable steps. After each step, run the tests; if any
  regress, you have changed behavior, not just structure.
- If a behavior change is forced by the refactor, stop and call it out
  explicitly rather than folding it in silently. Refactors and behavior changes
  do not mix in one change.
- Prefer the smallest mechanical move that clarifies the code. Rename, extract,
  inline — one kind of step at a time is easier to review than a mixed rewrite.";

/// Role body for [`Role::Debug`]: reproduce, isolate, then fix.
pub(crate) const DEBUG_ROLE: &str = "\
YOUR ROLE: REPRODUCE, ISOLATE, AND FIX
- Reproduce the failure first. A reproducible failure is fixable; an
  un-reproduced one is a guess. Capture the exact command, input, and observed
  output before theorizing.
- Form one hypothesis and probe it with Read, Grep, and Bash (logs, verbose
  flags, a minimal repro script). Narrow the cause before touching a fix.
- Fix the root cause, not the symptom. The smallest change that removes the
  failure mode at its source is usually right; papering over a symptom moves
  the bug elsewhere.
- After fixing, confirm the repro now passes and run the surrounding tests to
  catch regressions. Distinguish clearly between what you observed, what you
  inferred, and what you changed.";

/// Role body for [`Role::Review`]: read-only critical pass.
pub(crate) const REVIEW_ROLE: &str = "\
YOUR ROLE: REVIEW AND REPORT — DO NOT EDIT SOURCE
- Treat this as a read-only pass. Inspect the diff or area with Read, Grep, and
  Bash (git, tests) and report findings; do not apply changes to source.
- Organize findings by severity: correctness bugs first, then risks and design
  smells, then style. For each, name the file and line and explain the concern
  concretely.
- Offer suggested fixes as proposals (\"consider extracting X\", \"this could
  overflow if N < 0\"), not as applied edits. Let the user decide what to act
  on.
- Call out anything you could not verify. A reviewer's value is honesty about
  what was checked and what wasn't.";

/// Role body for [`Role::Docs`]: author or revise documentation.
pub(crate) const DOCS_ROLE: &str = "\
YOUR ROLE: WRITE OR REVISE DOCUMENTATION
- Match the existing voice, structure, and formatting of the docs around you.
  Consistency with neighbors reads as one coherent document; a clashing style
  reads as noise.
- Write substantive content. Document the why and the how-to, with runnable
  examples; avoid filler lines that exist only to pad length.
- Keep code examples accurate and runnable. If you cannot verify a command or
  snippet, say so rather than presenting it as tested.
- You may edit documentation files and doc-comments. Do not change source
  logic under the documentation — if the docs and the code disagree, flag the
  discrepancy rather than silently \"fixing\" one to match the other.";

/// Role body for [`Role::Tests`]: write and improve tests.
pub(crate) const TESTS_ROLE: &str = "\
YOUR ROLE: WRITE AND IMPROVE TESTS
- Cover behavior, not implementation. A test that pins a public outcome
  survives refactors; one that asserts private call shape breaks under
  harmless restructuring.
- Prefer a few meaningful cases (including the edge: empty, off-by-one, the
  bug being fixed) over rote enumeration of identical inputs. Each test should
  fail for one identifiable reason if it fails.
- Run the suite to confirm green after writing. A test that does not yet pass
  is a finding, not a deliverable — report it and its cause.
- You may edit test files. Treat the code under test as read-only context; if
  the code itself is wrong, say so rather than weakening a test to match it.";

impl Role {
    /// The role's system-prompt prose body.
    ///
    /// Each variant returns its own [`Role`]-specific guidance. The runner
    /// prepends the shared agent discipline, the detected tech profile, and
    /// the per-tool fragments; this is only the role-specific portion.
    #[must_use]
    pub const fn system_prompt(self) -> &'static str {
        match self {
            Role::General => GENERAL_ROLE,
            Role::Coding => CODING_ROLE,
            Role::Refactor => REFACTOR_ROLE,
            Role::Debug => DEBUG_ROLE,
            Role::Review => REVIEW_ROLE,
            Role::Docs => DOCS_ROLE,
            Role::Tests => TESTS_ROLE,
        }
    }
}

/// Errors arising while loading configuration.
///
/// Returned by [`DchConfig::load`] and [`DchConfig::load_from_dir`]. A missing
/// config file is not an error — defaults are used — so both variants only arise
/// when a file is present but cannot be read or parsed.
#[derive(Debug, thiserror::Error)]
pub enum DchConfigError {
    /// I/O failure reading a present config file.
    ///
    /// Wraps the underlying [`std::io::Error`] from a filesystem read, such as a
    /// permission denial or a broken symlink.
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed TOML or schema mismatch in a present file.
    ///
    /// Wraps the [`toml::de::Error`] produced when the file's contents fail to
    /// deserialize into [`DchConfig`].
    #[error("failed to parse config TOML: {0}")]
    Parse(#[from] toml::de::Error),
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
}

/// Provider connection settings.
///
/// Everything needed to point the agent at a model endpoint: which model, where
/// it lives, how to authenticate, and the token/timeout limits to enforce. All
/// fields default via the manual [`Default`] impl below.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Primary model identifier.
    ///
    /// The model name the provider expects (e.g. `gpt-4o`). Defaults to an empty
    /// string, meaning "not yet configured".
    pub model: String,

    /// Base URL of the provider endpoint.
    ///
    /// The host root the API client builds request URLs against. Defaults to an
    /// empty string; callers typically fill it from
    /// [`ApiType::default_base_url`].
    pub base_url: String,

    /// Which provider to speak to.
    ///
    /// Selects the wire protocol and default endpoint. Defaults to
    /// [`ApiType::Ollama`]. See [`ApiType`].
    pub api_type: ApiType,

    /// Optional API key; may also come from an env var.
    ///
    /// Sent as a bearer credential when present. Defaults to `None`; local
    /// providers like [`ApiType::Ollama`] do not need one.
    pub api_key: Option<String>,

    /// Max response tokens per turn.
    ///
    /// Caps the length of each model reply, in tokens. Defaults to `32_000`.
    pub max_tokens: u32,

    /// Context window size of the primary model, in tokens.
    ///
    /// The full window the model can attend to per request. Defaults to
    /// `200_000`; feeds into compaction decisions via
    /// [`DchConfig::to_session_config`].
    pub context_window: u64,

    /// Per-request timeout in seconds.
    ///
    /// How long a single API request may take before being aborted. Defaults to
    /// `120` seconds.
    pub request_timeout_secs: u64,

    /// Secondary model used if the primary errors out.
    ///
    /// Falls back to this model identifier when a primary request fails.
    /// Defaults to `None`, meaning no fallback is configured.
    pub fallback_model: Option<String>,
}

/// Display / rendering preferences.
///
/// Controls how agent output is presented to the user. All fields default via
/// the manual [`Default`] impl below.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Disable ANSI color output.
    ///
    /// When true, output is plain text with no color escapes. Defaults to
    /// `false` (color enabled).
    pub no_color: bool,

    /// How much to print.
    ///
    /// Selects the runtime verbosity level. Defaults to [`Verbosity::Normal`].
    /// See [`Verbosity`].
    pub verbosity: Verbosity,

    /// Theme name (resolved by the TUI).
    ///
    /// A free-form name the TUI maps to a concrete color scheme. Defaults to
    /// `"default"`.
    pub theme: String,
}

/// Runner runtime behavior.
///
/// Per-run and per-session execution knobs: turn budget, compaction, permission
/// mode, and the system prompt. All fields default via the manual [`Default`]
/// impl below.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct RunnerConfig {
    /// Hard ceiling on turns per run.
    ///
    /// Maximum number of turns a single run may take before stopping, in turns.
    /// Defaults to `200`.
    pub max_turns: usize,

    /// Whether to auto-compact the conversation when it grows large.
    ///
    /// When true, the session compacts itself once it crosses
    /// [`RunnerConfig::compact_threshold`]. Defaults to `true`.
    pub auto_compact: bool,

    /// Compaction threshold as a percentage (0–100) of the context window.
    ///
    /// Matches the `u8` percentage loopctl's `SessionConfig` expects, so the
    /// value passes through `to_session_config` with no conversion. Values
    /// above 100 are clamped to 100 by `SessionConfig`'s construction clamp;
    /// the meaningful range is `0..=100`.
    pub compact_threshold: u8,

    /// When to prompt the user before side-effecting actions.
    ///
    /// Controls how autonomously the agent may act. Defaults to
    /// [`PermissionMode::Auto`]. See [`PermissionMode`].
    pub permission_mode: PermissionMode,

    /// Which role the agent takes on for this session.
    ///
    /// Selects the guidance the agent receives about how to approach the work.
    /// Defaults to [`Role::General`]. See [`Role`]. Consumed by the runner
    /// (which composes the full prompt from the role's prose, the detected
    /// tech stack, and the per-tool fragments); it is **not** carried by
    /// [`DchConfig::to_session_config`], because composing the prompt needs
    /// the tool registry and the repo root, which the config layer does not
    /// own.
    #[serde(default)]
    pub role: Role,

    /// Per-role prose overrides.
    ///
    /// Each entry replaces the built-in prose of one [`Role`] (the shared
    /// discipline, detected tech stack, and per-tool fragments still append).
    /// A role with no entry uses its built-in [`Role::system_prompt`]. This is
    /// the escape hatch for users who want to customize how a specific role
    /// instructs the agent without editing the binary. Loaded from
    /// `[[runner.role_overrides]]`.
    #[serde(default)]
    pub role_overrides: Vec<RoleOverride>,
}

/// A user-supplied replacement for one [`Role`]'s built-in prose.
///
/// The runner looks up the selected [`Role`] in [`RunnerConfig::role_overrides`]
/// and, if present, uses `prompt` in place of [`Role::system_prompt`]. The
/// shared discipline, tech profile, and per-tool fragments still append — only
/// the role-specific body is replaced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RoleOverride {
    /// The role whose built-in prose this entry replaces.
    ///
    /// Matched against the selected [`RunnerConfig::role`]; only an entry
    /// whose role matches the selection takes effect.
    pub role: Role,

    /// The replacement prose. Used verbatim in place of
    /// [`Role::system_prompt`] for this role.
    pub prompt: String,
}

impl RunnerConfig {
    /// Look up a user override for `role`, if any.
    ///
    /// Returns the override's prompt when an entry for `role` exists in
    /// [`Self::role_overrides`], otherwise `None` (meaning: use the role's
    /// built-in [`Role::system_prompt`]).
    #[must_use]
    pub fn role_override(&self, role: Role) -> Option<&str> {
        self.role_overrides
            .iter()
            .find(|o| o.role == role)
            .map(|o| o.prompt.as_str())
    }
}

/// One technology in a project, with its toolchain and conventions.
///
/// Most real projects are polyglot — a Rust core, a `TypeScript` frontend, a
/// Python tooling script — and each language has its own build/test/lint
/// commands and its own conventions. This struct captures one such language;
/// [`ProjectConfig`] holds a `Vec<Tech>` for all of them.
///
/// Every field except `language` is optional: set only what detection got
/// wrong or can't infer. Loaded from the `[[project.techs]]` array-of-tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Tech {
    /// The language this entry describes, e.g. `"rust"` or `"cpp"`.
    ///
    /// Matches against the detected language so the runner can merge detected
    /// and configured entries by language. Required — an entry with no
    /// language names nothing.
    pub language: String,

    /// Command that builds this language's code.
    ///
    /// Overrides the detected build command for this language, e.g.
    /// `"cargo build"`. `None` keeps the detected value (or leaves it empty
    /// when detection found none).
    pub build: Option<String>,

    /// Command that runs this language's tests.
    ///
    /// Overrides the detected test command, e.g. `"cargo test"`. `None`
    /// keeps the detected value.
    pub test: Option<String>,

    /// Command that lints this language's code.
    ///
    /// Overrides the detected lint command, e.g. `"cargo clippy"`. `None`
    /// keeps the detected value.
    pub lint: Option<String>,

    /// Free-form conventions for this language: style rules, module layout,
    /// anything detection can't capture. Appended to the prompt verbatim under
    /// this language's section.
    pub conventions: Option<String>,
}

/// Project / tech-stack overrides for the auto-detected profile.
///
/// Polyglot projects set `[[project.techs]]` once per language; the runner
/// merges each with its detected counterpart by language (set fields override,
/// configured languages detection missed are appended, detected languages the
/// config doesn't mention are kept). `conventions` holds project-wide
/// conventions that span all languages (commit format, branch policy).
///
/// Loaded from the `[project]` table. Absent entirely when the user relies on
/// auto-detection.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Per-language tech entries.
    ///
    /// One `[[project.techs]]` table per language the user wants to declare or
    /// override. The runner merges each with its detected counterpart by
    /// language; entries for languages detection missed are appended.
    pub techs: Vec<Tech>,

    /// Free-form project-wide conventions that apply across all languages.
    ///
    /// Prose for anything detection can't capture: commit-message format,
    /// branch policy, where new modules go. Appended to the prompt verbatim.
    pub conventions: Option<String>,
}

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
            compact_threshold: 80,
            permission_mode: PermissionMode::default(),
            role: Role::default(),
            role_overrides: Vec::new(),
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
    /// Reads configuration from the standard location resolved by
    /// [`config_dir`], delegating the actual file loading to
    /// [`DchConfig::load_from_dir`]. When no config file is present it returns
    /// [`DchConfig::default`] unchanged.
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

    /// Map the session-scoped fields to a [`loopctl::config::SessionConfig`].
    ///
    /// Carries the system prompt, context window, compaction threshold, and
    /// auto-compact flag — the settings that are stable across `run()` calls
    /// on the same agent. Provider-specific fields (`model`, `max_tokens`) are
    /// not session-config concerns; they are consumed by the API client via
    /// [`ApiConfig`] directly. The session id is minted at runtime by loopctl,
    /// not carried in config.
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
    /// c.api.context_window = 128_000;
    /// let sc = c.to_session_config();
    /// assert_eq!(sc.context_window, 128_000);
    /// ```
    #[must_use]
    pub fn to_session_config(&self) -> loopctl::config::SessionConfig {
        loopctl::config::SessionConfig {
            system_prompt: None,
            context_window: self.api.context_window,
            compact_threshold: self.runner.compact_threshold.min(100),
            auto_compact: self.runner.auto_compact,
        }
    }

    /// Map the per-run fields to a [`loopctl::engine::RunConfig`].
    ///
    /// Carries `max_turns` — the per-run turn budget. Other `RunConfig` fields
    /// (parallel dispatch, manager reset) are left at loopctl's defaults; they
    /// have no equivalent in [`RunnerConfig`] today.
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
    /// ```
    #[must_use]
    pub fn to_run_config(&self) -> loopctl::engine::RunConfig {
        let mut rc = loopctl::engine::RunConfig::default();
        rc.max_turns = self.runner.max_turns;
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
compact_threshold = 75
permission_mode = "accept_edits"
role = "coding"

[[runner.role_overrides]]
role = "coding"
prompt = "You are a careful coding assistant."

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
        assert_eq!(c.runner.compact_threshold, 80);
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
        assert_eq!(c.runner.compact_threshold, 75);
        assert_eq!(c.runner.permission_mode, PermissionMode::AcceptEdits);
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
    fn test_to_session_config_borrows_not_consumes() {
        let c = DchConfig::default();
        let _sc = c.to_session_config();
        assert_eq!(c.api.base_url, "");
        assert_eq!(c.display.theme, "default");
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

    #[test]
    fn role_default_is_general() {
        assert_eq!(Role::default(), Role::General);
    }

    #[test]
    fn runner_config_default_carries_general_role() {
        assert_eq!(RunnerConfig::default().role, Role::General);
    }

    #[test]
    fn role_round_trips_through_toml_as_snake_case() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", "[runner]\nrole = \"debug\"\n");
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        assert_eq!(c.runner.role, Role::Debug);

        let serialized = toml::to_string(&RunnerConfig {
            role: Role::Refactor,
            ..RunnerConfig::default()
        })
        .unwrap();
        assert!(
            serialized.contains("role = \"refactor\""),
            "snake_case serialization: {serialized}"
        );
    }
}
