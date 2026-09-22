//! The Bash tool — executes shell commands with timeout and background jobs.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::context::RunnerContext;
use crate::context::require_cwd;
use crate::context::runner_ctx;
use crate::input::get_u64;
use crate::jobs::command_summary;
use crate::jobs::spawn_background_job;

/// Default command timeout in seconds.
///
/// Used when the model's input omits `timeout`; the same default bounds
/// background jobs spawned without one.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Hard ceiling on a command timeout.
///
/// Larger input values are clamped rather than rejected, so an
/// over-ambitious request still runs with the maximum allowed wait.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Per-stream cap on captured stdout or stderr, in bytes.
///
/// Each stream is read independently under this cap and drained past
/// it, so a command's total retained output is bounded by twice this
/// plus the join and metadata line. Shared with the Jobs tool, whose
/// stored payloads and renderings answer to the same bound.
pub(crate) const MAX_OUTPUT_BYTES: usize = 1_000_000;

/// The marker appended wherever captured output was capped.
///
/// One wording across every path — the live command's streams, a
/// render-time over-cap join, a stored job payload's middle cut — so a
/// consumer keying on the marker finds all of them.
pub(crate) const TRUNCATION_MARKER: &str = "...[output truncated]";

/// Commands that are safe to run concurrently (read-only).
///
/// Matched as boundary-aware prefixes of the trimmed command — `cargo
/// check` qualifies, `cargo checkout` does not. A command matching
/// here, with no shell operator or unsafe substring, dispatches in
/// parallel with other reads.
const READ_ONLY_PREFIXES: &[&str] = &[
    "cat",
    "ls",
    "ll",
    "grep",
    "find",
    "head",
    "tail",
    "wc",
    "echo",
    "pwd",
    "which",
    "file",
    "stat",
    "git status",
    "git diff",
    "git log",
    "git branch",
    "git show",
    "git remote",
    "cargo check",
    "cargo test --no-run",
    "cargo clippy --no-deps",
    "make -n",
];

/// Shell operators that indicate a compound command (always unsafe).
///
/// Any one of these disqualifies a command from concurrent execution
/// regardless of the words around it — a pipeline or redirection can
/// have side effects even when every word looks read-only.
const SHELL_OPERATORS: &[&str] = &["&&", "||", ";", "|", "`", "$(", ">", ">>", "<"];

/// Substrings that make an otherwise-allowlisted command unsafe.
///
/// Guard against mutating flags and subcommands hiding inside an
/// allowlisted prefix, such as `find -delete`, find's file-writing
/// `-fls`/`-fprint*` flags, `git diff --output` writing its output to a
/// file in either the `=`-joined or space-separated form, or
/// `git branch -D`.
const UNSAFE_SUBSTRINGS: &[&str] = &[
    " -delete",
    " -exec",
    " -fls",
    " -fprint",
    " --output",
    "git branch -D",
    "git branch -d",
    "git branch --delete",
    "git remote add",
    "git remote remove",
    "git remote rm",
    "git remote set-url",
    "git remote rename",
];

/// RAII guard that kills a child's process group on drop.
///
/// On timeout cancellation, `tokio::time::timeout` drops the future, dropping
/// the `Child` (SIGKILL to the direct child) and this guard (SIGKILL to the
/// whole process group). This ensures sub-shells, pipelines, and `sleep`
/// grandchildren die too — not just the `bash` child. A reaped child
/// disarms the guard, so a helper the command backgrounded **with its
/// output redirected away from the pipes** survives the tool's return.
/// A bare `cmd &` does not: the helper inherits the pipe write ends,
/// so the tool blocks on EOF until the helper exits — and on timeout
/// the still-armed guard kills it with the group.
#[cfg(unix)]
struct ChildGuard {
    /// Process-group ID of the child, when it was successfully spawned into its own group.
    ///
    /// `None` when no live group exists to signal — the guard then has
    /// nothing to do on drop. The guard stores the PGID rather than the PID
    /// because killing the negated PGID reaches the whole group (sub-shells,
    /// pipelines, and grandchildren), not just the direct `bash` child.
    pgid: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ChildGuard {
    /// Disarm the drop-kill.
    ///
    /// Called once the child has been reaped. From that point the
    /// process group belongs to whatever the command left running in it,
    /// so the guard must not signal it — neither to kill a deliberately
    /// backgrounded helper nor a group whose ID a new process may
    /// already have recycled.
    fn disarm(&mut self) {
        self.pgid = None;
    }
}

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            // SAFETY: a negative pid makes libc::kill signal the entire
            // process group (the standard Unix pgroup-kill idiom); SIGKILL
            // delivery cannot fail on a live group in a way the caller could
            // recover from, so the return value is deliberately ignored.
            unsafe {
                libc::kill(pgid.wrapping_neg(), libc::SIGKILL);
            }
        }
    }
}

