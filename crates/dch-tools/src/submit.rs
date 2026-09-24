//! The Submit tool — git patch generation for PR submission.

use std::future::Future;
use std::pin::Pin;
use std::process::Output as ProcessOutput;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use std::fmt::Write as _;

use serde_json::Value;
use serde_json::json;

use crate::regex_cache::get_or_compile;

/// One cargo summary line: `test result: ok. 12 passed; 0 failed; 2 ignored`.
///
/// Cargo prints one such line per test target — every crate's unit
/// tests, every integration target, the doc-tests — so
/// [`parse_cargo_test_results`] aggregates every match rather than
/// trusting a single line. The status word (`ok`/`FAILED`) becomes the
/// run's failure verdict when the counted failures cannot (a target can
/// fail with zero failing tests), and the trailing groups are cargo's
/// own spelling: the count it calls `ignored` feeds the skipped tally.
const CARGO_RESULT_PATTERN: &str =
    r"test result:\s*(\w+)\.\s*(\d+)\s+passed;\s*(\d+)\s+failed(?:;\s*(\d+)\s+ignored)?";

/// A failing cargo test name, in either of cargo's two spellings.
///
/// Verbose runs print `test foo::bar ... FAILED`; quiet runs (the tool's
/// own detected command) print `foo::bar --- FAILED`. The alternation
/// accepts both so the failed-tests list carries the names cargo
/// printed, in cargo's own spelling, whichever verbosity produced the
/// output.
const CARGO_FAIL_PATTERN: &str = r"(?:test\s+)?(\S+)\s+(?:\.\.\.|---)\s+FAILED";

/// A jest/pytest-style summary: `5 passed, 2 failed` or `2 failed, 5 passed`.
///
/// Jest prints the failed count first (`Tests: 2 failed, 3 passed`);
/// pytest and the terse npm spellings print passed first, and the comma
/// between the counts is optional — one pattern with both orderings
/// serves every spelling for [`parse_jest_results`], whose framework
/// probe cannot tell them apart either.
const GENERIC_RESULT_PATTERN: &str =
    r"(?:(\d+)\s+passed,?\s+(\d+)\s+failed|(\d+)\s+failed,?\s+(\d+)\s+passed)";

/// The glyphs a failing jest/pytest case line is marked with.
///
/// Jest's own reporter uses `✕`; some terminals and the pytest family
/// render `✗` or `●` in the same position. The failing-case names are
/// taken from after whichever glyph the line carries.
const FAIL_MARKS: [char; 3] = ['✗', '✕', '●'];

/// The base commit diffed against when the caller names none.
///
/// Mirrors the schema's documented default: the last commit, so a
/// submission on a clean tree reviews exactly the change just made.
const DEFAULT_BASE: &str = "HEAD~1";

/// The byte cap the diff body obeys in every output format.
///
/// A lockfile-scale diff would otherwise flow into the model's context
/// unbounded — no middleware trims a tool's payload — so the body is cut
/// at the same budget the Bash tool gives a command's output, and the
/// truncation marker keeps the cut visible rather than silent.
const MAX_DIFF_BYTES: usize = 1_000_000;

/// Generate a clean git patch for submission/PR.
///
/// Runs `git diff` against a base commit (default `HEAD~1`), optionally
/// runs the project's test command, and renders the result as `unified`,
/// `github`, `git`, or `summary`. The test run may itself mutate the
/// workspace, and a test run racing a real submission is nonsensical, so
/// the tool is neither read-only nor concurrency-safe.
pub struct SubmitTool;

impl Tool for SubmitTool {
    fn name(&self) -> &'static str {
        "Submit"
    }

    fn description(&self) -> &'static str {
        "Generate a clean git patch for submission/PR. Creates standardized \
         diff with test summary."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "Base commit to diff against (default: HEAD~1); a commit, branch, or tag name — must not start with '-'"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["unified", "github", "git", "summary"],
                        "default": "github",
                        "description": "Patch format"
                    },
                    "include_tests": {
                        "type": "boolean",
                        "default": true,
                        "description": "Include test results in the patch"
                    },
                    "summary": {
                        "type": "boolean",
                        "default": true,
                        "description": "Include change summary"
                    },
                    "target_branch": {
                        "type": "string",
                        "default": "main",
                        "description": "Target branch name (for PR context)"
                    },
                    "test_command": {
                        "type": "string",
                        "description": "Command to run tests (auto-detect if omitted)"
                    }
                },
                "required": []
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let cwd = ctx.cwd.clone();
        Box::pin(self.submit_inner(input, cwd))
    }
}

impl SubmitTool {
    /// Body of [`Tool::call`].
    ///
    /// Reads the wire fields with their documented defaults and hands
    /// the rest to [`generate_patch`]; the outcome maps onto an ordinary
    /// or error-marked tool result.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when a `git` or test-command
    /// process cannot be spawned at all. Every ordinary failure — not a
    /// repository, an unknown base ref, a test run that cannot start —
    /// is a soft error-marked result so the model can react without the
    /// turn failing.
    async fn submit_inner(&self, input: Value, cwd: String) -> Result<ToolOutput, ToolError> {
        let base = input.get("base").and_then(Value::as_str);
        let format = input
            .get("format")
            .and_then(Value::as_str)
            .map_or(DiffFormat::GitHub, DiffFormat::from_str);
        let include_tests = input
            .get("include_tests")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let include_summary = input
            .get("summary")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let target_branch = input
            .get("target_branch")
            .and_then(Value::as_str)
            .unwrap_or("main");
        let test_command = input.get("test_command").and_then(Value::as_str);

        let outcome = generate_patch(
            &cwd,
            base.unwrap_or(DEFAULT_BASE),
            format,
            include_tests,
            include_summary,
            Some(target_branch),
            test_command,
        )
        .await?;
        Ok(match outcome {
            PatchOutcome::Patch(text) => ToolOutput::text(text),
            PatchOutcome::Failed(text) => ToolOutput::error_text(text),
        })
    }
}

