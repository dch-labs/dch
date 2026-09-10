//! The Bash tool — executes shell commands with timeout and background jobs.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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
/// plus the join and metadata line.
const MAX_OUTPUT_BYTES: usize = 1_000_000;

/// The marker appended wherever captured output was capped.
///
/// One wording across every path — the live command's streams, a
/// render-time over-cap join, a stored job payload's middle cut — so a
/// consumer keying on the marker finds all of them.
const TRUNCATION_MARKER: &str = "...[output truncated]";

/// Display bound for a command rendered in job output, in bytes.
///
/// The `jobs` listing and the `job_status` header interpolate the
/// command verbatim, and nothing bounds the model's input — a huge
/// command would re-open the unbounded listing hole the payload work
/// closed, and would outgrow the header room [`cap_job_payload`]
/// reserves. Renderings show at most this many bytes of it.
const MAX_COMMAND_SUMMARY_BYTES: usize = 256;

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
/// `-fls`/`-fprint*` flags, `git diff --output=` writing its output to a
/// file, or `git branch -D`.
const UNSAFE_SUBSTRINGS: &[&str] = &[
    " -delete",
    " -exec",
    " -fls",
    " -fprint",
    " --output=",
    "git branch -D",
    "git branch -d",
    "git branch --delete",
    "git remote add",
    "git remote remove",
    "git remote rm",
    "git remote set-url",
    "git remote rename",
];

/// Status of a background job.
///
/// Stored inside [`BackgroundJob`] in the global job table. Transitions are
/// one-way: a job starts [`Running`](Self::Running), then moves to either
/// [`Completed`](Self::Completed) or [`Failed`](Self::Failed) when the process
/// exits or the timeout fires. Once terminal, the status never changes again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JobStatus {
    /// Still running.
    ///
    /// No output is available yet — the process has not exited. The payload is
    /// absent because stdout/stderr are collected only when the job finishes.
    /// Polled by `job_status` until it transitions to a terminal variant.
    Running,

    /// Finished successfully.
    ///
    /// The payload is the job's final tool output: stdout followed by stderr,
    /// plus the `[exit …, …ms]` metadata line. Returned to the model when it
    /// polls a completed job.
    Completed(String),

    /// Failed or timed out.
    ///
    /// The payload is whatever output was captured before termination — which
    /// may be partial if the process was killed by the timeout. If no output
    /// was produced (e.g. killed instantly), the payload is an error or
    /// timeout message instead.
    Failed(String),
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("Running"),
            Self::Completed(payload) => {
                write!(f, "Completed: {payload}")
            }
            Self::Failed(payload) => {
                write!(f, "Failed: {payload}")
            }
        }
    }
}

/// One tracked background job.
///
/// Stored in the global job table keyed by `id`. Created by
/// `spawn_background_job` when the Bash tool runs with `background: true`;
/// updated by the process-wait future when the job exits or times out.
#[derive(Debug, Clone)]
struct BackgroundJob {
    /// Monotonic job identifier.
    ///
    /// Allocated from the global ID counter at spawn time and never reused.
    /// The model uses this to poll status via `operation: "job_status"`.
    id: u64,

    /// The command string.
    ///
    /// Stored verbatim (exactly as the model supplied it) for display in the
    /// `jobs` listing. Not used for execution — that happens at spawn time.
    command: String,

    /// Current status of the job.
    ///
    /// Polled by `job_status` on each request. Updated in place when the
    /// process exits (success or failure) or when the timeout fires — the job
    /// table entry is mutated under the table's mutex.
    status: JobStatus,
}

/// Global background job table.
///
/// Keyed by the monotonic job ID and shared by every Bash call in the
/// process: spawns insert here, detached completion tasks update their
/// entries in place under the mutex, and the job operations read
/// clones. A poisoned lock is tolerated — accessors return empty
/// rather than failing the tool call.
static JOB_TABLE: LazyLock<Mutex<BTreeMap<u64, BackgroundJob>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Monotonic counter for job IDs.
///
/// Relaxed increments at spawn time hand out IDs that are never
/// reused, so a stale ID from a finished, cleaned-up, or evicted job
/// can never resolve to a different job later.
static JOB_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// How many terminal jobs the table retains before the oldest are evicted.
///
/// Completed payloads are the table's memory cost; a session that never
/// calls `cleanup_jobs` would otherwise retain every job forever.
const MAX_TERMINAL_JOBS: usize = 20;