/// Execute a bash command.
///
/// Supports background jobs, timeout enforcement, and a dynamic concurrency
/// check (read-only commands are safe to run concurrently). Not read-only
/// overall: any command that fails the concurrency check runs serialized
/// against other writes.
pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Execute a bash command. Supports background jobs, timeout enforcement, \
         and a dynamic concurrency check (read-only commands are safe to run \
         concurrently)."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to execute."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run command in the background and return a job ID immediately",
                        "default": false
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Command timeout in seconds (default: 120, max: 600)",
                        "default": 120,
                        "minimum": 1,
                        "maximum": 600
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        Box::pin(self.call_inner(input, rc))
    }

    fn is_safe_for_concurrent_execution(&self, input: &Value) -> bool {
        is_read_only_command(input)
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn system_prompt(&self) -> Option<String> {
        Some(
            "Run commands via the Bash tool. Prefer specific commands over \
              scripts. Never run destructive commands (`rm -rf /`, force-push) \
              without stating intent first. Background jobs you expect to \
              outlast ten minutes cannot finish — the timeout bounds them at \
              600 seconds; split the work or checkpoint and resume. Manage \
              and poll background jobs through the Jobs tool."
                .to_string(),
        )
    }
}

impl BashTool {
    /// Body of [`Tool::call`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] for a missing `RunnerContext`, a missing
    /// `command`, a negative or zero `timeout`, or a failure to spawn the
    /// subprocess.
    async fn call_inner(
        &self,
        input: Value,
        runner_context: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let cwd = require_cwd(runner_context)?.to_string_lossy().to_string();

        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing command".to_string()))?;
        let timeout_secs = get_u64(&input, "timeout")?
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        if timeout_secs == 0 {
            return Err(ToolError::InvalidInput(
                "'timeout' must be at least 1, got 0".to_string(),
            ));
        }

        if input
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let id = spawn_background_job(command, &cwd, timeout_secs);
            let summary = command_summary(command);
            return Ok(ToolOutput::text(format!(
                "Started background job {id}: {summary}"
            )));
        }

        let timeout = Duration::from_secs(timeout_secs);
        let exec = Box::pin(execute_command(command, &cwd));

        match tokio::time::timeout(timeout, exec).await {
            Ok(result) => result,
            Err(_) => Ok(ToolOutput::error_text(format!(
                "Command timed out after {timeout_secs} seconds"
            ))),
        }
    }
}

/// Execute a command in the foreground, capturing stdout + stderr.
///
/// Spawns `bash -c <command>` in a new process group, reads both pipes
/// concurrently with a per-stream cap of [`MAX_OUTPUT_BYTES`], and returns the
/// combined output with an `[exit {code}, {duration_ms}ms]` metadata line
/// appended. Once a stream's cap is reached it is drained but no longer
/// retained, preventing unbounded memory growth from commands that produce
/// gigabytes of output.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] if the process fails to spawn or wait
/// fails. Also the engine behind background jobs: the Jobs tool's
/// detached completion tasks run commands through here.
pub(crate) async fn execute_command(command: &str, cwd: &str) -> Result<ToolOutput, ToolError> {
    let start = Instant::now();
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("Failed to spawn command: {e}")))?;

    #[cfg(unix)]
    let mut guard = ChildGuard {
        pgid: child.id().and_then(|id| libc::pid_t::try_from(id).ok()),
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let ((stdout, stdout_cut), (stderr, stderr_cut)) = tokio::join!(
        async {
            match stdout {
                Some(mut s) => read_bounded(&mut s, MAX_OUTPUT_BYTES).await,
                None => (String::new(), false),
            }
        },
        async {
            match stderr {
                Some(mut s) => read_bounded(&mut s, MAX_OUTPUT_BYTES).await,
                None => (String::new(), false),
            }
        }
    );
    let status = child
        .wait()
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to wait for command: {e}")))?;
    #[cfg(unix)]
    guard.disarm();
    let duration_ms = start.elapsed().as_millis();
    let exit_code = status.code().unwrap_or(-1);
    let mut body = if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    };
    if stdout_cut || stderr_cut {
        body.push('\n');
        body.push_str(TRUNCATION_MARKER);
    }

    let output_text = format!("{body}\n[exit {exit_code}, {duration_ms}ms]");
    if status.success() {
        Ok(ToolOutput::text(output_text))
    } else {
        Ok(ToolOutput::error_text(output_text))
    }
}