/// The patch a submission renders, or a soft failure to report.
///
/// A soft failure keeps the turn alive: the model sees an error-marked
/// result it can act on (pick a different base, initialize a repository)
/// instead of the run aborting.
enum PatchOutcome {
    /// The formatted patch body, ready to hand to the requester.
    ///
    /// Carries whichever renderer the `format` input selected; an empty
    /// change set lands here too, with the no-changes message as its
    /// body.
    Patch(String),

    /// An error-marked message for conditions the caller can correct.
    ///
    /// Not a repository, an unresolvable base, an option-shaped base:
    /// each names what went wrong in terms the model can act on, so the
    /// turn continues instead of aborting.
    Failed(String),
}

/// The output format a submission renders.
///
/// Selected by the `format` input string; spellings follow the wire
/// schema's enum, and anything unrecognized falls back to the unified
/// diff rather than failing the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffFormat {
    /// The raw unified diff, verbatim.
    ///
    /// What `git diff` printed, capped by [`MAX_DIFF_BYTES`] and wrapped
    /// in nothing else — for consumers that apply or review hunks with
    /// no surrounding prose.
    Unified,

    /// A GitHub-flavored markdown PR body.
    ///
    /// The default: a summary block, the file list, the test table, and
    /// a fenced diff, shaped to paste straight into a pull-request
    /// description.
    GitHub,

    /// A `git format-patch`-style header followed by the diff.
    ///
    /// Keeps the plain-text shape mail-style tooling expects — a short
    /// statistics header, one test tally line, then the bare hunks.
    GitPatch,

    /// A compact human-readable summary.
    ///
    /// Counts and file names only, no diff body — the format to reach
    /// for when the reader wants the shape of the change rather than
    /// its content.
    Summary,
}

impl DiffFormat {
    /// Parse the wire's `format` spelling.
    ///
    /// Case-insensitive; `git` and the legacy `patch` alias both select
    /// [`DiffFormat::GitPatch`], and any unrecognized value selects
    /// [`DiffFormat::Unified`].
    fn from_str(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "github" => Self::GitHub,
            "git" | "patch" => Self::GitPatch,
            "summary" => Self::Summary,
            _ => Self::Unified,
        }
    }
}

/// Parsed test-suite results embedded in a patch.
///
/// Carried by the renderers into their test sections. When the parse
/// produced no counts — the generic heuristic ran because no
/// framework-specific summary matched — `output_summary` is the
/// truncated raw output the renderer shows so a reader can judge the
/// run the numbers could not describe.
#[derive(Debug, Clone)]
struct TestResults {
    /// The framework display name the detection probe chose.
    ///
    /// Threaded into the rendered test section so a reader knows which
    /// runner produced the counts ("cargo", "jest/npm", …).
    framework: String,

    /// Total tests the parse attributed to the run.
    ///
    /// Passed, failed, and skipped summed; a parse that produced no
    /// counts leaves it at zero, which is what triggers the renderers'
    /// raw-output fallback.
    total: usize,

    /// Tests reported passing.
    ///
    /// Summed over every summary line for runners that print one per
    /// target, cargo being the common case.
    passed: usize,

    /// Tests reported failing.
    ///
    /// Every renderer's pass/fail verdict derives from this count
    /// alone; the cargo parser's status-word fold guarantees a failing
    /// target cannot leave it at zero.
    failed: usize,

    /// Tests reported skipped.
    ///
    /// Cargo's summary calls these `ignored`; the count lands here under
    /// the renderer's shared vocabulary.
    skipped: usize,

    /// Names of the failing tests, as the framework printed them.
    ///
    /// Rendered as a capped list so a reader can jump from the patch to
    /// the exact cases that broke; empty when the framework prints
    /// counts only.
    failures: Vec<String>,

    /// The truncated raw output the counts were parsed from.
    ///
    /// Rendered when the counts are zero and the output is not, so the
    /// evidence behind a numberless run stays visible.
    output_summary: String,
}

/// The test runner a project's marker files imply.
///
/// One probe produces both halves — the shell command that runs the
/// suite and the display name the patch's test section carries — so the
/// parser selection and the rendered name can never disagree.
struct DetectedFramework {
    /// The shell command `run_tests` executes.
    ///
    /// Includes its own output redirection (`2>&1`) so framework
    /// diagnostics land in the captured output the parsers read.
    command: &'static str,

    /// The framework name threaded into [`TestResults::framework`].
    ///
    /// Also selects the parser: the cargo and jest spellings route
    /// their output to the exact parsers, everything else falls to the
    /// generic heuristic.
    name: &'static str,
}

/// Generate the patch body for one submission.
///
/// Orchestrates the shell-outs in order — changed files, diff content,
/// statistics, optional test run — then hands the pieces to the selected
/// renderer. A `git` failure short-circuits to [`PatchOutcome::Failed`]
/// with git's own stderr; an empty change set renders the no-changes
/// message as ordinary output.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] only when a `git` process cannot
/// be spawned; git's own non-zero exits become [`PatchOutcome`]
/// variants rather than errors.
async fn generate_patch(
    cwd: &str,
    base_commit: &str,
    format: DiffFormat,
    include_tests: bool,
    include_summary: bool,
    target_branch: Option<&str>,
    test_command: Option<&str>,
) -> Result<PatchOutcome, ToolError> {
    if base_commit.starts_with('-') {
        return Ok(PatchOutcome::Failed(format!(
            "Invalid base commit '{base_commit}': the base names a commit to diff against, and a \
             leading '-' would be read by git as an option. Name a commit, branch, or tag instead."
        )));
    }
    let changed_files = match git_changed_files(cwd, base_commit).await? {
        GitFiles::List(files) => files,
        GitFiles::Failed(stderr) => {
            return Ok(PatchOutcome::Failed(format!(
                "Not in a git repository (or bad base commit '{base_commit}'): {stderr}"
            )));
        }
    };
    if changed_files.is_empty() {
        return Ok(PatchOutcome::Patch(
            "No changes detected compared to base commit.".to_string(),
        ));
    }

    let diff_content = git_diff_content(cwd, base_commit).await?;
    let (additions, deletions) = parse_diff_stats(&diff_content);
    let test_results = if include_tests {
        Some(run_tests(cwd, test_command).await?)
    } else {
        None
    };

    let body = match format {
        DiffFormat::GitHub => format_github_patch(
            &changed_files,
            &diff_content,
            additions,
            deletions,
            target_branch,
            test_results.as_ref(),
            include_summary,
        ),
        DiffFormat::GitPatch => format_git_patch(
            &changed_files,
            &diff_content,
            additions,
            deletions,
            test_results.as_ref(),
        ),
        DiffFormat::Unified => diff_content,
        DiffFormat::Summary => {
            format_summary(&changed_files, additions, deletions, test_results.as_ref())
        }
    };
    Ok(PatchOutcome::Patch(body))
}

