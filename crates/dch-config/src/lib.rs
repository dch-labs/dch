//! TOML configuration loading for `dch`.

#![warn(missing_docs)]

use std::path::Path;
use std::path::PathBuf;

/// The window assumed when neither the config nor the provider names one.
///
/// The stand-in every discovery failure lands on: large enough for a
/// frontier-model session, small enough to keep a compaction threshold
/// meaningful, and identical everywhere the fallback is needed so the
/// config mapping and the runner agree on it.
pub const FALLBACK_CONTEXT_WINDOW: u64 = 200_000;

/// Which LLM provider to target.
///
/// Selects the wire protocol and default [`ApiConfig::base_url`] used when
/// talking to a model. The variant also gates provider-specific request shaping
/// elsewhere in the application. Serde (de)serializes values as their lowercase
/// name (e.g. `ollama`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ApiType {
    /// `OpenAI`-compatible API.
    ///
    /// The Chat Completions schema that several other providers (including
    /// vLLM servers) speak, so they map to this variant's client family;
    /// `DeepSeek` and `Grok` carry their own variants selecting their
    /// provider profiles while reusing the same wire protocol.
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
    /// A standalone variant selecting its provider profile, whose endpoint,
    /// credential variable, and default model the profile supplies.
    DeepSeek,

    /// xAI `Grok`.
    ///
    /// A standalone variant selecting its provider profile, whose endpoint,
    /// credential variable, and default model the profile supplies.
    Grok,

    /// `Z.AI` API.
    ///
    /// A standalone variant selecting its provider profile, whose endpoint,
    /// credential variable, and default model the profile supplies.
    Zai,

    /// Azure OpenAI.
    ///
    /// The deployment-style endpoint under `https://{resource}.openai.azure.com`.
    /// The resource name comes from [`ApiConfig::azure_resource`] or the
    /// `AZURE_OPENAI_RESOURCE` environment variable; credentials follow the
    /// provider profile (`AZURE_OPENAI_API_KEY`).
    Azure,

    /// Moonshot AI.
    ///
    /// An OpenAI-compatible API served by Moonshot; its endpoint,
    /// credentials, and default model come from its provider profile
    /// (`MOONSHOT_API_KEY`).
    Moonshot,

    /// AWS Bedrock.
    ///
    /// SigV4-authenticated native endpoint; credentials come from the standard
    /// `AWS_*` environment variables, and `api_key`/`base_url` do not apply.
    Bedrock,
}

impl ApiType {
    /// The stock default `base_url` for this provider, when it has one.
    ///
    /// Consulted as the effective endpoint when [`ApiConfig::base_url`] is
    /// unset, for the stock providers (`OpenAi`, `Anthropic`, `Gemini`).
    /// The profiled providers (`Ollama`, `DeepSeek`, `Grok`, `Azure`,
    /// `Moonshot`, `Zai`) take their default endpoint from their provider
    /// profile instead, and `Bedrock` derives its endpoint from the AWS
    /// region, so this returns `None` for all of them — the type itself
    /// keeps "defer to the profile" distinct from a configured host.
    /// Provider clients may append their own path suffixes on top of a
    /// host root.
    #[must_use]
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com"),
            Self::Ollama
            | Self::DeepSeek
            | Self::Grok
            | Self::Zai
            | Self::Azure
            | Self::Bedrock
            | Self::Moonshot => None,
        }
    }

    /// Every provider variant, the enumeration tests iterate.
    ///
    /// A manually maintained list, not a derived one: Rust has no
    /// exhaustive-enumeration reflection, so a variant added to
    /// [`ApiType`] is compiler-forced into
    /// [`Self::default_base_url`]'s match — the real enforcement point —
    /// but not into this list, whose per-variant pins would then silently
    /// skip it. `api_type_all_lists_each_variant_once` catches duplicates
    /// only — a mistyped variant path is a compile error; extend this
    /// list in the same change that adds a variant.
    pub const ALL: [ApiType; 10] = [
        ApiType::OpenAi,
        ApiType::Anthropic,
        ApiType::Gemini,
        ApiType::Ollama,
        ApiType::DeepSeek,
        ApiType::Grok,
        ApiType::Zai,
        ApiType::Azure,
        ApiType::Moonshot,
        ApiType::Bedrock,
    ];
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

/// Where the TUI's activity strip renders while the agent runs.
///
/// Selects the breathing bar's row: the spacer directly above the
/// composer, or a dedicated first row of the terminal. Consumed from
/// [`DisplayConfig::strip_position`] and (de)serialized as its
/// lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StripPosition {
    /// The spacer row directly above the composer.
    ///
    /// The default placement: the bar claims the row the composer's
    /// hairline would otherwise paint, and stands the rule down while
    /// it breathes.
    #[default]
    Composer,

    /// A dedicated first row of the terminal.
    ///
    /// The bar claims screen row 0, full width, above the conversation
    /// pane — which shrinks by one row — and the composer's spacer row
    /// keeps its hairline.
    Top,
}