/// Read a child pipe into a `String`, capping retained data at `max_bytes`.
///
/// Once the cap is reached, the remaining output is drained to EOF (so the
/// pipe doesn't block the child) but not stored, preventing unbounded memory
/// growth from commands that produce gigabytes of output. The lossy
/// UTF-8 conversion can itself overshoot the cap — invalid bytes expand
/// up to three-to-one under replacement — so the converted text is
/// re-cut to the cap here. The returned flag reports either overflow, so
/// the caller can mark the truncation for the model instead of cutting
/// silently.
async fn read_bounded<R>(stream: &mut R, max_bytes: usize) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(8192);
    let mut truncated = false;
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < max_bytes {
                    let room = max_bytes.saturating_sub(buf.len());
                    if let Some(chunk) = tmp.get(..n.min(room)) {
                        buf.extend_from_slice(chunk);
                    }
                    if n > room {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if text.len() > max_bytes {
        truncated = true;
        let mut cut = max_bytes;
        while !text.is_char_boundary(cut) && cut > 0 {
            cut = cut.saturating_sub(1);
        }
        text.truncate(cut);
    }
    (text, truncated)
}

/// Truncate `s` in place to at most `max_bytes`, landing on a UTF-8 char boundary.
///
/// If `s` already fits, it is left untouched. Otherwise the cut point walks
/// back from `max_bytes` to the preceding char boundary so the result stays
/// valid UTF-8, the tail is dropped, and a [`TRUNCATION_MARKER`] is appended
/// so the model can see the output was capped. Used on renderings that
/// combine independently capped streams — a job payload joining capped
/// stdout and stderr can exceed the cap by construction; the live command
/// path marks its own truncation at the source instead, and a stored job
/// payload is capped before it ever reaches here.
pub(crate) fn truncate_string(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while !s.is_char_boundary(cut) && cut > 0 {
        cut = cut.saturating_sub(1);
    }
    s.truncate(cut);
    s.push_str(TRUNCATION_MARKER);
}

/// Check whether a command is read-only (safe to run concurrently).
///
/// Compound commands (containing shell operators), shell redirections, and
/// destructive subcommands are always unsafe. Otherwise the command is checked
/// against the read-only prefix allowlist with boundary-aware matching so
/// `cargo check` matches but `cargo checkout` does not.
fn is_read_only_command(input: &Value) -> bool {
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return false;
    };
    let normalized = shell_normalized(command);
    if normalized.is_empty() {
        return false;
    }
    if SHELL_OPERATORS.iter().any(|op| normalized.contains(op)) {
        return false;
    }
    if UNSAFE_SUBSTRINGS.iter().any(|sub| normalized.contains(sub)) {
        return false;
    }
    READ_ONLY_PREFIXES.iter().any(|prefix| {
        if normalized.len() == prefix.len() {
            return normalized == *prefix;
        }
        if normalized.len() > prefix.len() {
            return normalized.starts_with(prefix)
                && normalized[prefix.len()..].starts_with(char::is_whitespace);
        }
        false
    })
}

/// Collapse a command the way the shell would tokenize it for matching.
///
/// Whitespace runs become single spaces and quote characters and
/// backslash escapes are dropped, so a tab-separated, quoted, or
/// escaped argument cannot slip an unsafe flag past checks that match
/// space-separated text — `find\t.\t-delete`, `find . "-delete"`, and
/// `git branch \-D` normalize to the same literal forms the denylist
/// guards.
fn shell_normalized(command: &str) -> String {
    command
        .replace(['"', '\'', '\\'], "")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// Shared fixtures for the Bash and Jobs tool tests.
///
/// The Jobs tests spawn commands through Bash and assert on the same
/// bounded-output contract, so the context factory and the output-cap
/// assertion live here where both modules reach them.
#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::arithmetic_side_effects,
    clippy::field_reassign_with_default
)]
pub(crate) mod test_support {
    use loopctl::tool::ToolContext;