/// The outcome of listing changed files, split by what a git failure means.
///
/// An empty [`GitFiles::List`] is an ordinary no-changes result; a failure
/// to run `git diff --name-only` at all means the directory is not a
/// repository or the base ref does not resolve, which the caller reports
/// with git's stderr verbatim.
enum GitFiles {
    /// The files the base diff touches, possibly empty.
    ///
    /// An empty list is the ordinary no-changes result, not an error;
    /// the caller renders the no-changes message from it.
    List(Vec<String>),

    /// Git exited non-zero; the payload is its trimmed stderr.
    ///
    /// Means the directory is not a repository or the base ref does not
    /// resolve; the caller reports the stderr verbatim so the model can
    /// tell which.
    Failed(String),
}

/// List the files `git diff --name-only <base>` reports.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] only when the `git` process itself
/// cannot be spawned; git's own non-zero exits land in [`GitFiles`].
async fn git_changed_files(cwd: &str, base: &str) -> Result<GitFiles, ToolError> {
    let output = git_output(cwd, &["diff", "--name-only", base]).await?;
    if !output.status.success() {
        return Ok(GitFiles::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect();
    Ok(GitFiles::List(files))
}

/// Read the diff `git diff <base>` produces, capped in size.
///
/// A non-zero exit yields an empty body — the changed-files listing has
/// already succeeded, so the rare failure here renders as a patch with no
/// diff hunks rather than an error. A body larger than
/// [`MAX_DIFF_BYTES`] is cut with a visible truncation marker so a
/// lockfile-scale diff cannot flood the conversation.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the `git` binary cannot be
/// spawned.
async fn git_diff_content(cwd: &str, base: &str) -> Result<String, ToolError> {
    let output = git_output(cwd, &["diff", base]).await?;
    if !output.status.success() {
        return Ok(String::new());
    }
    Ok(truncate_output(
        &String::from_utf8_lossy(&output.stdout),
        MAX_DIFF_BYTES,
    ))
}

/// Run one `git` invocation in `cwd` and collect its raw outcome.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the `git` binary cannot be
/// spawned — the caller treats a missing git as an environmental failure,
/// not a per-repo condition.
async fn git_output(cwd: &str, args: &[&str]) -> Result<ProcessOutput, ToolError> {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to run git {args:?}: {e}")))
}

/// Count added and removed lines in a unified diff.
///
/// `+++`/`---` file headers are excluded so only hunk content counts;
/// any other line punctuation leaves both counters untouched.
fn parse_diff_stats(diff: &str) -> (usize, usize) {
    let mut additions: usize = 0;
    let mut deletions: usize = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            additions = additions.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions = deletions.saturating_add(1);
        }
    }
    (additions, deletions)
}

/// Run the project's tests and parse the output.
///
/// The command comes from `test_command` when given, otherwise from the
/// marker probe. A command that cannot even be spawned renders as a
/// zero-count result carrying "Tests could not be run" — the patch still
/// produces, because a broken test setup must not block reading the diff.
///
/// # Errors
///
/// Never fails in practice: a command that cannot be spawned produces
/// the zero-count result instead, and the framework carries the
/// [`Result`] shape only to match the caller's fallibility.
async fn run_tests(cwd: &str, test_command: Option<&str>) -> Result<TestResults, ToolError> {
    let detected = detect_test_framework(cwd).await;
    let command = test_command.unwrap_or(detected.command);
    let output = tokio::process::Command::new("bash")
        .args(["-c", command])
        .current_dir(cwd)
        .output()
        .await;
    let combined = match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!("{stdout}\n{stderr}")
        }
        Err(_) => {
            return Ok(TestResults {
                framework: detected.name.to_string(),
                total: 0,
                passed: 0,
                failed: 0,
                skipped: 0,
                failures: Vec::new(),
                output_summary: "Tests could not be run".to_string(),
            });
        }
    };
    Ok(parse_test_results(&combined, detected.name))
}

/// Probe the project's marker files for its test runner.
///
/// The four markers cover the realistic target projects — Rust, JS/TS,
/// Go, Python — and a project with none of them gets a no-op command so
/// the test section renders "0 tests" instead of erroring.
async fn detect_test_framework(cwd: &str) -> DetectedFramework {
    let markers = [
        ("Cargo.toml", "cargo test --quiet 2>&1", "cargo"),
        ("package.json", "npm test 2>&1", "jest/npm"),
        ("go.mod", "go test ./... 2>&1", "go test"),
    ];
    for (marker, command, name) in markers {
        if tokio::fs::try_exists(std::path::Path::new(cwd).join(marker))
            .await
            .unwrap_or(false)
        {
            return DetectedFramework { command, name };
        }
    }
    if tokio::fs::try_exists(std::path::Path::new(cwd).join("pyproject.toml"))
        .await
        .unwrap_or(false)
    {
        return DetectedFramework {
            command: "pytest 2>&1",
            name: "pytest",
        };
    }
    DetectedFramework {
        command: "echo 'No test framework detected' && exit 0",
        name: "unknown",
    }
}