/// The animation the TUI's activity strip plays while the agent runs.
///
/// Selects the breathing bar's motion. Consumed from
/// [`DisplayConfig::strip_style`] and (de)serialized as its lowercase
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StripStyle {
    /// Grows from the row's center out to both edges, then shrinks
    /// back.
    ///
    /// The default motion: a triangle breath that never fully
    /// disappears — its run keeps a one-cell floor.
    #[default]
    Breathe,

    /// Fills from the left edge to the right end, then clears
    /// left-to-right.
    ///
    /// The right edge stays pinned while the left edge chases it off,
    /// the row blanking for an instant before the fill restarts from
    /// a one-cell seed.
    Sweep,

    /// Sweeps each way in turn — fill and clear left-to-right, then
    /// fill and clear right-to-left, repeating.
    ///
    /// The sweep's motion ping-ponging between the edges: each
    /// direction pass fills from its edge and clears toward the
    /// other, the row blanking for an instant at every turn.
    Bounce,

    /// Glides a short segment from edge to edge and back, repeating.
    ///
    /// The scanner's segment spans a quarter of the row (floored at
    /// three cells) and travels continuously — one pass each way per
    /// cycle, the motion never pausing or blanking.
    Travel,

    /// Circulates a short segment out the right edge and in the left,
    /// one continuous lap after another.
    ///
    /// Travel's segment without the bounce: a quarter-row run
    /// (floored at three cells) advances around the row, its head
    /// entering the left edge while its tail still exits the right —
    /// the row never blanking through the wrap.
    Snake,

    /// Crawls across the row in stretch-and-shrink beats, absorbed at
    /// the far edge before reappearing at the left.
    ///
    /// The inchworm: a seed stretches left-anchored to the center,
    /// shrinks as the tail catches up, stretches center-anchored to
    /// the right edge, and shrinks once more until gone — one
    /// traversal per period.
    Worm,

    /// Stretches from the seed across the whole row to the right
    /// edge, then folds back into it.
    ///
    /// The long worm: the run grows from the left seed along the
    /// worm's eased path until it spans the full row at the period's
    /// half, then shrinks along the same path as the tail races from
    /// the left edge and swallows it into the right; one traversal
    /// per period.
    #[serde(rename = "worm_long")]
    WormLong,

    /// Draws no strip at all.
    ///
    /// The configured row stays empty for the whole run, the
    /// composer's hairline returns in the spacer, and the animation
    /// wake stands down entirely.
    Off,

    /// Holds a full-width bar that never moves, its color slowly
    /// breathing.
    ///
    /// One full breath per period between the strip color and about
    /// half its brightness — a static bar that slightly glows. A
    /// theme whose strip color is a named color renders the bar
    /// without the breath.
    Glow,
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

impl PermissionMode {
    /// The mode's config spelling.
    ///
    /// The `snake_case` name serde reads and writes — the same string
    /// the TUI's status bar shows, so what the user configured and
    /// what the display names never drift apart.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::AcceptEdits => "accept_edits",
            Self::Interactive => "interactive",
        }
    }
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Deserialize, serde::Serialize,
)]
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

/// Role body for [`Role::General`].
///
/// General assistance and sysadmin work — the default for non-programming
/// sessions.
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

/// Role body for [`Role::Coding`].
///
/// Implement features end-to-end: read first, change, then build and test.
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
  repo before inventing a new one.";

/// Role body for [`Role::Refactor`].
///
/// Restructure without behavior change; the suite is the referee.
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

/// Role body for [`Role::Debug`].
///
/// Reproduce, isolate, then fix — evidence before edits.
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

/// Role body for [`Role::Review`].
///
/// A read-only critical pass; findings are reported, not applied.
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

/// Role body for [`Role::Docs`].
///
/// Author or revise documentation; prose and examples, not source logic.
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

/// Role body for [`Role::Tests`].
///
/// Write and improve tests; production code changes only to make them
/// testable.
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

    /// All variants, in declaration order.
    ///
    /// Single source of truth for consumers that need to enumerate roles
    /// (e.g. the message-analysis schema's `enum` constraint, a `--help`
    /// listing). Adding a variant to `Role` and forgetting to add it here is
    /// caught by the `all_roles_covered` test.
    pub const ALL: [Role; 7] = [
        Role::General,
        Role::Coding,
        Role::Refactor,
        Role::Debug,
        Role::Review,
        Role::Docs,
        Role::Tests,
    ];
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

    /// A present config file carried a value the loader rejects.
    ///
    /// Raised by post-parse validation for values whose silently-substituted
    /// behavior would contradict the documented contract — the message names
    /// the field, the rejected value, and the bound it violated.
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// MCP tool-server settings, in the `[mcp]` config section.
///
/// Each entry names one external MCP server whose tools are adapted into the
/// agent's tool registry at startup, exposed as `{name}__{tool}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct McpConfig {
    /// Servers to connect at startup, in configuration order.
    ///
    /// Defaults to empty — no external tools.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One `[[mcp.servers]]` entry: an MCP server to spawn over stdio.
///
/// The command is launched as a child process and speaks the MCP stdio
/// transport; a server that fails to start or complete its handshake is a
/// startup error (fail-closed) rather than a silently missing tool set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct McpServerConfig {
    /// Short name used to prefix the server's tools (`{name}__{tool}`).
    ///
    /// Also names the server in startup errors.
    pub name: String,

    /// Executable to spawn.
    ///
    /// Resolved on `PATH` or taken as an absolute path.
    pub command: String,

    /// Arguments passed to the executable.
    ///
    /// Defaults to empty.
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra environment variables for the child process.
    ///
    /// Defaults to empty.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,

    /// Restrict the server to these tool names.
    ///
    /// Defaults to `None` — every tool the server exposes is adapted. A
    /// listed name the server does not offer is a startup error rather than
    /// a silently missing tool.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
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
}