    use crate::bash::MAX_OUTPUT_BYTES;
    use crate::context::RunnerContext;

    /// The last `n` characters of `text`, for readable failure output.
    pub(crate) fn tail_of(text: &str, n: usize) -> String {
        let skip = text.chars().count().saturating_sub(n);
        text.chars().skip(skip).collect()
    }

    /// Assert combined output stayed under the per-stream cap plus a
    /// small margin for the stdout/stderr join and the trailing `[exit …]`
    /// metadata line, showing the tail when it did not.
    ///
    /// # Panics
    ///
    /// Panics when `text` exceeds the cap plus `margin`, naming the tail.
    pub(crate) fn assert_bounded(text: &str, margin: usize) {
        assert!(
            text.len() < MAX_OUTPUT_BYTES + margin,
            "output was {} bytes (cap ~{} + margin {}), tail: {:?}",
            text.len(),
            MAX_OUTPUT_BYTES,
            margin,
            tail_of(text, 120)
        );
    }

    /// A tool context whose cwd and [`RunnerContext`] both point at `cwd`.
    pub(crate) fn ctx_in(cwd: &str) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string();
        ctx.set_extension(RunnerContext::new(std::path::PathBuf::from(cwd)));
        ctx
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    clippy::map_unwrap_or
)]
mod tests {
    use super::test_support::{assert_bounded, ctx_in, tail_of};
    use super::*;
    use crate::jobs::MAX_COMMAND_SUMMARY_BYTES;

    // The job table lives in the jobs module; its test fixtures own the
    // guard that serializes every job-table touch, spawn tests here
    // included.
    use crate::jobs::test_support::JOB_TEST_GUARD;

    #[test]
    fn concurrency_check_allowlist_hits() {
        for cmd in [
            "cat f",
            "ls -la",
            "grep x .",
            "git status",
            "git diff",
            "git log",
            "git branch",
            "git show",
            "git remote",
            "cargo check",
            "cargo test --no-run",
            "cargo clippy --no-deps",
            "make -n",
            "head -5 f",
            "wc -l f",
            "echo hi",
            "pwd",
            "which bash",
            "file x",
            "stat x",
        ] {
            assert!(
                is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should be read-only"
            );
        }
    }