/// Parse test output under the detected framework's parser.
///
/// The framework name picks the cargo or jest parser first — the counts
/// those produce are exact — and anything else (or a parser that finds no
/// summary line) falls through to the generic heuristic, which reports
/// only whether the output mentions `FAIL`.
fn parse_test_results(output: &str, framework: &str) -> TestResults {
    match framework {
        "cargo" => {
            if let Some(results) = parse_cargo_test_results(output) {
                return results;
            }
        }
        "jest/npm" => {
            if let Some(results) = parse_jest_results(output) {
                return results;
            }
        }
        _ => {}
    }
    TestResults {
        framework: framework.to_string(),
        total: 0,
        passed: 0,
        failed: usize::from(output.contains("FAIL")),
        skipped: 0,
        failures: Vec::new(),
        output_summary: truncate_output(output, 500),
    }
}

/// Parse cargo's per-target `test result:` summaries and failing names.
///
/// Cargo prints one summary line per test target — a workspace run
/// carries one per crate, integration targets and doc-tests each add
/// another — so the counts are summed over every line and the run fails
/// if any target's status word is `FAILED`, even when that target
/// counted zero failing tests. Failing names are collected from either
/// of cargo's spellings (see [`CARGO_FAIL_PATTERN`]).
///
/// Returns [`None`] when the output carries no cargo summary — the caller
/// then applies the generic heuristic.
fn parse_cargo_test_results(output: &str) -> Option<TestResults> {
    let result_re = get_or_compile(CARGO_RESULT_PATTERN, false).ok()?;
    let fail_re = get_or_compile(CARGO_FAIL_PATTERN, false).ok()?;
    let failures = output
        .lines()
        .filter_map(|line| {
            fail_re
                .captures(line)
                .and_then(|caps| caps.get(1))
                .map(|name| name.as_str().to_string())
        })
        .collect();
    let mut saw_summary = false;
    let mut passed: usize = 0;
    let mut summed_failed: usize = 0;
    let mut skipped: usize = 0;
    let mut any_target_failed = false;
    for caps in result_re.captures_iter(output) {
        saw_summary = true;
        passed = passed.saturating_add(capture_usize(&caps, 2));
        summed_failed = summed_failed.saturating_add(capture_usize(&caps, 3));
        skipped = skipped.saturating_add(capture_usize(&caps, 4));
        any_target_failed |= caps.get(1).is_some_and(|word| word.as_str() == "FAILED");
    }
    if !saw_summary {
        return None;
    }
    let failed = if summed_failed == 0 && any_target_failed {
        1
    } else {
        summed_failed
    };
    Some(TestResults {
        framework: "cargo".to_string(),
        total: passed.saturating_add(failed).saturating_add(skipped),
        passed,
        failed,
        skipped,
        failures,
        output_summary: truncate_output(output, 500),
    })
}

/// Parse jest/npm's summary counts and failing test names.
///
/// The counts come from the last summary match in the output — jest
/// prints per-suite tallies before the final `Tests:` line, so the last
/// one is the run's — in either ordering (see
/// [`GENERIC_RESULT_PATTERN`]). Failing names come from the lines a
/// [`FAIL_MARKS`] glyph marks.
///
/// Unlike the cargo parser this always produces a result — jest's summary
/// grammar is loose enough that absence of the pattern is itself the
/// answer (zero tests parsed).
fn parse_jest_results(output: &str) -> Option<TestResults> {
    let result_re = get_or_compile(GENERIC_RESULT_PATTERN, false).ok()?;
    let mut passed = 0;
    let mut failed = 0;
    for caps in result_re.captures_iter(output) {
        if caps.get(1).is_some() {
            passed = capture_usize(&caps, 1);
            failed = capture_usize(&caps, 2);
        } else {
            failed = capture_usize(&caps, 3);
            passed = capture_usize(&caps, 4);
        }
    }
    let mut failures = Vec::new();
    for line in output.lines() {
        if let Some(at) = FAIL_MARKS.iter().find_map(|mark| line.find(*mark)) {
            let name = line
                .get(at..)
                .unwrap_or("")
                .chars()
                .skip(1)
                .collect::<String>()
                .trim()
                .to_string();
            if !name.is_empty() {
                failures.push(name);
            }
        }
    }
    Some(TestResults {
        framework: "jest".to_string(),
        total: passed.saturating_add(failed),
        passed,
        failed,
        skipped: 0,
        failures,
        output_summary: truncate_output(output, 500),
    })
}

/// Read a numeric capture group as a count, defaulting to zero.
///
/// A group that did not participate in the match — the optional
/// `ignored` count in a short cargo summary, for instance — reads as
/// zero rather than failing the parse.
fn capture_usize(caps: &regex::Captures<'_>, group: usize) -> usize {
    caps.get(group)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0)
}

/// Cap `output` at `max_bytes`, cutting on a character boundary.
///
/// The cut lands on the last boundary at or before the budget so a
/// multibyte character straddling it is dropped whole rather than sliced
/// apart; the `...`/`(truncated)` suffix keeps the cut visible.
fn truncate_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let mut end = max_bytes;
    while !output.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let head = output.get(..end).unwrap_or(output);
    format!("{head}...\n(truncated)")
}

