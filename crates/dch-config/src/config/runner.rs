//! The `[runner]` section: the per-run and per-session execution
//! knobs.
//!
//! [`RunnerConfig`] carries the turn budget, compaction choice,
//! permission mode, and tool ceilings; [`Role`] with its prose bodies
//! is the `role` knob's vocabulary.

use crate::DchConfigError;
use crate::PermissionMode;

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
    /// caught by the `role_all_covers_every_variant` test.
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

    /// Seconds to wait before the first rate-limit retry, and the floor
    /// the retry ladder doubles from.
    ///
    /// The stream handler meets a 429/503 two ways: a server-sent
    /// `Retry-After` is honoured when present, and a hintless hit — the
    /// shape quota-windowed providers like GLM send — sleeps this long
    /// first, then doubles per consecutive hit up to the ladder's cap.
    /// The stock 5-second floor is tuned for transient bursts, not for
    /// quota windows that run minutes: a hintless ladder floors at
    /// 5 + 10 + 20 before escalating, so a plan throttled for a minute
    /// dies while the window is still open. Fifteen turns the same
    /// ladder into 15 + 30 + 60, long enough to outlast the shorter
    /// windows without stretching a transient blip past notice.
    /// Values below `1` fail the load: a zero floor would retry a
    /// throttled endpoint immediately. Loaded from
    /// `[runner] rate_limit_delay_secs`.
    pub rate_limit_delay_secs: u64,

    /// Smallest byte window the repetition guard treats as a loop period.
    ///
    /// The stream repetition guard aborts a model call whose text ends in
    /// one byte window repeated verbatim four times running. The library
    /// floors the window at 48 bytes, and a byte-level window cannot tell
    /// the horizontal border of an ASCII-art box from a loop: the border
    /// is one character repeated, a box-drawing horizontal is three bytes
    /// in UTF-8, and sixty-four of them are exactly four 48-byte windows.
    /// The default floors the window at 384 bytes instead: four windows
    /// hold 512 such characters, so a border would have to be 512 wide to
    /// trip, while a genuine stanza loop still trips, after about 1.5
    /// `KiB` of output rather than 192 bytes. `0` disarms the guard; any
    /// other value below the library's 48-byte floor or above its
    /// 4096-byte period cap fails the load. Defaults to `384`. Loaded from
    /// `[runner] repetition_min_period_bytes`.
    pub repetition_min_period_bytes: usize,

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
    /// [`crate::DchConfig::to_session_config`], because composing the
    /// prompt needs the tool registry and the repo root, which the
    /// config layer does not own.
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

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            max_turns: 200,
            turn_timeout_secs: 300,
            rate_limit_delay_secs: 15,
            repetition_min_period_bytes: 384,
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

/// The smallest accepted `[runner] turn_timeout_secs` value, in seconds.
///
/// Public because the write-through gates mirror it: the composer's
/// `/set` refuses a value below the floor with the same number the
/// loader enforces, so the two checks can never disagree about the
/// bound.
///
/// The stream handler floors a whole-stream budget at its first-event
/// window of this length, so a configured total below the bound would be
/// silently replaced rather than honored; the loader rejects it instead.
pub const TURN_TIMEOUT_FLOOR_SECS: u64 = 120;
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