/// Spawn `command` as a background job and return its ID immediately.
///
/// Allocates a fresh monotonic ID, inserts a [`BackgroundJob`] in the
/// [`Running`](JobStatus::Running) state into [`JOB_TABLE`], and `tokio::spawn`s
/// a detached task that runs the command under [`execute_command`] capped at
/// `timeout_secs`. When the task finishes (success, failure, or timeout) it
/// updates the table entry in place to [`Completed`](JobStatus::Completed) or
/// [`Failed`](JobStatus::Failed); the caller never blocks on that transition —
/// it polls later via the `job_status` operation. A poisoned job-table lock is
/// tolerated: the spawn still returns the ID even if the entry could not be
/// recorded, matching the rest of the job-table accessors' handling.
fn spawn_background_job(command: &str, cwd: &str, timeout_secs: u64) -> u64 {
    let id = JOB_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let job = BackgroundJob {
        id,
        command: command.to_owned(),
        status: JobStatus::Running,
    };

    if let Ok(mut table) = JOB_TABLE.lock() {
        table.insert(id, job);
    }

    let owned_command = command.to_owned();
    let owned_cwd = cwd.to_owned();
    let timeout = Duration::from_secs(timeout_secs);

    tokio::spawn(async move {
        let exec = Box::pin(execute_command(&owned_command, &owned_cwd));
        let (mut text, is_error) = match tokio::time::timeout(timeout, exec).await {
            Ok(result) => result.map_or_else(
                |e| (e.to_string(), true),
                |o| (o.text_content(), o.is_error),
            ),
            Err(_) => (
                format!("Command timed out after {timeout_secs} seconds"),
                true,
            ),
        };
        cap_job_payload(&mut text);
        let status = if is_error {
            JobStatus::Failed(text)
        } else {
            JobStatus::Completed(text)
        };

        if let Ok(mut table) = JOB_TABLE.lock() {
            if let Some(job) = table.get_mut(&id) {
                job.status = status;
            }
            prune_terminal_jobs(&mut table);
        }
    });

    id
}

/// Retrieve a single background job by its ID.
///
/// Looks up the job in the global table under the table's mutex and returns a
/// clone of its current state. Returns `None` if the ID doesn't exist (the job
/// was never spawned, or already cleaned up via [`cleanup_jobs`]).
fn get_job(id: u64) -> Option<BackgroundJob> {
    JOB_TABLE.lock().ok()?.get(&id).cloned()
}

/// List all tracked background jobs.
///
/// Returns a clone of every entry in the global job table, in `BTreeMap` key
/// order (ascending job ID). Used by the `operation: "jobs"` path of the Bash
/// tool to show the model what's running and what has finished. Returns an
/// empty vec if the table is empty or the lock is poisoned.
fn list_jobs() -> Vec<BackgroundJob> {
    JOB_TABLE
        .lock()
        .map(|t| t.values().cloned().collect())
        .unwrap_or_default()
}

/// Remove terminal jobs (completed or failed) from the table.
///
/// Retains only [`JobStatus::Running`] entries, evicting everything else.
/// Returns the number of jobs removed. Used by the `operation: "cleanup_jobs"`
/// path of the Bash tool so the model can reclaim table space after polling
/// all results. Returns 0 if the lock is poisoned or no terminal jobs exist.
fn cleanup_jobs() -> usize {
    let Ok(mut table) = JOB_TABLE.lock() else {
        return 0;
    };
    let before = table.len();
    table.retain(|_, job| matches!(job.status, JobStatus::Running));

    before.saturating_sub(table.len())
}

/// Evict the oldest terminal jobs beyond [`MAX_TERMINAL_JOBS`].
///
/// Keys are monotonic job IDs, so ascending order is spawn order: the
/// oldest completed or failed payloads go first, and running jobs are
/// never evicted.
fn prune_terminal_jobs(table: &mut BTreeMap<u64, BackgroundJob>) {
    let excess = table
        .values()
        .filter(|job| !matches!(job.status, JobStatus::Running))
        .count()
        .saturating_sub(MAX_TERMINAL_JOBS);
    let evict: Vec<u64> = table
        .iter()
        .filter(|(_, job)| !matches!(job.status, JobStatus::Running))
        .map(|(id, _)| *id)
        .take(excess)
        .collect();
    for id in evict {
        table.remove(&id);
    }
}