/// Render a GitHub-flavored markdown PR body.
///
/// The default format: a summary block, the modified-file list capped at
/// twenty entries, the test-results table, the diff in a fenced block,
/// and the target-branch footer when one is named.
fn format_github_patch(
    files: &[String],
    diff: &str,
    additions: usize,
    deletions: usize,
    target_branch: Option<&str>,
    test_results: Option<&TestResults>,
    include_summary: bool,
) -> String {
    let mut output = String::new();
    output.push_str("## Pull Request Summary\n\n");
    if include_summary {
        write!(
            output,
            "### Changes\n- **Files changed**: {}\n- **+{} additions / -{} deletions**\n\n",
            files.len(),
            additions,
            deletions
        )
        .ok();
        output.push_str("### Modified Files\n");
        for file in files.iter().take(20) {
            writeln!(output, "- `{file}`").ok();
        }
        if files.len() > 20 {
            write!(output, "... and {} more", files.len().saturating_sub(20)).ok();
        }
        output.push('\n');
    }
    if let Some(results) = test_results {
        let status = if results.failed == 0 {
            "Passing"
        } else {
            "Failing"
        };
        write!(
            output,
            "### Test Results\n\n**Status**: {} | **Framework**: {}\n\n\
             | Metric | Count |\n|--------|-------|\n| Total | {} |\n| Passed | {} |\n\
             | Failed | {} |\n| Skipped | {} |\n\n",
            status,
            results.framework,
            results.total,
            results.passed,
            results.failed,
            results.skipped
        )
        .ok();
        if results.total == 0 && !results.output_summary.trim().is_empty() {
            output.push_str("```\n");
            output.push_str(&results.output_summary);
            output.push_str("\n```\n\n");
        }
        if !results.failures.is_empty() {
            output.push_str("**Failed Tests**:\n```\n");
            for name in results.failures.iter().take(10) {
                writeln!(output, "✗ {name}").ok();
            }
            if results.failures.len() > 10 {
                writeln!(
                    output,
                    "... and {} more",
                    results.failures.len().saturating_sub(10)
                )
                .ok();
            }
            output.push_str("```\n\n");
        }
    }
    output.push_str("### Diff\n\n```diff\n");
    output.push_str(diff);
    output.push_str("\n```\n");
    if let Some(branch) = target_branch {
        write!(output, "\n**Target branch**: `{branch}`").ok();
    }
    output
}

/// Render a `git format-patch`-style header plus the raw diff.
///
/// Plain text throughout — no markdown fences or tables — because this
/// format exists for consumers that read mail-style patches. The test
/// tally compresses to a single line, with the failing names
/// parenthesized when there are any.
fn format_git_patch(
    files: &[String],
    diff: &str,
    additions: usize,
    deletions: usize,
    test_results: Option<&TestResults>,
) -> String {
    let mut output = String::new();
    writeln!(output, "Patch: {} file(s) changed", files.len()).ok();
    write!(output, "Statistics: +{additions} -{deletions}\n\n").ok();
    if let Some(results) = test_results {
        write!(
            output,
            "Tests: {} passed, {} failed",
            results.passed, results.failed
        )
        .ok();
        if !results.failures.is_empty() {
            write!(output, " ({})", results.failures.join(", ")).ok();
        }
        output.push('\n');
    }
    output.push_str("\n---\n\n");
    output.push_str(diff);
    output
}