/// Provider connection settings.
///
/// Everything needed to point the agent at a model endpoint: which model, where
/// it lives, how to authenticate, and the token/timeout limits to enforce. All
/// fields default via the manual [`Default`] impl below.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Primary model identifier.
    ///
    /// The model name the provider expects (e.g. `gpt-4o`). Defaults to an empty
    /// string, meaning "not yet configured".
    pub model: String,

    /// Base URL of the provider endpoint.
    ///
    /// The host root the API client builds request URLs against. Defaults to
    /// an empty string; the stock providers fall back to
    /// [`ApiType::default_base_url`], while the profiled providers fall back
    /// to their loopctl provider-profile endpoint.
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
    /// The full window one request may carry. Unset (the default) means
    /// discover it from the provider when it starts — a local
    /// OpenAI-compatible server is asked directly, and anything without
    /// an answerable capability route falls back to
    /// [`FALLBACK_CONTEXT_WINDOW`]. An explicit value always wins over
    /// discovery. Feeds into compaction decisions via
    /// [`DchConfig::to_session_config`].
    pub context_window: Option<u64>,

    /// Per-request timeout in seconds.
    ///
    /// How long a single API request may take before being aborted. Defaults to
    /// `120` seconds.
    pub request_timeout_secs: u64,

    /// Secondary model used if the primary errors out.
    ///
    /// Routes to this model once the primary has failed often enough to trip
    /// the fallback breaker — not on the first failure. Defaults to `None`,
    /// meaning no fallback is configured.
    pub fallback_model: Option<String>,

    /// Azure OpenAI resource name.
    ///
    /// The `{resource}` in `https://{resource}.openai.azure.com`. Consulted only
    /// for [`ApiType::Azure`]; falls back to the `AZURE_OPENAI_RESOURCE`
    /// environment variable when `None`.
    pub azure_resource: Option<String>,

    /// Request-level thinking budget for reasoning-capable models.
    ///
    /// One of `low`, `medium`, `high`, or `max`, mapped at provider
    /// construction onto the family's native dial — the Anthropic
    /// `thinking.budget_tokens` ladder, the OpenAI `reasoning_effort`
    /// parameter, the Gemini `thinkingConfig.thinkingBudget` budget.
    /// Unset (the default) sends byte-identical requests
    /// and the provider's own thinking defaults apply; an unrecognized
    /// value fails client construction rather than silently defaulting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// Whether Anthropic-protocol requests carry prompt-cache breakpoints.
    ///
    /// When `true` (the default), requests to `[api] api_type` `anthropic`
    /// and `zai` mark the provider's prompt-cache breakpoints (tools,
    /// system, and the trailing message blocks), so the stable
    /// conversation prefix is served from cache across a session's turns
    /// instead of re-processed every request. Set `false` for an
    /// Anthropic-compatible endpoint that rejects the extra field.
    /// OpenAI-protocol providers cache automatically server-side and
    /// ignore this setting on the wire; the cached share is reported in
    /// usage either way. Loaded from `[api] prompt_cache`.
    pub prompt_cache: bool,
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

    /// Where the TUI's activity strip renders while the agent runs.
    ///
    /// `composer` (the default) places the breathing bar in the spacer row
    /// directly above the prompt box; `top` gives it a dedicated first row
    /// of the terminal, above the conversation pane. See
    /// [`StripPosition`].
    pub strip_position: StripPosition,

    /// The animation the TUI's activity strip plays.
    ///
    /// `breathe` (the default) grows from the row's center out to
    /// both edges and shrinks back; `sweep` fills from the left edge
    /// to the right end and then clears left-to-right. See
    /// [`StripStyle`].
    pub strip_style: StripStyle,

    /// Let the TUI capture the mouse.
    ///
    /// Capture is what lets the wheel scroll the conversation, and it is
    /// also what takes click-drag text selection away from the terminal —
    /// the two are mutually exclusive, and many terminals offer no
    /// modifier bypass. Defaults to `true`: the wheel scrolls the
    /// conversation. Set `false` to hand selection back to the terminal
    /// — the wheel then reaches the app only where the terminal
    /// supports alternate-scroll translation, and keyboard scrolling
    /// (PageUp/PageDown, arrows on an empty editor) covers the rest.
    pub mouse_capture: bool,

    /// How the TUI lays out a file edit's diff in the conversation
    /// pane.
    ///
    /// `inline` (the default) stacks removals above insertions as
    /// full-width solid bands; `side-by-side` pairs each removed line
    /// with its replacement across a center gutter. Narrow terminals
    /// fall back to the inline shape whatever the setting. See
    /// [`DiffLayout`].
    pub diff_layout: DiffLayout,

    /// A custom clipboard command for the TUI's selection copy.
    ///
    /// When set, a completed selection's text is piped to this command's
    /// stdin — run through `sh -c`, so arguments and pipes work — in
    /// addition to the built-in transports: the terminal-side OSC 52
    /// escape and whatever host clipboard tools the environment names
    /// (`tmux set-buffer`, `wl-copy`, `xclip`, `xsel`, `pbcopy`). Set it
    /// where none of the built-ins reach, e.g. a remote session whose
    /// local terminal ignores OSC 52. Defaults to unset.
    pub copy_command: Option<String>,
}

/// How the TUI renders a file edit's diff in the conversation pane.
///
/// Both layouts show the same hunks with the same real line numbers;
/// the choice is spatial. A side-by-side view pairs each removed line
/// with the line that replaced it across a center gutter, which reads
/// fastest on wide terminals; an inline view stacks removals above
/// insertions, which fits narrow terminals and unified-diff habits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffLayout {
    /// Two panes across one row: the old line beside its replacement.
    ///
    /// A replacement reads as one glance. Terminals narrower than the
    /// two-pane floor fall back to the inline shape rather than
    /// squeeze the panes unreadably, so the choice degrades
    /// gracefully on split screens.
    SideBySide,

    /// One pane: removals stack above the insertions that replaced
    /// them, in unified-diff order.
    ///
    /// The default: full-width solid bands under bright text —
    /// the shape an edit reads fastest in.
    #[default]
    Inline,
}