/// The range check on `repetition_min_period_bytes`, shared by the
/// loader and the runner's builder.
///
/// `0` disarms the guard and stands. Any other value must sit within the
/// library's window — at or above its period floor, since four copies of
/// a shorter window fit inside ordinary text and the guard would abort
/// legitimate generation, and at or below its period cap, since the
/// guard never scans a window that long and would silently never fire.
///
/// # Errors
///
/// Returns [`DchConfigError::Invalid`] naming the value and the bound it
/// violates.
pub fn check_repetition_min_period_bytes(period: usize) -> Result<(), DchConfigError> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::write_config;
    use crate::{DchConfig, DchConfigError, PermissionMode};

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
    fn the_default_rate_limit_floor_outlasts_a_short_quota_window() {
        // The knob's default is the lever the ladder needs: fifteen
        // seconds floors the hintless ramp at 15 + 30 + 60, where the
        // stock five floors it at 5 + 10 + 20 — the difference between
        // riding out a short quota window and escalating inside it.
        assert_eq!(
            DchConfig::default().runner.rate_limit_delay_secs,
            15,
            "the default floor is fifteen seconds"
        );
    }

    #[test]
    fn from_toml_str_rejects_an_immediate_rate_limit_retry() {
        let err = DchConfig::from_toml_str("[runner]\nrate_limit_delay_secs = 0\n").unwrap_err();
        assert!(
            matches!(
                &err,
                DchConfigError::Invalid(message)
                    if message.contains("rate_limit_delay_secs = 0")
                        && message.contains("1-second floor")
            ),
            "the rejection names the value and the floor: {err:?}"
        );
    }

    #[test]
    fn from_toml_str_loads_a_tuned_rate_limit_floor() {
        let config = DchConfig::from_toml_str("[runner]\nrate_limit_delay_secs = 30\n").unwrap();
        assert_eq!(
            config.runner.rate_limit_delay_secs, 30,
            "a tuned floor loads verbatim — the knob the quota window asks for"
        );
    }

    #[test]
    fn the_default_repetition_floor_is_well_above_an_ascii_art_border() {
        // Sixty-four box-drawing horizontals are 192 bytes — four of the
        // library's 48-byte windows — so the library floor reads a border
        // as a loop; four default dch windows hold 512 such glyphs.
        assert_eq!(
            DchConfig::default().runner.repetition_min_period_bytes,
            384,
            "the default floor is 384 bytes"
        );
    }

    #[test]
    fn from_toml_str_rejects_a_repetition_floor_below_the_library_minimum() {
        let err =
            DchConfig::from_toml_str("[runner]\nrepetition_min_period_bytes = 47\n").unwrap_err();
        assert!(
            matches!(
                &err,
                DchConfigError::Invalid(message)
                    if message.contains("repetition_min_period_bytes = 47")
                        && message.contains("48-byte floor")
            ),
            "the rejection names the value and the floor: {err:?}"
        );
    }

    #[test]
    fn from_toml_str_rejects_a_repetition_floor_above_the_period_cap() {
        let err =
            DchConfig::from_toml_str("[runner]\nrepetition_min_period_bytes = 4097\n").unwrap_err();
        assert!(
            matches!(
                &err,
                DchConfigError::Invalid(message)
                    if message.contains("repetition_min_period_bytes = 4097")
                        && message.contains("4096-byte period cap")
            ),
            "the rejection names the value and the cap: {err:?}"
        );
    }

    #[test]
    fn from_toml_str_loads_the_repetition_floor_at_either_bound() {
        // The library's own floor and cap are accepted values: a host
        // asking for the library default on purpose gets it, and so does
        // one asking for the widest window the guard ever scans.
        let config =
            DchConfig::from_toml_str("[runner]\nrepetition_min_period_bytes = 48\n").unwrap();
        assert_eq!(
            config.runner.repetition_min_period_bytes, 48,
            "the library floor loads verbatim"
        );
        let config =
            DchConfig::from_toml_str("[runner]\nrepetition_min_period_bytes = 4096\n").unwrap();
        assert_eq!(
            config.runner.repetition_min_period_bytes, 4096,
            "the period cap loads verbatim"
        );
    }

    #[test]
    fn from_toml_str_loads_zero_as_a_disarmed_guard() {
        // Zero is the documented disarm value, not a below-floor mistake:
        // the loader passes it through and the runner maps it to a
        // disarmed guard.
        let config =
            DchConfig::from_toml_str("[runner]\nrepetition_min_period_bytes = 0\n").unwrap();
        assert_eq!(
            config.runner.repetition_min_period_bytes, 0,
            "the disarm value loads verbatim"
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
        assert_eq!(PermissionMode::ReadOnly.as_str(), "read_only");
        assert_eq!(PermissionMode::Interactive.as_str(), "interactive");
        // The retired spellings load through the migration mapping.
        assert_eq!(
            DchConfig::from_toml_str("[runner]\npermission_mode = \"plan\"")
                .expect("plan migrates")
                .runner
                .permission_mode,
            PermissionMode::ReadOnly,
            "plan was renamed read_only"
        );
        assert_eq!(
            DchConfig::from_toml_str("[runner]\npermission_mode = \"accept_edits\"")
                .expect("accept_edits migrates")
                .runner
                .permission_mode,
            PermissionMode::Interactive,
            "accept_edits dissolved into interactive"
        );
    }
}