/// Render a compact human-readable summary with no diff body.
///
/// The file list caps at twenty entries and announces how many more the
/// diff touches, so a wide change cannot silently hide its extent.
fn format_summary(
    files: &[String],
    additions: usize,
    deletions: usize,
    test_results: Option<&TestResults>,
) -> String {
    let mut output = String::new();
    output.push_str("Change Summary\n");
    writeln!(output, "  Files: {}", files.len()).ok();
    writeln!(output, "  +{additions} / -{deletions}").ok();
    if let Some(results) = test_results {
        let status = if results.failed == 0 {
            "[PASS]"
        } else {
            "[FAIL]"
        };
        writeln!(
            output,
            "  Tests: {} {}/{}",
            status, results.passed, results.total
        )
        .ok();
    }
    output.push_str("\nFiles:\n");
    for file in files.iter().take(20) {
        writeln!(output, "  - {file}").ok();
    }
    if files.len() > 20 {
        writeln!(output, "  ... and {} more", files.len().saturating_sub(20)).ok();
    }
    output
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use loopctl::tool::ToolContext;

    /// A tool context whose cwd is `dir`, carrying a runner context.
    ///
    /// The extension is what the file tools read for the same cwd, so a
    /// called Submit behaves as it would inside a real run.
    fn ctx_in(cwd: &std::path::Path) -> ToolContext {
        let mut ctx = ToolContext {
            cwd: cwd.to_string_lossy().into_owned(),
            ..ToolContext::default()
        };
        ctx.set_extension(crate::context::RunnerContext::new(cwd.to_path_buf()));
        ctx
    }

    /// Build a Submit input from optional `(key, value)` overrides.
    ///
    /// Every test drives the tool through the same JSON surface the
    /// model sees; an absent key exercises the field's documented
    /// default.
    fn submit_args(extra: &[(&str, Value)]) -> Value {
        let mut input = json!({});
        for (key, value) in extra {
            input[*key] = value.clone();
        }
        input
    }

    /// A three-commit baseline plus one staged-but-uncommitted change.
    ///
    /// Three commits make `HEAD~1` and wider bases resolve, and the
    /// staged change is what a default submission reports — `git diff`
    /// never sees untracked files, so the fixture stages rather than
    /// merely writes it.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch=main"]);
        std::fs::write(dir.path().join("base.txt"), "baseline\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "baseline"]);
        std::fs::write(dir.path().join("second.txt"), "second\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "second"]);
        std::fs::write(dir.path().join("third.txt"), "third\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "third"]);
        std::fs::write(dir.path().join("changed.txt"), "modified\n").unwrap();
        git(dir.path(), &["add", "changed.txt"]);
        dir
    }

    /// Run one `git` command in `dir` with a fixed identity.
    ///
    /// The fixed author and committer keep fixture commits reproducible
    /// across environments; a command that fails aborts the test.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .status()
            .expect("git spawns");
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    /// Call the tool once and unwrap the engine result.
    ///
    /// Soft errors surface as the error-marked output they are; only a
    /// process that cannot be spawned would trip the unwrap.
    async fn call(input: Value, cwd: &std::path::Path) -> ToolOutput {
        SubmitTool.call(input, &ctx_in(cwd)).await.unwrap()
    }

    #[test]
    fn diff_format_parses_salvage_spellings() {
        assert_eq!(DiffFormat::from_str("github"), DiffFormat::GitHub);
        assert_eq!(DiffFormat::from_str("GitHub"), DiffFormat::GitHub);
        assert_eq!(DiffFormat::from_str("GIT"), DiffFormat::GitPatch);
        assert_eq!(DiffFormat::from_str("git"), DiffFormat::GitPatch);
        assert_eq!(DiffFormat::from_str("patch"), DiffFormat::GitPatch);
        assert_eq!(DiffFormat::from_str("summary"), DiffFormat::Summary);
        assert_eq!(DiffFormat::from_str("unknown"), DiffFormat::Unified);
        assert_eq!(DiffFormat::from_str(""), DiffFormat::Unified);
    }

    #[test]
    fn diff_stats_count_additions_and_deletions() {
        let headers_only = "--- a/file.rs\n+++ b/file.rs\n";
        assert_eq!(parse_diff_stats(headers_only), (0, 0));
        let balanced = "--- a/f\n+++ b/f\n@@\n-old\n+new\n ctx\n";
        assert_eq!(parse_diff_stats(balanced), (1, 1));
        let skewed = "--- a/f\n+++ b/f\n@@\n+a\n+b\n+c\n+d\n+e\n-x\n-y\n-z\n";
        assert_eq!(parse_diff_stats(skewed), (5, 3));
    }

    #[test]
    fn truncate_output_cuts_on_a_character_boundary() {
        let short = "abc";
        assert_eq!(truncate_output(short, 100), "abc");
        let ascii = "a".repeat(1000);
        let cut = truncate_output(&ascii, 100);
        assert!(cut.len() <= 115, "suffix keeps the cut near budget");
        let multibyte = "é".repeat(100);
        let cut = truncate_output(&multibyte, 101);
        assert!(
            cut.starts_with(&"é".repeat(50)) && cut.ends_with("...\n(truncated)"),
            "the cut drops the straddling é whole: {cut:?}"
        );
        assert!(!cut.contains(char::REPLACEMENT_CHARACTER));
    }

    /// A verbatim `cargo test --quiet 2>&1` transcript from a fixture
    /// crate whose library target passes one test and ignores another
    /// while its integration target fails one — the two-summary shape
    /// every real workspace run produces, in the quiet spelling the
    /// tool's own detected command outputs. Only the panic thread id is
    /// normalized out; it varies per run and nothing parses it.
    const QUIET_CARGO_TRANSCRIPT: &str = "\
running 2 tests
i.

test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 1 test
integration_fails --- FAILED

failures:

---- integration_fails stdout ----

thread 'integration_fails' panicked at tests/it.rs:2:26:
boom
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    integration_fails

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--test it`
";

    #[test]
    fn cargo_results_aggregate_every_target_summary() {
        let results = parse_test_results(QUIET_CARGO_TRANSCRIPT, "cargo");
        assert_eq!(results.passed, 1, "the passing lib target counts");
        assert_eq!(
            results.failed, 1,
            "the failing integration target counts, not just the first summary line"
        );
        assert_eq!(
            results.skipped, 1,
            "cargo's `ignored` spelling feeds the skipped count"
        );
        assert_eq!(results.total, 3);
        assert_eq!(
            results.failures,
            vec!["integration_fails".to_string()],
            "the quiet `name --- FAILED` spelling is collected"
        );
    }

    #[test]
    fn cargo_results_parse_the_verbose_failure_spelling() {
        let verbose = "test foo::bar ... FAILED\n\ntest result: FAILED. 10 passed; 3 failed;";
        let results = parse_test_results(verbose, "cargo");
        assert_eq!(results.passed, 10);
        assert_eq!(results.failed, 3);
        assert_eq!(results.failures, vec!["foo::bar".to_string()]);
    }

    #[test]
    fn a_failing_target_with_zero_failed_tests_still_fails() {
        let output =
            "test result: FAILED. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;";
        let results = parse_test_results(output, "cargo");
        assert_eq!(
            results.failed, 1,
            "the status word alone marks the run failed"
        );
    }

    #[test]
    fn cargo_output_without_a_summary_falls_to_the_heuristic() {
        let results = parse_test_results("compiling stuff...", "cargo");
        assert_eq!(results.passed, 0, "no summary line falls to the heuristic");
    }

    #[test]
    fn jest_results_parse_from_framework_output() {
        let output = "5 passed, 2 failed\n✗ does the thing\n● other failure";
        let results = parse_test_results(output, "jest/npm");
        assert_eq!(results.total, 7);
        assert_eq!(results.passed, 5);
        assert_eq!(results.failed, 2);
        assert_eq!(results.failures.len(), 2);
        assert!(results.failures.contains(&"does the thing".to_string()));

        let quiet = "suite ran";
        let results = parse_test_results(quiet, "jest/npm");
        assert_eq!(results.total, 0);
    }

    #[test]
    fn jest_results_parse_the_failed_first_summary_order() {
        let output = "FAIL src/app.test.js\n  \u{2715} renders the header\n  \u{2715} saves on click\n\
                      \nTest Suites: 1 failed, 1 total\n\
                      Tests:       2 failed, 3 passed, 5 total\n";
        let results = parse_test_results(output, "jest/npm");
        assert_eq!(results.passed, 3, "jest's failed-first order parses");
        assert_eq!(results.failed, 2);
        assert_eq!(results.total, 5);
        assert_eq!(
            results.failures.len(),
            2,
            "jest's \u{2715} glyph marks failing cases: {:?}",
            results.failures
        );
        assert!(results.failures.contains(&"renders the header".to_string()));
    }

    #[test]
    fn generic_output_counts_fail_mentions() {
        let results = parse_test_results("setup FAIL teardown FAIL ok", "unknown");
        assert_eq!(
            results.failed, 1,
            "the heuristic reports presence, not count"
        );
        assert_eq!(results.framework, "unknown");
    }

    #[tokio::test]
    async fn marker_files_select_the_test_command() {
        for (marker, command) in [
            ("Cargo.toml", "cargo test --quiet 2>&1"),
            ("package.json", "npm test 2>&1"),
            ("go.mod", "go test ./... 2>&1"),
            ("pyproject.toml", "pytest 2>&1"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(marker), "").unwrap();
            let detected = detect_test_framework(dir.path().to_str().unwrap()).await;
            assert_eq!(detected.command, command, "{marker} selects {command}");
        }
        let bare = tempfile::tempdir().unwrap();
        let detected = detect_test_framework(bare.path().to_str().unwrap()).await;
        assert_eq!(
            detected.command, "echo 'No test framework detected' && exit 0",
            "no markers select the no-op"
        );
    }

    #[tokio::test]
    async fn marker_files_select_the_framework_name() {
        for (marker, name) in [
            ("Cargo.toml", "cargo"),
            ("package.json", "jest/npm"),
            ("go.mod", "go test"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(marker), "").unwrap();
            let detected = detect_test_framework(dir.path().to_str().unwrap()).await;
            assert_eq!(detected.name, name, "{marker} names {name}");
        }
        let bare = tempfile::tempdir().unwrap();
        let detected = detect_test_framework(bare.path().to_str().unwrap()).await;
        assert_eq!(detected.name, "unknown");
    }

    #[test]
    fn submit_schema_matches_the_spec() {
        let schema = SubmitTool.schema();
        let input = schema.input_schema;
        let properties = input
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert_eq!(properties.len(), 6, "exactly six properties");
        for key in [
            "base",
            "format",
            "include_tests",
            "summary",
            "target_branch",
            "test_command",
        ] {
            assert!(properties.contains_key(key), "{key} present");
        }
        assert_eq!(
            input.pointer("/properties/format/enum"),
            Some(&json!(["unified", "github", "git", "summary"]))
        );
        assert_eq!(
            input.pointer("/properties/format/default"),
            Some(&json!("github"))
        );
        assert_eq!(
            input.pointer("/properties/include_tests/default"),
            Some(&json!(true))
        );
        assert_eq!(
            input.pointer("/properties/summary/default"),
            Some(&json!(true))
        );
        assert_eq!(
            input.pointer("/properties/target_branch/default"),
            Some(&json!("main"))
        );
        assert_eq!(
            input.get("required"),
            Some(&json!([])),
            "no field is required"
        );
    }

    #[test]
    fn submit_registers_in_the_builtin_registry() {
        let registry = crate::registry::builtin_registry();
        assert!(registry.get("Submit").is_some(), "registered");
        assert!(!SubmitTool.is_read_only());
        assert!(!SubmitTool.is_concurrency_safe());
    }

    #[test]
    fn submit_has_no_system_prompt_fragment() {
        assert!(SubmitTool.system_prompt().is_none());
    }

    #[tokio::test]
    async fn github_format_renders_a_pr_body_by_default() {
        let repo = init_repo();
        let out = call(submit_args(&[]), repo.path()).await;
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.starts_with("## Pull Request Summary"), "{text}");
        assert!(text.contains("### Changes"), "{text}");
        assert!(text.contains("### Modified Files"), "{text}");
        assert!(text.contains("```diff"), "{text}");
        assert!(text.contains("**Target branch**: `main`"), "{text}");
        assert!(text.contains("changed.txt"), "{text}");
    }

    #[tokio::test]
    async fn unified_format_renders_the_raw_diff() {
        let repo = init_repo();
        let out = call(submit_args(&[("format", json!("unified"))]), repo.path()).await;
        let text = out.text_content();
        assert!(text.starts_with("diff --git"), "{text}");
        assert!(!text.contains("## Pull Request"), "{text}");
    }

    #[tokio::test]
    async fn git_format_renders_a_patch_header() {
        let repo = init_repo();
        let out = call(submit_args(&[("format", json!("git"))]), repo.path()).await;
        let text = out.text_content();
        assert!(text.starts_with("Patch: "), "{text}");
        assert!(text.contains("Statistics: +"), "{text}");
        assert!(text.contains("\n---\n\n"), "{text}");
        assert!(text.contains("diff --git"), "{text}");
    }

    #[tokio::test]
    async fn summary_format_renders_a_compact_report() {
        let repo = init_repo();
        let out = call(submit_args(&[("format", json!("summary"))]), repo.path()).await;
        let text = out.text_content();
        assert!(text.starts_with("Change Summary"), "{text}");
        assert!(text.contains("- changed.txt"), "{text}");
        assert!(!text.contains("```diff"), "{text}");
    }

    #[tokio::test]
    async fn a_clean_tree_reports_no_changes() {
        let repo = init_repo();
        git(repo.path(), &["restore", "--staged", "changed.txt"]);
        std::fs::remove_file(repo.path().join("changed.txt")).unwrap();
        let out = call(submit_args(&[("base", json!("HEAD"))]), repo.path()).await;
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            out.text_content(),
            "No changes detected compared to base commit."
        );
    }

    #[tokio::test]
    async fn a_plain_directory_is_a_soft_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = call(submit_args(&[]), dir.path()).await;
        assert!(out.is_error, "the soft-error flag is set");
        assert!(
            out.text_content().contains("Not in a git repository"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn an_explicit_base_widens_the_diff() {
        let repo = init_repo();
        let narrow = call(submit_args(&[]), repo.path()).await.text_content();
        let wide = call(submit_args(&[("base", json!("HEAD~2"))]), repo.path())
            .await
            .text_content();
        assert!(!narrow.contains("second.txt"), "HEAD~1 skips it: {narrow}");
        assert!(
            wide.contains("second.txt"),
            "HEAD~2 covers the second commit's file: {wide}"
        );
    }

    #[tokio::test]
    async fn tests_can_be_disabled() {
        let repo = init_repo();
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname = \"s\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let out = call(submit_args(&[("include_tests", json!(false))]), repo.path()).await;
        let text = out.text_content();
        assert!(!text.contains("### Test Results"), "{text}");
        assert!(!text.contains("Tests:"), "{text}");
    }

    #[tokio::test]
    async fn an_explicit_test_command_bypasses_detection() {
        let repo = init_repo();
        let out = call(
            submit_args(&[("test_command", json!("echo 'all good'"))]),
            repo.path(),
        )
        .await;
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("### Test Results"), "{text}");
        assert!(
            text.contains("all good"),
            "unparsed raw output renders for the reader: {text}"
        );
    }

    #[tokio::test]
    async fn cargo_tests_run_and_render() {
        let repo = init_repo();
        std::fs::create_dir(repo.path().join("src")).unwrap();
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname = \"submit_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("src/lib.rs"),
            "#[test]\nfn passes() {}\n\n#[test]\n#[ignore = \"fixture\"]\nfn skipped_by_fixture() {}\n",
        )
        .unwrap();
        let out = call(submit_args(&[]), repo.path()).await;
        let text = out.text_content();
        assert!(text.contains("### Test Results"), "{text}");
        assert!(text.contains("**Framework**: cargo"), "{text}");
        assert!(text.contains("**Status**: Passing"), "{text}");
        assert!(text.contains("| Passed | 1 |"), "{text}");
        assert!(text.contains("| Skipped | 1 |"), "{text}");
    }

    #[tokio::test]
    async fn a_leading_dash_base_is_rejected() {
        let repo = init_repo();
        let outside = std::env::temp_dir().join("dch-submit-injection-probe.txt");
        if let Err(err) = std::fs::remove_file(&outside)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            panic!("a leftover probe file could not be removed: {err}");
        }
        let base = format!("--output={}", outside.display());
        let out = call(submit_args(&[("base", json!(base))]), repo.path()).await;
        assert!(out.is_error, "the option-shaped base is a soft error");
        let text = out.text_content();
        assert!(
            text.contains("Invalid base commit") && text.contains("read by git as an option"),
            "the message tells the model what a base is for: {text}"
        );
        assert!(
            !outside.exists(),
            "git never receives the value as an option"
        );
    }

    #[tokio::test]
    async fn an_oversized_diff_truncates_with_a_marker() {
        let repo = init_repo();
        std::fs::write(repo.path().join("large.txt"), "x".repeat(1_050_000)).unwrap();
        git(repo.path(), &["add", "large.txt"]);
        let out = call(submit_args(&[("format", json!("unified"))]), repo.path()).await;
        let text = out.text_content();
        assert!(text.starts_with("diff --git"), "{text}");
        assert!(text.contains("(truncated)"), "the cut is visible: {text}");
        assert!(
            text.len() < 1_010_000,
            "the body is bounded near the cap: {}",
            text.len()
        );
    }

    #[tokio::test]
    async fn a_test_run_that_cannot_spawn_reports_zero_counts() {
        let results = run_tests("/definitely/not/a/directory", None)
            .await
            .unwrap();
        assert_eq!(results.total, 0, "the unspawnable run counts nothing");
        assert_eq!(
            results.output_summary, "Tests could not be run",
            "the soft result says so in its summary"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_base_yields_an_empty_diff_body() {
        let repo = init_repo();
        let diff = git_diff_content(repo.path().to_str().unwrap(), "not-a-ref")
            .await
            .unwrap();
        assert!(
            diff.is_empty(),
            "a non-zero git diff exit yields an empty body: {diff}"
        );
    }

    #[tokio::test]
    async fn the_file_list_cap_announces_the_rest() {
        let repo = init_repo();
        for index in 0..25 {
            std::fs::write(repo.path().join(format!("extra{index:02}.txt")), "x\n").unwrap();
        }
        git(repo.path(), &["add", "."]);
        let summary = call(submit_args(&[("format", json!("summary"))]), repo.path())
            .await
            .text_content();
        assert!(
            summary.contains("extra18.txt"),
            "the last file inside the cap lists: {summary}"
        );
        assert!(
            !summary.contains("extra19.txt"),
            "the first file past the cap does not: {summary}"
        );
        assert!(
            summary.contains("... and 7 more"),
            "the summary renderer announces the hidden files (27 total, 20 listed): {summary}"
        );
        let github = call(submit_args(&[]), repo.path()).await.text_content();
        assert!(
            github.contains("... and 7 more"),
            "the github renderer announces them too: {github}"
        );
    }

    #[test]
    fn the_failed_tests_list_caps_at_ten() {
        let names: Vec<String> = (0..12).map(|index| format!("case_{index:02}")).collect();
        let results = TestResults {
            framework: "cargo".to_string(),
            total: 12,
            passed: 0,
            failed: 12,
            skipped: 0,
            failures: names,
            output_summary: String::new(),
        };
        let rendered = format_github_patch(
            &["src/a.rs".to_string()],
            "diff --git a/src/a.rs b/src/a.rs",
            1,
            1,
            None,
            Some(&results),
            true,
        );
        assert!(rendered.contains("case_09"), "the tenth failure lists");
        assert!(!rendered.contains("case_10"), "the eleventh does not");
        assert!(
            rendered.contains("... and 2 more"),
            "the cap announces the hidden failures"
        );
    }

    #[tokio::test]
    async fn the_summary_block_can_be_disabled() {
        let repo = init_repo();
        let out = call(submit_args(&[("summary", json!(false))]), repo.path()).await;
        let text = out.text_content();
        assert!(text.contains("### Test Results"), "{text}");
        assert!(text.contains("### Diff"), "{text}");
        assert!(!text.contains("### Changes"), "{text}");
        assert!(!text.contains("### Modified Files"), "{text}");
    }
}