/// A display-bounded rendering of a job's command.
///
/// The stored command stays verbatim; only renderings pass through
/// here, so a pathological command cannot outgrow the job output's
/// caps. Uses [`truncate_string`], so an elided command carries
/// [`TRUNCATION_MARKER`] like any other cut.
fn command_summary(command: &str) -> String {
    let mut summary = command.to_string();
    truncate_string(&mut summary, MAX_COMMAND_SUMMARY_BYTES);
    summary
}

/// A payload-free status rendering for the `jobs` listing.
///
/// The listing concatenates one line per job; inlining each terminal
/// payload would multiply the per-job cap into an unbounded listing, so
/// the listing reports sizes and leaves payloads to `job_status`.
fn job_summary(status: &JobStatus) -> String {
    match status {
        JobStatus::Running => "Running".to_string(),
        JobStatus::Completed(payload) => format!("Completed ({} bytes)", payload.len()),
        JobStatus::Failed(payload) => format!("Failed ({} bytes)", payload.len()),
    }
}

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
                        "description": "The command to execute"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run command in the background and return a job ID immediately",
                        "default": false
                    },
                    "operation": {
                        "type": "string",
                        "description": "Special operation (omit for normal command execution)",
                        "enum": ["jobs", "job_status", "cleanup_jobs"]
                    },
                    "job_id": {
                        "type": "integer",
                        "description": "Job ID to query (used with operation=job_status)"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Command timeout in seconds (default: 120, max: 600)",
                        "default": 120,
                        "minimum": 1,
                        "maximum": 600
                    }
                },
                "required": [],
                "anyOf": [
                    {"required": ["command"]},
                    {"required": ["operation"]}
                ]
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
             without stating intent first. Background long jobs."
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
    /// `command`, an unknown `operation`, a negative or zero `timeout`, or a
    /// failure to spawn the subprocess.
    async fn call_inner(
        &self,
        input: Value,
        runner_context: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let cwd = require_cwd(runner_context)?.to_string_lossy().to_string();

        if let Some(op) = input.get("operation").and_then(Value::as_str) {
            return dispatch_operation(op, &input);
        }

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
            return Ok(ToolOutput::text(format!(
                "Started background job {id}: {command}"
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

/// Handle a job-management `operation` request.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] for an unknown operation or when
/// `job_status` is called without a `job_id`.
fn dispatch_operation(operation: &str, input: &Value) -> Result<ToolOutput, ToolError> {
    match operation {
        "jobs" => {
            let jobs = list_jobs();
            let text = if jobs.is_empty() {
                "No background jobs.".to_string()
            } else {
                jobs.iter()
                    .map(|j| {
                        format!(
                            "  [{}] {} — {}",
                            j.id,
                            command_summary(&j.command),
                            job_summary(&j.status)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(ToolOutput::text(text))
        }
        "job_status" => {
            let id = input
                .get("job_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| ToolError::InvalidInput("job_status requires job_id".to_string()))?;
            match get_job(id) {
                Some(job) => {
                    let mut text = format!(
                        "[{}] {}: {}",
                        job.id,
                        command_summary(&job.command),
                        job.status
                    );
                    truncate_string(&mut text, MAX_OUTPUT_BYTES);
                    Ok(ToolOutput::text(text))
                }
                None => Ok(ToolOutput::error_text(format!("Job {id} not found"))),
            }
        }
        "cleanup_jobs" => {
            let removed = cleanup_jobs();
            Ok(ToolOutput::text(format!(
                "Removed {removed} completed/failed jobs."
            )))
        }
        _ => Err(ToolError::InvalidInput(format!(
            "Unknown operation: '{operation}'. Supported: jobs, job_status, cleanup_jobs."
        ))),
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
/// fails.
async fn execute_command(command: &str, cwd: &str) -> Result<ToolOutput, ToolError> {
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
/// payload is capped by [`cap_job_payload`] before it ever reaches here.
fn truncate_string(s: &mut String, max_bytes: usize) {
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

/// Cap a stored job payload, cutting from the middle so both ends
/// survive.
///
/// A completed job's text joins two independently capped streams, so it
/// can reach twice the render cap; a head-only cut would drop the
/// stderr tail and the `[exit …]` line — the parts a failed
/// high-output job is polled for. The middle cut keeps the head of the
/// stdout and the tail (the stderr end, any stream markers, the exit
/// metadata) with [`TRUNCATION_MARKER`] naming the elided span. The
/// halves are sized to leave room for the header `job_status` prepends
/// at render time — the id, the [`command_summary`]-bounded command,
/// and the status prefix stay under the reservation by construction —
/// so the render-time cut is a last-resort guard, not the plan. The
/// threshold carries the reservation too: a payload that lands just
/// under the cap but leaves no header room is cut like any other.
fn cap_job_payload(text: &mut String) {
    let reserved = 512usize
        .saturating_add(TRUNCATION_MARKER.len())
        .saturating_add(2);
    if text.len().saturating_add(reserved) <= MAX_OUTPUT_BYTES {
        return;
    }
    let keep = MAX_OUTPUT_BYTES.saturating_sub(reserved) / 2;
    let mut head_end = keep;
    while !text.is_char_boundary(head_end) && head_end > 0 {
        head_end = head_end.saturating_sub(1);
    }
    let mut tail_start = text.len().saturating_sub(keep);
    while !text.is_char_boundary(tail_start) && tail_start < text.len() {
        tail_start = tail_start.saturating_add(1);
    }
    let tail = text.split_off(tail_start);
    text.truncate(head_end);
    text.push('\n');
    text.push_str(TRUNCATION_MARKER);
    text.push('\n');
    text.push_str(&tail);
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
    let normalized = command.trim();
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
    use super::*;

    /// Serializes tests that touch the global job table, preventing cross-test
    /// interference from the shared `JOB_TABLE` / `JOB_ID_COUNTER`.
    static JOB_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Empty the global job table.
    ///
    /// A background job's completion task dies with its test's runtime, so
    /// an entry from an earlier test can linger as `Running` forever; tests
    /// that count table state start from a clean slate.
    fn reset_job_table() {
        if let Ok(mut table) = JOB_TABLE.lock() {
            table.clear();
        }
    }

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
        ] {
            assert!(
                !is_read_only_command(&json!({ "command": cmd })),
                "'{cmd}' should NOT be read-only (mutating find)"
            );
        }
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
        assert!(props.contains_key("operation"));
        assert!(props.contains_key("job_id"));
        assert!(props.contains_key("timeout"));
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
        // No field is universally required: `command` runs a command, while
        // `operation` manages background jobs and short-circuits before
        // `command` is read. The two are alternatives, enforced by anyOf.
        assert!(
            required.is_empty(),
            "required should be empty: {required:?}"
        );
        let any_of = schema
            .input_schema
            .get("anyOf")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(any_of.len(), 2, "anyOf should require command or operation");
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

    #[test]
    fn cap_job_payload_cuts_payloads_that_leave_no_header_room() {
        let mut s = "x".repeat(MAX_OUTPUT_BYTES - 100);
        cap_job_payload(&mut s);
        assert!(
            s.len() < MAX_OUTPUT_BYTES - 300,
            "the render header must fit after the cut, retained {}",
            s.len()
        );
        assert!(s.contains(TRUNCATION_MARKER));
    }

    #[test]
    fn cap_job_payload_leaves_small_payloads_untouched() {
        let mut s = "out\n[exit 0, 12ms]".to_string();
        cap_job_payload(&mut s);
        assert_eq!(s, "out\n[exit 0, 12ms]");
    }

    #[test]
    fn cap_job_payload_keeps_both_ends_and_the_exit_line() {
        // 1 MB of stdout head plus a stderr tail and exit line: together
        // past the cap, so the middle is elided.
        let mut s = format!(
            "{}\nboom: real failure text\n[exit 3, 45ms]",
            "x".repeat(MAX_OUTPUT_BYTES)
        );
        let original_head = s.get(..32).map(str::to_string).unwrap_or_default();
        cap_job_payload(&mut s);
        assert!(
            s.len() <= MAX_OUTPUT_BYTES - 512 + 16,
            "the capped payload leaves header room under the render cap: {}",
            s.len()
        );
        assert!(
            s.starts_with(&original_head),
            "the stdout head survives the middle cut"
        );
        assert!(
            s.ends_with("boom: real failure text\n[exit 3, 45ms]"),
            "the stderr tail and exit line must survive the middle cut"
        );
        assert!(
            s.contains(TRUNCATION_MARKER),
            "the elided middle is named by the shared marker"
        );
    }

    #[test]
    fn cap_job_payload_lands_on_char_boundaries() {
        let mut s = format!("€{}", "y".repeat(MAX_OUTPUT_BYTES));
        cap_job_payload(&mut s);
        // No panic from split_off/truncate on a multibyte head — both
        // cut points walked to boundaries first.
        assert!(s.contains(TRUNCATION_MARKER));
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

    fn tail_of(text: &str, n: usize) -> String {
        let skip = text.chars().count().saturating_sub(n);
        text.chars().skip(skip).collect()
    }

    /// Assert combined output stayed under the per-stream cap plus a
    /// small margin for the stdout/stderr join and the trailing `[exit …]`
    /// metadata line, showing the tail when it did not.
    fn assert_bounded(text: &str, margin: usize) {
        assert!(
            text.len() < MAX_OUTPUT_BYTES + margin,
            "output was {} bytes (cap ~{} + margin {}), tail: {:?}",
            text.len(),
            MAX_OUTPUT_BYTES,
            margin,
            tail_of(text, 120)
        );
    }

    fn ctx_in(cwd: &str) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string();
        ctx.set_extension(RunnerContext::new(std::path::PathBuf::from(cwd)));
        ctx
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
    async fn job_status_after_completion() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        // Spawn a background job.
        let spawn_input = json!({ "command": "echo bgdone", "background": true });
        let out = tool.call(spawn_input, &ctx).await.unwrap();
        let text = out.text_content();
        // Extract job id from "Started background job N: ...".
        let id: u64 = text
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        // Wait for completion.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let status_input = json!({ "command": "", "operation": "job_status", "job_id": id });
        let out = tool.call(status_input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            text.contains("Completed") && text.contains("bgdone"),
            "the job must reach the terminal status with its payload: {text}"
        );
    }

    #[tokio::test]
    async fn job_status_caps_the_stored_payload() {
        let _guard = JOB_TEST_GUARD.lock().await;
        reset_job_table();
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        let spawn_input = json!({ "command": "yes y | head -c 2000000", "background": true });
        let out = tool.call(spawn_input, &ctx).await.unwrap();
        let id: u64 = out
            .text_content()
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let out = tool
                .call(
                    json!({ "command": "", "operation": "job_status", "job_id": id }),
                    &ctx,
                )
                .await
                .unwrap();
            text = out.text_content();
            if text.contains("Completed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(text.contains("Completed"), "job must finish: {text}");
        assert_bounded(&text, 512);
    }

    #[test]
    fn command_summary_bounds_a_pathological_command() {
        let long = "x".repeat(10_000);
        let summary = command_summary(&long);
        assert!(
            summary.len() <= MAX_COMMAND_SUMMARY_BYTES + TRUNCATION_MARKER.len(),
            "an elided command stays within the display bound plus marker"
        );
        assert!(summary.contains(TRUNCATION_MARKER));
        assert_eq!(command_summary("echo hi"), "echo hi");
    }

    #[tokio::test]
    async fn a_long_command_job_keeps_its_exit_line_in_job_status() {
        let _guard = JOB_TEST_GUARD.lock().await;
        reset_job_table();
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        // Past the header reservation even after the display bound's
        // marker: the exit line must still survive the rendering.
        let command = format!("echo {} && echo marker-done", "x".repeat(600));
        let out = tool
            .call(json!({ "command": command, "background": true }), &ctx)
            .await
            .unwrap();
        let id: u64 = out
            .text_content()
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let out = tool
                .call(
                    json!({ "command": "", "operation": "job_status", "job_id": id }),
                    &ctx,
                )
                .await
                .unwrap();
            text = out.text_content();
            if text.contains("Completed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(text.contains("Completed"), "job must finish: {text}");
        assert!(
            text.contains("marker-done"),
            "the payload survives the rendering: {text}"
        );
        assert!(
            text.contains("[exit 0"),
            "a long command must not push the render into a tail cut: {text}"
        );
        assert_bounded(&text, 600);
    }

    #[tokio::test]
    async fn jobs_listing_bounds_a_long_command() {
        let _guard = JOB_TEST_GUARD.lock().await;
        reset_job_table();
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        let command = format!("echo {}", "y".repeat(600));
        tool.call(json!({ "command": command, "background": true }), &ctx)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while list_jobs()
            .iter()
            .any(|job| job.status == JobStatus::Running)
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let out = tool
            .call(json!({ "command": "", "operation": "jobs" }), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        assert!(
            text.contains(TRUNCATION_MARKER),
            "an over-bound command is elided in the listing: {text}"
        );
        assert!(
            !text
                .lines()
                .any(|line| line.len() > MAX_COMMAND_SUMMARY_BYTES + 200),
            "every listing line stays within the display bound's reach"
        );
    }

    #[tokio::test]
    async fn terminal_jobs_are_evicted_beyond_the_retention_cap() {
        let _guard = JOB_TEST_GUARD.lock().await;
        reset_job_table();
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        for _ in 0..(MAX_TERMINAL_JOBS + 5) {
            tool.call(json!({ "command": "true", "background": true }), &ctx)
                .await
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while list_jobs()
            .iter()
            .any(|job| job.status == JobStatus::Running)
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let retained = list_jobs().len();
        assert!(
            retained <= MAX_TERMINAL_JOBS,
            "the table must cap terminal retention, retained {retained}"
        );
        assert!(retained > 0, "recent terminal jobs stay queryable");
    }

    #[tokio::test]
    async fn jobs_lists_and_cleanup_removes() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tool = BashTool;

        // Spawn two quick background jobs.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let ctx = ctx_in(cwd);
        tool.call(json!({ "command": "echo a", "background": true }), &ctx)
            .await
            .unwrap();
        tool.call(json!({ "command": "echo b", "background": true }), &ctx)
            .await
            .unwrap();

        // Wait for completion.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // List.
        let list_out = tool
            .call(json!({ "command": "", "operation": "jobs" }), &ctx)
            .await
            .unwrap();
        assert!(!list_out.is_error);
        assert!(
            !list_out.text_content().contains("[exit"),
            "the listing must summarize, not inline payloads"
        );

        // Cleanup.
        let clean_out = tool
            .call(json!({ "command": "", "operation": "cleanup_jobs" }), &ctx)
            .await
            .unwrap();
        let text = clean_out.text_content();
        assert!(text.contains("Removed"), "cleanup text: {text}");
    }

    #[tokio::test]
    async fn background_job_timeout_marks_failed() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        // Spawn a background job that sleeps longer than its timeout.
        let spawn_input = json!({ "command": "sleep 30", "background": true, "timeout": 1 });
        let out = tool.call(spawn_input, &ctx).await.unwrap();
        let text = out.text_content();
        let id: u64 = text
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        // Wait long enough for the timeout to fire and update the job table.
        tokio::time::sleep(Duration::from_secs(3)).await;

        let status_input = json!({ "command": "", "operation": "job_status", "job_id": id });
        let out = tool.call(status_input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            text.contains("Failed"),
            "timed-out job should be Failed: {text}"
        );
        assert!(
            text.contains("timed out"),
            "failure message should mention timeout: {text}"
        );
    }

    #[tokio::test]
    async fn a_high_output_job_keeps_its_exit_line_in_job_status() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        // Both streams past the per-stream cap: the joined payload
        // reaches twice the render cap, so an uncapped store would push
        // the exit line out of any head-only cut.
        let spawn_input = json!({
            "command": "head -c 1200000 /dev/zero | tr '\\0' 'x'; \
                        head -c 1200000 /dev/zero | tr '\\0' 'e' >&2; exit 3",
            "background": true
        });
        let out = tool.call(spawn_input, &ctx).await.unwrap();
        let id: u64 = out
            .text_content()
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let deadline = Instant::now() + Duration::from_secs(10);
        let text = loop {
            let status_input = json!({ "command": "", "operation": "job_status", "job_id": id });
            let out = tool.call(status_input, &ctx).await.unwrap();
            let text = out.text_content();
            if !text.contains("Running") || Instant::now() > deadline {
                break text;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        assert!(
            text.contains("[exit 3,"),
            "the exit line must survive the payload cap: {}",
            tail_of(&text, 200)
        );
        assert!(
            text.contains(TRUNCATION_MARKER),
            "the elided middle must be named: {}",
            tail_of(&text, 200)
        );
        assert!(
            text.len() <= MAX_OUTPUT_BYTES + 16,
            "the rendered status stays within the cap: {}",
            text.len()
        );
    }

    #[tokio::test]
    async fn background_job_custom_timeout_completes() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);

        // A quick command with a short custom timeout should complete fine.
        let spawn_input = json!({ "command": "echo bgok", "background": true, "timeout": 5 });
        let out = tool.call(spawn_input, &ctx).await.unwrap();
        let text = out.text_content();
        let id: u64 = text
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        tokio::time::sleep(Duration::from_millis(500)).await;

        let status_input = json!({ "command": "", "operation": "job_status", "job_id": id });
        let out = tool.call(status_input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(
            text.contains("Completed") && text.contains("bgok"),
            "the job must reach the terminal status with its payload: {text}"
        );
    }

    #[tokio::test]
    async fn unknown_operation_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "", "operation": "frobnicate" });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn job_status_without_id_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = BashTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "command": "", "operation": "job_status" });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
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