/// Which compaction strategy an automatically compacting session runs.
///
/// The runner installs the selected compactor when it assembles the loop,
/// so every run mode compacts the same way from the same knob. The
/// summarizer kinds spend model calls to keep the evicted conversation's
/// facts reachable; truncation spends none and simply drops the oldest
/// messages; rolling spends small calls continuously so no single pass
/// grows large.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactorSelection {
    /// Drop the oldest messages, preserving the recent tail verbatim.
    ///
    /// The zero-model-call strategy: no summarization prompts run, and the
    /// information in the dropped messages is gone from the feed.
    Truncating,

    /// Summarize through a question/answer pass over the session's model.
    ///
    /// The dropped portion is summarized, the model is asked what the
    /// summary is missing, and the answers are folded back in — decisions
    /// and identifiers survive the pass instead of evaporating.
    Qa,

    /// Summarize into fixed sections through the session's model.
    ///
    /// The dropped portion becomes a structured summary with key facts,
    /// decisions, pending tasks, and open questions as separate sections.
    Structured,

    /// Age turns into micro-summaries while the session runs.
    ///
    /// Each pass summarizes only the turns that have aged past the
    /// rolling threshold — bounded input and output per call, so the
    /// pass costs seconds instead of minutes — and the question/answer
    /// chain stands behind it as the emergency floor for growth the
    /// aging cannot keep up with.
    Rolling,
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

    /// Whole-request streaming budget for one model call, in seconds.
    ///
    /// The wall clock a single streaming model call may consume before the
    /// runner abandons it — prompt processing plus the model's entire
    /// generation, reasoning included. The prompt is provider-cached between
    /// calls, so the budget is spent almost entirely on *generation*: a
    /// reasoning-heavy model on a CPU backend can emit thousands of tokens
    /// in one call, and the 300-second default kills such calls
    /// mid-generation; raise it for slow local backends. Two bounds shape
    /// the honest range: values below 120 fail the load (the stream handler
    /// floors the total budget at its 120-second first-event window, so a
    /// lower ask would be silently replaced), and the wait for the *first*
    /// token is always capped at that same 120-second window — a backend
    /// whose first token takes longer fails the attempt there whatever this
    /// budget says. Defaults to `300`. Loaded from
    /// `[runner] turn_timeout_secs`.
    pub turn_timeout_secs: u64,

    /// Whether to auto-compact the conversation when it grows large.
    ///
    /// When true, the session compacts itself once it crosses
    /// [`RunnerConfig::compact_threshold`]. Defaults to `true`.
    pub auto_compact: bool,

    /// Compaction threshold as a percentage (0–100) of the context window.
    ///
    /// Defaults to `80`. Matches the `u8` percentage loopctl's
    /// `SessionConfig` expects, so the value passes through
    /// `to_session_config` with no conversion; that conversion clamps values
    /// above 100 to 100. The meaningful range is `0..=100`.
    pub compact_threshold: u8,

    /// Which compaction strategy automatic compaction runs.
    ///
    /// Selects the compactor the runner installs for the session: `qa`
    /// (the default) runs the question/answer summarizer, `structured`
    /// the sectioned summarizer, `rolling` the bounded aging pass, and
    /// `truncating` the plain oldest-drop that spends no model calls.
    /// The selection is strict — no other compactor ever runs in the
    /// chosen one's place: a stage whose model call fails fails its
    /// pass, the engine defers while the payload fits the window, and
    /// the next turn retries the same compactor. Loaded from
    /// `[runner] compactor`.
    pub compactor: CompactorSelection,

    /// When to prompt the user before side-effecting actions.
    ///
    /// Controls how autonomously the agent may act. Defaults to
    /// [`PermissionMode::Auto`]. See [`PermissionMode`].
    pub permission_mode: PermissionMode,

    /// Whether independent tool calls in one response dispatch
    /// concurrently.
    ///
    /// When `true` (the default), a response carrying several tool calls
    /// runs its concurrency-safe subset at once — the engine's wave
    /// planner still serializes every call that is not safe to overlap,
    /// so turning this off only trades throughput for strictly
    /// one-at-a-time execution. Loaded from `[runner]
    /// parallel_tool_dispatch`; a section without the key keeps the
    /// default via the container-level serde default, which reads this
    /// struct's own [`Default`] impl rather than the type's.
    pub parallel_tool_dispatch: bool,

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

    /// Whether file tools may reach paths outside the working directory.
    ///
    /// `false` (the default) keeps every file-touching tool confined to the
    /// working directory — traversal, unrelated absolute paths, and escaping
    /// symlinks are rejected. `true` lifts that containment so system-file
    /// workflows work (`/etc/nginx`, `/home/you/.ssh`, `/var/log`); paths
    /// are used verbatim, so a leading `~` is not expanded. The CLI flag
    /// `--unsafe-paths` forces it on for one run. It never crosses into the
    /// session/run configuration — path containment is a tool-layer concern.
    #[serde(default)]
    pub unsafe_paths: bool,

    /// Line ceiling one `Read` (or one file inside a `ReadFiles` batch)
    /// may return.
    ///
    /// A file longer than this is cut with a marker naming the continuation
    /// offset, so the model pages the remainder. The default of `1200`
    /// covers a long documentation file in one call; raising it trades
    /// context spend per read for fewer paging round-trips, lowering it
    /// forces more, smaller windows. Values below 1 are floored to 1 by
    /// the underlying tool builder. Loaded from `[runner] read_max_lines`.
    pub read_max_lines: usize,

    /// Byte ceiling on the joined, line-numbered output of one read.
    ///
    /// The real bound on a single file view — a single minified line or a
    /// full window of wide lines cuts here even under the line ceiling,
    /// on the last complete line, with the continuation offset in the
    /// marker. The default is `49_152` bytes, which keeps one file view
    /// inside [`max_tool_output_chars`](Self::max_tool_output_chars) so the
    /// read's own continuation-naming marker is the one that fires. Values
    /// below 1 are floored to 1 by the underlying tool builder. Loaded from
    /// `[runner] read_max_bytes`.
    pub read_max_bytes: usize,

    /// Character ceiling on one dispatched tool result.
    ///
    /// The conversation-level bound a tool cannot exceed: installed as the
    /// dispatch pipeline's outermost cap, so no single result can spend more
    /// of the model's context than this whatever the tool returned. A result
    /// over the ceiling keeps its leading characters plus a `[truncated]`
    /// marker, so a cut reads as a cut rather than passing as the tool's
    /// whole output. `0` disables the cap, which also lifts the ordering
    /// requirement on
    /// [`read_max_bytes`](Self::read_max_bytes). Loaded from `[runner]
    /// max_tool_output_chars`.
    pub max_tool_output_chars: usize,
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

    /// The replacement prose.
    ///
    /// Used verbatim in place of [`Role::system_prompt`] for this role. The
    /// shared agent discipline, detected tech profile, and per-tool fragments
    /// still append — only the role-specific body is replaced. There is no
    /// length limit; the caller composes the full prompt from this string plus
    /// the other parts.
    pub prompt: String,
}