    #[test]
    fn concurrency_check_write_commands() {
        for cmd in [
            "rm x",
            "cargo build",
            "cargo run",
            "make install",
            "git commit",
            "git push",
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only"
            );
        }
    }

    #[test]
    fn concurrency_check_boundary_correctness() {
        // cargo checkout → not safe (checkout mutates), proves boundary check.
        assert!(!is_read_only_command(
            &json!({ "command": "cargo checkout" })
        ));
        // git show → safe; but with a pipe → unsafe.
        assert!(is_read_only_command(&json!({ "command": "git show" })));
        assert!(!is_read_only_command(
            &json!({ "command": "git show | tee log" })
        ));
    }

    #[test]
    fn concurrency_check_compound_commands_unsafe() {
        for cmd in [
            "cat a && rm b",
            "echo x | tee y",
            "ls ; rm z",
            "echo $(whoami)",
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only (compound)"
            );
        }
    }

    #[test]
    fn concurrency_check_redirections_unsafe() {
        for cmd in [
            "echo hi > file",
            "echo hi >> file",
            "cat f < input",
            "git log > out.txt",
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only (redirection)"
            );
        }
    }

    #[test]
    fn concurrency_check_find_mutating_unsafe() {
        for cmd in [
            "find . -delete",
            "find / -exec rm {} \\;",
            "find . -name '*.tmp' -delete",
            "find . -fls /tmp/out",
            "find . -fprint /tmp/names",
            "find . -fprint0 /tmp/names0",
            "find . -fprintf /tmp/report '\\n'",
            "find\t.\t-delete",
            "find . \"-delete\"",
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only (mutating find)"
            );
        }
    }

    #[test]
    fn concurrency_check_whitespace_and_quotes_normalized() {
        for cmd in [
            "git branch\t-D\tmain",
            "git remote\tadd\torigin\turl",
            "git diff\t--output=/tmp/patch",
            "git branch \"-D\" main",
            "git branch \\-D main",
            "find . \\-delete",
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only (separator/quote-normalized)"
            );
        }
        assert!(
            is_read_only_command(&json!({ "command": "git\tstatus" })),
            "a tab-separated safe command stays read-only once normalized"
        );
    }

    #[test]
    fn concurrency_check_git_output_flag_unsafe() {
        assert!(
            !is_read_only_command(&json!({ "command": "git diff --output=/tmp/patch" })),
            "git diff --output writes a file and must not be read-only"
        );
        assert!(
            !is_read_only_command(&json!({ "command": "git log --output=/tmp/log" })),
            "git log --output writes a file and must not be read-only"
        );
        for cmd in ["git diff --output /tmp/patch", "git log --output /tmp/log"] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' writes a file in its space-separated form and must not be read-only"
            );
        }
    }

    #[test]
    fn concurrency_check_git_mutating_subcommands_unsafe() {
        for cmd in [
            "git branch -D feature",
            "git branch -d old",
            "git branch --delete stale",
            "git remote add origin url",
            "git remote remove upstream",
            "git remote set-url origin url",
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only (mutating git)"
            );
        }
    }

    #[test]
    fn concurrency_check_safe_git_still_allowed() {
        // Read-only git subcommands remain safe after the denylist.
        assert!(is_read_only_command(&json!({ "command": "git branch" })));
        assert!(is_read_only_command(
            &json!({ "command": "git branch --list" })
        ));
        assert!(is_read_only_command(&json!({ "command": "git remote" })));
        assert!(is_read_only_command(&json!({ "command": "git remote -v" })));
    }

    #[test]
    fn concurrency_check_missing_command() {
        assert!(!is_read_only_command(&json!({})));
        assert!(!is_read_only_command(&json!({ "command": "" })));
    }

    #[test]
    fn schema_has_v1_properties() {
        let schema = BashTool.schema();
        let props = schema
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(props.contains_key("command"));
        assert!(props.contains_key("background"));
        assert!(props.contains_key("timeout"));
        // Job management belongs to the Jobs tool — Bash never asks for a
        // field it would ignore.
        assert!(!props.contains_key("operation"));
        assert!(!props.contains_key("job_id"));
        // No Docker fields.
        assert!(!props.contains_key("use_docker"));
        assert!(!props.contains_key("docker_image"));
        assert!(!props.contains_key("work_dir"));

        let required = schema
            .input_schema
            .get("required")
            .unwrap()
            .as_array()
            .unwrap();
        // `command` is the single required field. Local/small models (and the
        // JSON-schema-to-grammar conversion used by llama.cpp-style servers)
        // do not honour `anyOf`, and an empty `required` lets them emit `{}`.
        assert_eq!(
            required.len(),
            1,
            "command must be the single required field: {required:?}"
        );
        assert_eq!(required[0], "command");
        assert!(
            schema.input_schema.get("anyOf").is_none(),
            "anyOf alternatives are not understood by local models; keep it out"
        );
    }

    #[test]
    fn constants_match_spec() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 120);
        assert_eq!(MAX_TIMEOUT_SECS, 600);
        assert_eq!(MAX_OUTPUT_BYTES, 1_000_000);
    }

    #[test]
    fn truncate_string_cuts_at_boundary() {
        let mut s = "hello".repeat(300_000); // 1.5 MB
        truncate_string(&mut s, MAX_OUTPUT_BYTES);
        // +24 covers the truncation suffix.
        assert!(s.len() <= MAX_OUTPUT_BYTES + 24);
        assert!(s.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn truncate_string_multibyte_boundary() {
        let mut s = "€".repeat(400_000); // 1.2 MB, multibyte
        truncate_string(&mut s, MAX_OUTPUT_BYTES);
        // Must land on a char boundary — no panic from String::truncate.
        assert!(s.len() <= MAX_OUTPUT_BYTES + 24);
    }

    #[tokio::test]
    async fn read_bounded_grows_past_initial_capacity() {
        // Initial capacity is 8192; data larger than that must still be fully
        // retained when under the cap. Proves the Vec grows dynamically.
        use std::io::Cursor;
        let data = "x".repeat(50_000); // well past 8192, well under MAX_OUTPUT_BYTES
        let mut cursor = Cursor::new(data.clone().into_bytes());
        let (result, truncated) = read_bounded(&mut cursor, MAX_OUTPUT_BYTES).await;
        assert_eq!(result, data, "all data should be retained");
        assert!(!truncated, "under-cap data must not report overflow");
    }

    #[tokio::test]
    async fn read_bounded_caps_at_max_bytes() {
        use std::io::Cursor;
        let data = "y".repeat(100_000);
        let mut cursor = Cursor::new(data.into_bytes());
        let (result, truncated) = read_bounded(&mut cursor, 10_000).await;
        assert!(
            result.len() <= 10_000,
            "retained {} bytes, should be <= 10000",
            result.len()
        );
        assert!(result.chars().all(|c| c == 'y'));
        assert!(truncated, "100k bytes into a 10k cap must report overflow");
    }

    #[tokio::test]
    async fn read_bounded_small_data_preserved() {
        use std::io::Cursor;
        let data = "hello world".to_string();
        let mut cursor = Cursor::new(data.clone().into_bytes());
        let (result, truncated) = read_bounded(&mut cursor, MAX_OUTPUT_BYTES).await;
        assert_eq!(result, data);
        assert!(!truncated);
    }

    #[tokio::test]
    async fn read_bounded_caps_the_lossy_expansion_of_invalid_utf8() {
        use std::io::Cursor;
        // 5k invalid bytes sit under a 10k raw cap but expand to ~15k of
        // replacement characters — the conversion itself must be re-cut.
        let mut cursor = Cursor::new(vec![0xFFu8; 5_000]);
        let (text, truncated) = read_bounded(&mut cursor, 10_000).await;
        assert!(
            truncated,
            "lossy expansion past the cap must report overflow"
        );
        assert!(
            text.len() <= 10_000,
            "retained {} bytes after conversion, cap is 10000",
            text.len()
        );
    }

    #[tokio::test]
    async fn read_bounded_drains_after_cap() {
        // When the cap is hit, the reader must drain to EOF (so the child's
        // pipe doesn't block) without storing more data. We verify this
        // indirectly: the Cursor is fully consumed (position at end) even
        // though only `cap` bytes were retained.
        use std::io::Cursor;
        let raw = vec![b'a'; 50_000];
        let mut cursor = Cursor::new(raw.clone());
        let (result, truncated) = read_bounded(&mut cursor, 1_000).await;
        assert!(result.len() <= 1_000);
        assert!(truncated, "50k bytes into a 1k cap must report overflow");
        // Cursor position should be at EOF — the reader drained the rest.
        assert_eq!(cursor.position(), 50_000);
    }

    #[test]
    fn bashtool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("Bash").expect("BashTool registered");
        assert!(!tool.is_read_only());
        // Dynamic concurrency: read-only input is safe, write is not.
        assert!(tool.is_safe_for_concurrent_execution(&json!({"command": "ls"})));
        assert!(!tool.is_safe_for_concurrent_execution(&json!({"command": "rm x"})));
    }

    #[test]
    fn system_prompt_present() {
        let prompt = BashTool.system_prompt();
        assert!(prompt.is_some());
        assert!(prompt.unwrap().contains("Bash"));
    }

    #[tokio::test]
    async fn echo_returns_stdout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "echo hello" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.text_content().contains("hello"));
        // Metadata line present.
        assert!(out.text_content().contains("[exit 0,"));
    }

    #[tokio::test]
    async fn failing_command_includes_exit_code() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "exit 3" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("[exit 3,"));
    }

    #[tokio::test]
    async fn stdout_and_stderr_combined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "echo out; echo err 1>&2" });
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("out"));
        assert!(text.contains("err"));
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "sleep 30", "timeout": 1 });
        let start = Instant::now();
        let out = tool.call(input, &ctx).await.unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_error);
        assert!(
            out.text_content().contains("timed out"),
            "{}",
            out.text_content()
        );
        // Should return well within the sleep duration.
        assert!(elapsed.as_secs() < 10, "took {elapsed:?}");
    }

    #[tokio::test]
    async fn timeout_kills_pipeline() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "sleep 30 | cat", "timeout": 1 });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("timed out"));
    }

    #[tokio::test]
    async fn output_truncation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        // ~2 MB of output.
        let input = json!({ "command": "yes y | head -c 2000000" });
        let out = tool.call(input, &ctx).await.unwrap();
        // Output + metadata line should be under the cap + a small margin.
        assert_bounded(&out.text_content(), 512);
    }

    #[tokio::test]
    async fn bounded_read_does_not_exhaust_memory() {
        // Produce far more than MAX_OUTPUT_BYTES (10 MB of 'y\n').
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "yes y | head -c 10000000" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert_bounded(&out.text_content(), 512);
    }

    #[tokio::test]
    async fn bounded_read_retains_content_within_cap() {
        // Output well within the cap — all content should be present.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "echo 'small output'" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.text_content().contains("small output"));
    }

    #[tokio::test]
    async fn bounded_read_stderr_independently_capped() {
        // stdout is tiny; stderr is large — stderr must be independently bounded.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "echo ok && dd if=/dev/zero bs=2000 count=1000 1>&2" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert_bounded(&out.text_content(), 200);
    }

    #[tokio::test]
    async fn capped_output_carries_a_truncation_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "yes y | head -c 2000000" });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(
            out.text_content().contains("...[output truncated]"),
            "a capped stream must say so, tail: {:?}",
            tail_of(&out.text_content(), 200)
        );
    }

    #[tokio::test]
    async fn a_detached_helper_survives_a_successful_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let marker = tmp.path().join("late-marker");
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "command": format!("(sleep 0.4 && touch {}) & echo started", marker.display())
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            marker.exists(),
            "a helper detached before the tool returned must not be group-killed"
        );
    }

    #[tokio::test]
    async fn a_redirected_helper_outlives_the_tool_call_without_blocking_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let marker = tmp.path().join("late-marker");
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        // The subshell's stdio is redirected to /dev/null, so it holds
        // none of the tool's pipes: the tool must return at the shell's
        // exit, long before the helper's 3 s sleep ends.
        let input = json!({
            "command": format!(
                "(sleep 3 && touch {}) >/dev/null 2>&1 &",
                marker.display()
            )
        });
        let started = Instant::now();
        let out = tool.call(input, &ctx).await.unwrap();
        let elapsed = started.elapsed();
        assert!(!out.is_error);
        assert!(
            elapsed < Duration::from_secs(2),
            "the tool must not block on a redirected helper's lifetime: {elapsed:?}"
        );
        assert!(
            !marker.exists(),
            "the helper is still asleep when the tool returns"
        );
        let deadline = Instant::now() + Duration::from_secs(6);
        while !marker.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            marker.exists(),
            "the redirected helper survives the tool's return"
        );
    }

    #[tokio::test]
    async fn bounded_read_pipe_does_not_block_child() {
        // A command that writes more than MAX_OUTPUT_BYTES then exits
        // successfully — the child must not hang waiting for the reader to
        // consume the full pipe. (The bounded reader drains to EOF even after
        // the cap is hit.)
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "command": "for i in $(seq 1 300000); do echo line$i; done",
            "timeout": 10
        });
        let out = tokio::time::timeout(Duration::from_secs(15), tool.call(input, &ctx)).await;
        assert!(out.is_ok(), "command should not hang on a full pipe");
        let out = out.unwrap().unwrap();
        assert!(out.text_content().contains("[exit 0,")); // completed normally
    }

    #[tokio::test]
    async fn background_returns_job_id() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "echo bgdone", "background": true });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.text_content().contains("Started background job"));
    }

    #[tokio::test]
    async fn a_background_acknowledgement_is_bounded() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let long = "x".repeat(MAX_COMMAND_SUMMARY_BYTES * 3);
        let input = json!({ "command": format!("echo {long}"), "background": true });
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            text.contains("Started background job"),
            "the acknowledgement still names the job: {text}"
        );
        assert!(
            text.len() < MAX_COMMAND_SUMMARY_BYTES * 2,
            "the acknowledgement must not echo a pathological command back \
             into the model's context: {} bytes",
            text.len()
        );
        assert!(
            text.contains(TRUNCATION_MARKER),
            "the elided command carries the shared marker: {text}"
        );
    }

    #[tokio::test]
    async fn missing_command_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({});
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn zero_timeout_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "true", "timeout": 0 });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("'timeout'")),
            "zero timeout should error: {err:?}"
        );
    }

    #[tokio::test]
    async fn negative_timeout_rejected_not_silently_defaulted() {
        // Regression: a negative timeout must error loudly, not silently
        // become the 120 s default (the model's explicit intent erased).
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "true", "timeout": -1 });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("'timeout'")),
            "negative timeout should name the field: {err:?}"
        );
    }
}