impl RunnerConfig {
    /// Look up a user override for `role`, if any.
    ///
    /// Returns the override's prompt when an entry for `role` exists in
    /// [`Self::role_overrides`], otherwise `None` (meaning: use the role's
    /// built-in [`Role::system_prompt`]). When multiple entries share the
    /// same role, the first declaration wins.
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
/// [`ProjectConfig`] holds a `Vec<TechProfile>` for all of them.
///
/// Every field except `language` is optional: set only what detection got
/// wrong or can't infer. Loaded from the `[[project.techs]]` array-of-tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TechProfile {
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

    /// Free-form conventions for this language.
    ///
    /// Style rules, module layout, anything detection can't capture. Appended
    /// to the prompt verbatim under this language's section. `None` keeps the
    /// detected value.
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
    pub techs: Vec<TechProfile>,

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

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            base_url: String::new(),
            api_type: ApiType::default(),
            api_key: None,
            max_tokens: 32_000,
            context_window: None,
            request_timeout_secs: 120,
            fallback_model: None,
            azure_resource: None,
            reasoning_effort: None,
            prompt_cache: true,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            no_color: false,
            verbosity: Verbosity::default(),
            theme: "default".to_string(),
            strip_position: StripPosition::default(),
            strip_style: StripStyle::default(),
            diff_layout: DiffLayout::default(),
            mouse_capture: true,
            copy_command: None,
        }
    }
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            max_turns: 200,
            turn_timeout_secs: 300,
            auto_compact: true,
            compact_threshold: 80,
            compactor: CompactorSelection::Qa,
            permission_mode: PermissionMode::default(),
            parallel_tool_dispatch: true,
            role: Role::default(),
            role_overrides: Vec::new(),
            unsafe_paths: false,
            read_max_lines: 1200,
            read_max_bytes: DEFAULT_READ_MAX_BYTES,
            max_tool_output_chars: DEFAULT_MAX_TOOL_OUTPUT_CHARS,
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

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
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

    /// Parse configuration from TOML text, rejecting values the runtime
    /// would silently rewrite.
    ///
    /// The one entry from configuration text to a usable config: parsing
    /// and validation are a single operation, so no loader can deserialize
    /// a file and then forget to check it. Every route to a config — the
    /// default directory search, an explicit `--config` file — arrives
    /// here. The defaults and every accepted value pass through untouched.
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
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Reject values whose silently-substituted behavior would contradict
    /// the documented contract.
    ///
    /// Runs after deserialization on every load, so a value the runtime
    /// would quietly rewrite surfaces as a startup error naming the field,
    /// the value, and the bound it violated. Reached only through
    /// [`from_toml_str`](Self::from_toml_str), which is what makes the
    /// guarantee hold on every route a file takes to reach the runner.
    ///
    /// # Errors
    ///
    /// Returns [`DchConfigError::Invalid`] when a field sits below the floor
    /// its consumer enforces, or when a read ceiling is set above the
    /// dispatch output cap.
    fn validate(&self) -> Result<(), DchConfigError> {
        if self.runner.turn_timeout_secs < TURN_TIMEOUT_FLOOR_SECS {
            return Err(DchConfigError::Invalid(format!(
                "turn_timeout_secs = {} is below the {}-second floor: the stream handler \
                 replaces a smaller whole-stream budget with its first-event window, so the \
                 configured value would never run",
                self.runner.turn_timeout_secs, TURN_TIMEOUT_FLOOR_SECS
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

/// The smallest accepted `[runner] turn_timeout_secs` value, in seconds.
///
/// The stream handler floors a whole-stream budget at its first-event
/// window of this length, so a configured total below the bound would be
/// silently replaced rather than honored; the loader rejects it instead.
const TURN_TIMEOUT_FLOOR_SECS: u64 = 120;

/// Default byte ceiling on one read's joined output.
///
/// Sized to stay under [`DEFAULT_MAX_TOOL_OUTPUT_CHARS`], so a whole file
/// view reaches the model carrying the read tool's own continuation marker
/// rather than being cut by the dispatch cap's generic one.
const DEFAULT_READ_MAX_BYTES: usize = 49_152;

/// Default character ceiling on one dispatched tool result.
///
/// Roughly a sixteenth of a mid-sized context window: large enough that a
/// legitimate documentation read lands whole, small enough that one flooded
/// call cannot spend a model's whole conversation.
const DEFAULT_MAX_TOOL_OUTPUT_CHARS: usize = 65_536;

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

    #[test]
    fn display_defaults_to_capturing_the_wheel() {
        let display = DisplayConfig::default();
        assert!(
            display.mouse_capture,
            "the wheel scrolls the conversation by default"
        );
    }

    /// Collect the key names a serialized value carries, descending
    /// tables and arrays of tables.
    ///
    /// Sections, their fields, and the fields inside each array entry
    /// all count as knobs, so a nested field the surface walk never
    /// saw is demanded like any other.
    fn collect_knobs(value: &serde_json::Value, names: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(table) => {
                for (key, inner) in table {
                    names.push(key.clone());
                    collect_knobs(inner, names);
                }
            }
            serde_json::Value::Array(entries) => {
                for entry in entries {
                    collect_knobs(entry, names);
                }
            }
            _ => {}
        }
    }

    /// Whether the sample spells `name` as a configuration key.
    ///
    /// A line counts only where the knob appears in one of TOML's key
    /// spellings — `name =`, a `[section]` header, or an
    /// `[[array.entry]]` header — commented or not. A prose mention
    /// satisfies none of them, which is the teeth the sweep exists
    /// for.
    fn sample_documents(sample: &str, name: &str) -> bool {
        sample.lines().any(|line| {
            let trimmed = line.trim();
            let spelled = trimmed.strip_prefix("# ").unwrap_or(trimmed);
            if let Some(header) = spelled.strip_prefix('[') {
                return header
                    .trim_end_matches(']')
                    .trim_matches(['[', ']'])
                    .split('.')
                    .any(|segment| segment == name);
            }
            spelled.starts_with(&format!("{name} =")) || spelled.starts_with(&format!("{name}="))
        })
    }

    /// Knobs of `config` the sample never spells as a key.
    ///
    /// The report drives the example-config sweep: an empty list is
    /// the passing verdict, and each entry names a knob whose
    /// documentation is missing or merely prose.
    fn undocumented_knobs(sample: &str, config: &DchConfig) -> Vec<String> {
        let mut names = Vec::new();
        collect_knobs(
            &serde_json::to_value(config).expect("the config serializes"),
            &mut names,
        );
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter(|name| !sample_documents(sample, name))
            .collect()
    }

    /// The default config plus one role override and one technology.
    ///
    /// The default's nested tables are empty, so a walk of it sees
    /// `role_overrides` but never the fields inside an entry; the
    /// fixture contributes those names mechanically.
    fn config_with_nested_entries() -> DchConfig {
        let mut config = DchConfig::default();
        config.runner.role_overrides.push(RoleOverride {
            role: Role::Coding,
            prompt: "plan the work".to_string(),
        });
        config.project.techs.push(TechProfile {
            language: "rust".to_string(),
            build: Some("cargo build".to_string()),
            test: None,
            lint: None,
            conventions: None,
        });
        config
    }

    #[test]
    fn from_toml_str_rejects_a_turn_timeout_below_the_floor() {
        let err = DchConfig::from_toml_str("[runner]\nturn_timeout_secs = 119\n").unwrap_err();
        assert!(
            matches!(
                &err,
                DchConfigError::Invalid(message)
                    if message.contains("turn_timeout_secs = 119") && message.contains("120")
            ),
            "the rejection names the value and the floor: {err:?}"
        );
    }

    #[test]
    fn from_toml_str_loads_a_turn_timeout_at_the_floor() {
        let config = DchConfig::from_toml_str("[runner]\nturn_timeout_secs = 120\n").unwrap();
        assert_eq!(
            config.runner.turn_timeout_secs, 120,
            "the floor itself is an accepted budget"
        );
    }

    #[test]
    fn the_output_cap_defaults_and_round_trips() {
        assert_eq!(
            DchConfig::default().runner.max_tool_output_chars,
            DEFAULT_MAX_TOOL_OUTPUT_CHARS,
            "the shipped default is the documented ceiling on one tool result"
        );
        let configured = DchConfig::from_toml_str("[runner]\nmax_tool_output_chars = 131072\n")
            .expect("a configured cap loads");
        assert_eq!(configured.runner.max_tool_output_chars, 131_072);
        let disabled =
            DchConfig::from_toml_str("[runner]\nmax_tool_output_chars = 0\n").expect("0 parses");
        assert_eq!(
            disabled.runner.max_tool_output_chars, 0,
            "0 is the documented disable sentinel, not a rejected value"
        );
    }

    #[test]
    fn a_per_file_read_ceiling_above_the_output_cap_fails_the_load() {
        let err = DchConfig::from_toml_str(
            "[runner]\nread_max_bytes = 65537\nmax_tool_output_chars = 65536\n",
        )
        .unwrap_err();
        assert!(
            matches!(
                &err,
                DchConfigError::Invalid(message)
                    if message.contains("read_max_bytes = 65537")
                        && message.contains("65536")
            ),
            "the rejection names the value and the cap it contradicts: {err:?}"
        );
    }

    #[test]
    fn a_per_file_read_ceiling_at_the_output_cap_loads() {
        let config = DchConfig::from_toml_str(
            "[runner]\nread_max_bytes = 4096\nmax_tool_output_chars = 4096\n",
        )
        .expect("a read ceiling exactly at the cap is coherent");
        assert_eq!(config.runner.read_max_bytes, 4096);
        assert_eq!(
            config.runner.max_tool_output_chars, 4096,
            "the rule orders the ceilings, it does not ban a whole-file view \
             that fills one result exactly"
        );
    }

    #[test]
    fn a_zero_output_cap_skips_the_coherence_check() {
        let config = DchConfig::from_toml_str(
            "[runner]\nread_max_bytes = 4194304\nmax_tool_output_chars = 0\n",
        )
        .expect("with the cap off there is nothing to contradict");
        assert_eq!(config.runner.read_max_bytes, 4_194_304);
        assert_eq!(
            config.runner.max_tool_output_chars, 0,
            "a disabled cap imposes no ordering on the read ceiling"
        );
    }

    #[test]
    fn the_default_read_byte_ceiling_sits_under_the_output_cap() {
        assert_eq!(
            (DEFAULT_READ_MAX_BYTES, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
            (49152, 65536),
            "the shipped default pair keeps the byte ceiling under the \
             output cap the coherence check protects"
        );
        let config = DchConfig::default();
        assert!(
            config.runner.read_max_bytes <= config.runner.max_tool_output_chars,
            "the default config itself satisfies the rule it enforces on users"
        );
    }

    #[test]
    fn the_knob_walk_descends_into_nested_tables() {
        let fixture = config_with_nested_entries();
        let without_nested = concat!(
            "# [[runner.role_overrides]]\n",
            "# role = \"coding\"\n",
            "# [[project.techs]]\n",
            "# language = \"rust\"\n",
        );
        let report = undocumented_knobs(without_nested, &fixture);
        for nested in ["prompt", "build", "test", "lint", "conventions"] {
            assert!(
                report.iter().any(|name| name == nested),
                "the walk demands the nested `{nested}` a flat sweep never saw: {report:?}"
            );
        }
        let with_nested = concat!(
            "# [[runner.role_overrides]]\n",
            "# role = \"coding\"\n",
            "# prompt = \"plan the work\"\n",
            "# [[project.techs]]\n",
            "# language = \"rust\"\n",
            "# build = \"cargo build\"\n",
            "# test = \"cargo test\"\n",
            "# lint = \"cargo clippy\"\n",
            "# conventions = \"none\"\n",
        );
        let report = undocumented_knobs(with_nested, &fixture);
        for nested in ["prompt", "build", "test", "lint", "conventions"] {
            assert!(
                !report.iter().any(|name| name == nested),
                "a key-shaped `{nested} =` line documents the nested knob: {report:?}"
            );
        }
    }

    #[test]
    fn a_prose_mention_does_not_count_as_documenting_a_knob() {
        let prose = "# raise compact_threshold when sessions run long\n";
        let report = undocumented_knobs(prose, &DchConfig::default());
        assert!(
            report.iter().any(|name| name == "compact_threshold"),
            "a prose mention is not a key-shaped spelling: {report:?}"
        );
        let keyed = "# compact_threshold = 80\n";
        let report = undocumented_knobs(keyed, &DchConfig::default());
        assert!(
            !report.iter().any(|name| name == "compact_threshold"),
            "the key-shaped spelling documents the knob: {report:?}"
        );
    }

    #[test]
    fn the_example_config_documents_every_configured_knob() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example.config");
        let sample = std::fs::read_to_string(path).expect("example.config is readable");
        toml::from_str::<DchConfig>(&sample).expect("example.config parses as a config");
        let report = undocumented_knobs(&sample, &config_with_nested_entries());
        assert!(
            report.is_empty(),
            "every knob the walk can see must appear in a key-shaped spelling: {report:?}"
        );
        // reasoning_effort skip-serializes when unset, so the walk
        // cannot see it; its presence is asserted by name.
        assert!(
            sample_documents(&sample, "reasoning_effort"),
            "example.config must document the `reasoning_effort` knob"
        );
    }

    #[test]
    fn strip_style_parses_with_a_breathe_default() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Breathe,
            "a config without the field keeps the breathing animation"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "sweep""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Sweep,
            "the left-anchored fill parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "bounce""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Bounce,
            "the two-way sweep parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "travel""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Travel,
            "the gliding scanner parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "snake""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Snake,
            "the circulating segment parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "worm""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Worm,
            "the inchworm crawl parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "worm_long""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::WormLong,
            "the full-row worm parses from its hyphenated name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "off""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Off,
            "the absent strip parses from its lowercase name"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_style = "glow""#).unwrap();
        assert_eq!(
            display.strip_style,
            StripStyle::Glow,
            "the breathing static bar parses from its lowercase name"
        );
        assert!(
            toml::from_str::<DisplayConfig>(r#"strip_style = "spin""#).is_err(),
            "an unknown style fails the load instead of falling back"
        );
    }

    #[test]
    fn strip_position_parses_with_a_composer_default() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(
            display.strip_position,
            StripPosition::Composer,
            "a config without the field keeps the composer placement"
        );
        let display: DisplayConfig = toml::from_str(r#"strip_position = "top""#).unwrap();
        assert_eq!(
            display.strip_position,
            StripPosition::Top,
            "the dedicated top row parses from its lowercase name"
        );
        assert!(
            toml::from_str::<DisplayConfig>(r#"strip_position = "middle""#).is_err(),
            "an unknown placement fails the load instead of falling back"
        );
    }

    #[test]
    fn a_config_without_the_mouse_field_parses_to_capture() {
        let display: DisplayConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert!(display.mouse_capture);
        assert_eq!(display.theme, "dracula");
    }

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
        assert_eq!(c.runner.turn_timeout_secs, 300);
        assert_eq!(c.runner.compact_threshold, 80);
        assert_eq!(c.runner.compactor, CompactorSelection::Qa);
        assert_eq!(c.runner.permission_mode, PermissionMode::Auto);
        assert_eq!(c.telemetry.level, "info");
        assert!(c.security.redact_secrets);
    }

    #[test]
    fn the_default_diff_layout_is_inline_and_every_spelling_loads() {
        assert_eq!(
            DisplayConfig::default().diff_layout,
            DiffLayout::Inline,
            "a config that never names a layout runs the inline bands"
        );
        for (spelling, expected) in [
            ("side-by-side", DiffLayout::SideBySide),
            ("inline", DiffLayout::Inline),
        ] {
            let parsed: DisplayConfig =
                toml::from_str(&format!("diff_layout = \"{spelling}\"")).unwrap();
            assert_eq!(
                parsed.diff_layout, expected,
                "the {spelling} spelling loads"
            );
        }
        assert!(
            toml::from_str::<DisplayConfig>("diff_layout = \"diagonal\"").is_err(),
            "an unknown layout is a config error, not a silent default"
        );
    }

    #[test]
    fn the_default_compactor_is_qa_and_every_spelling_loads() {
        assert_eq!(
            RunnerConfig::default().compactor,
            CompactorSelection::Qa,
            "a config that never names a compactor runs the qa summarizer"
        );
        for (spelling, expected) in [
            ("truncating", CompactorSelection::Truncating),
            ("qa", CompactorSelection::Qa),
            ("structured", CompactorSelection::Structured),
            ("rolling", CompactorSelection::Rolling),
        ] {
            let parsed: RunnerConfig =
                toml::from_str(&format!("compactor = \"{spelling}\"")).unwrap();
            assert_eq!(parsed.compactor, expected);
        }
        assert!(
            toml::from_str::<RunnerConfig>("compactor = \"beam\"").is_err(),
            "an unknown spelling fails the load instead of silently compacting another way"
        );
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
    fn a_turn_timeout_below_the_first_event_floor_fails_the_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(
            tmp.path(),
            "config.toml",
            "[runner]\nturn_timeout_secs = 119\n",
        );
        let err = DchConfig::load_from_dir(tmp.path()).unwrap_err();
        assert!(
            matches!(
                &err,
                DchConfigError::Invalid(message)
                    if message.contains("turn_timeout_secs = 119") && message.contains("120")
            ),
            "the rejection names the value and the floor: {err:?}"
        );
    }

    #[test]
    fn a_turn_timeout_at_the_first_event_floor_loads() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(
            tmp.path(),
            "config.toml",
            "[runner]\nturn_timeout_secs = 120\n",
        );
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        assert_eq!(
            c.runner.turn_timeout_secs, 120,
            "the floor itself is an accepted budget"
        );
    }

    #[test]
    fn present_but_empty_mcp_section_parses_as_no_servers() {
        let c: DchConfig = toml::from_str("[mcp]\n").unwrap();
        assert!(c.mcp.servers.is_empty());
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
    fn unknown_runner_keys_still_load() {
        let c: DchConfig = toml::from_str("[runner]\nmystery_knob = \"x\"\n").unwrap();
        assert_eq!(c.runner.max_turns, 200);
    }

    #[test]
    fn a_runner_section_without_the_dispatch_key_stays_parallel() {
        let c: DchConfig = toml::from_str("[runner]\nmax_turns = 50\n").unwrap();
        assert!(
            c.runner.parallel_tool_dispatch,
            "a missing key must keep the default, not fall to the bool default"
        );
    }

    #[test]
    fn every_variant_round_trips_through_its_lowercase_name() {
        // The literals are the guard on the persisted on-disk format: a
        // variant rename or a rename_all change breaks a literal here,
        // not just the derived-name comparison in the same loop.
        let wire_names = [
            (ApiType::OpenAi, "openai"),
            (ApiType::Anthropic, "anthropic"),
            (ApiType::Gemini, "gemini"),
            (ApiType::Ollama, "ollama"),
            (ApiType::DeepSeek, "deepseek"),
            (ApiType::Grok, "grok"),
            (ApiType::Zai, "zai"),
            (ApiType::Azure, "azure"),
            (ApiType::Moonshot, "moonshot"),
            (ApiType::Bedrock, "bedrock"),
        ];
        for (variant, name) in wire_names {
            let parsed: ApiConfig = toml::from_str(&format!("api_type = \"{name}\"\n")).unwrap();
            assert_eq!(
                parsed.api_type, variant,
                "wire name {name} must keep mapping to {variant:?}"
            );
            assert_eq!(
                format!("{variant:?}").to_lowercase(),
                name,
                "{variant:?} must derive its literal wire name"
            );
            let source = ApiConfig {
                api_type: variant,
                ..ApiConfig::default()
            };
            let written = toml::to_string(&source).unwrap();
            let reparsed: ApiConfig = toml::from_str(&written).unwrap();
            assert_eq!(
                reparsed, source,
                "{variant:?} must survive a serialize/deserialize round trip \
                 whole — an unset Option coming back as an empty string would \
                 turn a missing key into a present-but-empty credential"
            );
        }
    }

    #[test]
    fn default_base_url_pins_stock_hosts_and_defers_every_other_variant() {
        // A reintroduced hard-coded host here would silently shadow the
        // endpoint its provider profile or AWS region derives; iterating
        // ALL keeps a future variant from escaping the guard.
        let stock = [
            (ApiType::OpenAi, "https://api.openai.com/v1"),
            (ApiType::Anthropic, "https://api.anthropic.com"),
            (ApiType::Gemini, "https://generativelanguage.googleapis.com"),
        ];
        for api_type in ApiType::ALL {
            match stock.iter().find(|(variant, _)| *variant == api_type) {
                Some((_, host)) => assert_eq!(
                    api_type.default_base_url(),
                    Some(*host),
                    "{api_type:?} must keep its stock host pinned"
                ),
                None => assert_eq!(
                    api_type.default_base_url(),
                    None,
                    "{api_type:?} must defer to its profile- or region-derived endpoint"
                ),
            }
        }
        for (variant, _) in stock {
            assert!(
                ApiType::ALL.contains(&variant),
                "{variant:?} must stay in ALL for its host pin to run"
            );
        }
    }

    #[test]
    fn api_type_all_lists_each_variant_once() {
        // Only the duplicate half is checkable from here: a variant
        // missing from ALL must be caught by the change that adds it —
        // the enum's exhaustive matches are the enforcement point.
        let mut seen = std::collections::HashSet::new();
        for api_type in ApiType::ALL {
            assert!(
                seen.insert(api_type),
                "duplicate variant in ALL: {api_type:?}"
            );
        }
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
    fn role_all_covers_every_variant() {
        let mut seen = std::collections::HashSet::new();
        for role in Role::ALL {
            assert!(seen.insert(role), "duplicate variant in ALL: {role:?}");
        }
        assert_eq!(
            seen.len(),
            7,
            "ALL must contain all 7 variants — add new ones here"
        );
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

    #[test]
    fn unsafe_paths_defaults_to_contained() {
        // The safe-by-default contract: absent from the config, containment
        // stays on.
        assert!(!DchConfig::default().runner.unsafe_paths);
    }

    #[test]
    fn unsafe_paths_parses_from_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_config(tmp.path(), "config.toml", "[runner]\nunsafe_paths = true\n");
        let c = DchConfig::load_from_dir(tmp.path()).unwrap();
        assert!(
            c.runner.unsafe_paths,
            "the opt-out must parse through serde"
        );
    }

    #[test]
    fn permission_mode_names_match_the_config_spelling() {
        assert_eq!(PermissionMode::Auto.as_str(), "auto");
        assert_eq!(PermissionMode::Plan.as_str(), "plan");
        assert_eq!(PermissionMode::AcceptEdits.as_str(), "accept_edits");
        assert_eq!(PermissionMode::Interactive.as_str(), "interactive");
    }
}
