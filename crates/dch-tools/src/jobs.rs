//! The Jobs tool — manages the background jobs the Bash tool spawns.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;

use crate::bash::MAX_OUTPUT_BYTES;
use crate::bash::TRUNCATION_MARKER;
use crate::bash::execute_command;
use crate::bash::truncate_string;

/// Display bound for a command rendered in job output, in bytes.
///
/// The `jobs` listing and the `job_status` header interpolate the
/// command verbatim, and nothing bounds the model's input — a huge
/// command would re-open the unbounded listing hole the payload work
/// closed, and would outgrow the header room [`cap_job_payload`]
/// reserves. Renderings show at most this many bytes of it.
pub(crate) const MAX_COMMAND_SUMMARY_BYTES: usize = 256;

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
    /// The payload is the job's final tool output: a failed-but-completed
    /// job's captured stdout and stderr plus the `[exit …, …ms]` metadata
    /// line, the fixed timeout message when the deadline fired — no partial
    /// pre-kill output is captured, the cancellation discards the streams —
    /// or the spawn error text when the command never started.
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
    /// The model uses this to poll status via the `job_status` operation.
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
/// Keyed by the monotonic job ID and shared by every background spawn
/// in the process: spawns insert here, detached completion tasks update
/// their entries in place under the mutex, and the Jobs tool's
/// operations read clones. A poisoned lock is tolerated — accessors
/// return empty rather than failing the tool call.
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
/// it polls later via the Jobs tool's `job_status` operation. A poisoned
/// job-table lock is tolerated: the spawn still returns the ID even if the
/// entry could not be recorded, matching the rest of the job-table accessors'
/// handling.
pub(crate) fn spawn_background_job(command: &str, cwd: &str, timeout_secs: u64) -> u64 {
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
/// was never spawned, evicted by the terminal-retention cap, or already
/// cleaned up via the `cleanup_jobs` operation).
fn get_job(id: u64) -> Option<BackgroundJob> {
    JOB_TABLE.lock().ok()?.get(&id).cloned()
}

/// List all tracked background jobs.
///
/// Returns a clone of every entry in the global job table, in `BTreeMap` key
/// order (ascending job ID). Used by the Jobs tool's `jobs` operation to show
/// the model what's running and what has finished. Returns an empty vec if
/// the table is empty or the lock is poisoned.
fn list_jobs() -> Vec<BackgroundJob> {
    JOB_TABLE
        .lock()
        .map(|t| t.values().cloned().collect())
        .unwrap_or_default()
}

/// Remove terminal jobs (completed or failed) from the table.
///
/// Retains only [`JobStatus::Running`] entries, evicting everything else.
/// Returns the number of jobs removed. Used by the Jobs tool's
/// `cleanup_jobs` operation so the model can reclaim table space after
/// polling all results. Returns 0 if the lock is poisoned or no terminal jobs
/// exist.
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
pub(crate) fn command_summary(command: &str) -> String {
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

/// Manage the background jobs the Bash tool spawns.
///
/// The Bash tool starts jobs with `background: true` and hands back an
/// ID; this tool is the other half of that contract — listing what is
/// running, polling one job's status and captured output by ID, and
/// clearing finished jobs out of the table. Splitting job management
/// into its own tool keeps every schema single-shape: `operation` is
/// this tool's one required field and `command` is the Bash tool's, so
/// no input needs a field it does not use.
pub struct JobsTool;

impl Tool for JobsTool {
    fn name(&self) -> &'static str {
        "Jobs"
    }

    fn description(&self) -> &'static str {
        "Manage background jobs started by the Bash tool: list them, poll one \
         job's status and captured output by id, or remove finished ones."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "description": "The job-management action to perform.",
                        "enum": ["jobs", "job_status", "cleanup_jobs"]
                    },
                    "job_id": {
                        "type": "integer",
                        "description": "Job ID to query (required for operation=job_status)"
                    }
                },
                "required": ["operation"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        Box::pin(async move { Self::call_inner(&input) })
    }

    fn system_prompt(&self) -> Option<String> {
        Some(
            "Background jobs return an ID immediately; poll their status and \
             captured output with the Jobs tool (operation=job_status, job_id) \
             instead of re-running the command. List running jobs with \
             operation=jobs, and clear finished ones with operation=cleanup_jobs."
                .to_string(),
        )
    }
}

impl JobsTool {
    /// Body of [`Tool::call`], synchronous by design: every operation
    /// reads or mutates the in-process job table, so there is nothing
    /// to await.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for a missing `operation`, an
    /// unknown `operation`, or a `job_status` called without a `job_id`.
    fn call_inner(input: &Value) -> Result<ToolOutput, ToolError> {
        let operation = input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing operation".to_string()))?;
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
                let id = input.get("job_id").and_then(Value::as_u64).ok_or_else(|| {
                    ToolError::InvalidInput("job_status requires job_id".to_string())
                })?;
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
}

/// Shared fixtures for the job-table tests of both tool modules.
///
/// The table and its ID counter are global, so every test that spawns a
/// job — Bash-side spawn tests included — locks the one guard before
/// touching it; table-counting tests also reset the table through here.
#[cfg(test)]
pub(crate) mod test_support {
    use super::JOB_TABLE;

    /// Serializes tests that touch the global job table, preventing
    /// cross-test interference from the shared `JOB_TABLE` /
    /// `JOB_ID_COUNTER`.
    pub(crate) static JOB_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Empty the global job table.
    ///
    /// A background job's completion task dies with its test's runtime, so
    /// an entry from an earlier test can linger as [`JobStatus::Running`]
    /// forever; tests that count table state start from a clean slate.
    pub(crate) fn reset_job_table() {
        if let Ok(mut table) = JOB_TABLE.lock() {
            table.clear();
        }
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
    use super::*;
    use crate::bash::BashTool;
    use crate::bash::test_support::{assert_bounded, ctx_in, tail_of};
    use crate::jobs::test_support::{JOB_TEST_GUARD, reset_job_table};
    use std::time::Instant;

    #[test]
    fn schema_pins_the_operation_only_shape() {
        let schema = JobsTool.schema();
        let props = schema
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(props.contains_key("operation"));
        assert!(props.contains_key("job_id"));
        assert!(
            !props.contains_key("command"),
            "job management never needs a command — the Bash/Jobs split exists \
             so no call carries a field it does not use"
        );

        let required = schema
            .input_schema
            .get("required")
            .unwrap()
            .as_array()
            .unwrap();
        // `operation` is the single required field — the exact grammar-safe
        // shape the split bought: plain required lists, no `anyOf`, so
        // schema-to-grammar servers (llama.cpp-style) can emit a valid
        // operation-only request without a filler `command`.
        assert_eq!(
            required.len(),
            1,
            "operation must be the single required field: {required:?}"
        );
        assert_eq!(required[0], "operation");
        assert!(
            schema.input_schema.get("anyOf").is_none(),
            "anyOf alternatives are not understood by local models; keep it out"
        );
    }

    #[test]
    fn constants_match_bash_render_caps() {
        assert_eq!(MAX_COMMAND_SUMMARY_BYTES, 256);
        assert_eq!(MAX_TERMINAL_JOBS, 20);
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

    #[test]
    fn jobstool_registered_in_builtin_registry() {
        let reg = crate::registry::builtin_registry();
        let tool = reg.get("Jobs").expect("JobsTool registered");
        assert!(!tool.is_read_only());
        assert!(
            !tool.is_safe_for_concurrent_execution(&json!({"operation": "jobs"})),
            "cleanup mutates the shared table, so jobs calls serialize"
        );
    }

    #[test]
    fn system_prompt_mentions_polling() {
        let prompt = JobsTool.system_prompt();
        assert!(prompt.is_some());
        assert!(prompt.unwrap().contains("job_status"));
    }

    #[tokio::test]
    async fn job_status_after_completion() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let ctx = ctx_in(cwd);

        // Spawn a background job through the Bash tool.
        let spawn_input = json!({ "command": "echo bgdone", "background": true });
        let out = BashTool.call(spawn_input, &ctx).await.unwrap();
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

        // Poll through the Jobs tool — no command field anywhere.
        let out = JobsTool
            .call(json!({ "operation": "job_status", "job_id": id }), &ctx)
            .await
            .unwrap();
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
        let ctx = ctx_in(cwd);

        let spawn_input = json!({ "command": "yes y | head -c 2000000", "background": true });
        let out = BashTool.call(spawn_input, &ctx).await.unwrap();
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
            let out = JobsTool
                .call(json!({ "operation": "job_status", "job_id": id }), &ctx)
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

    #[tokio::test]
    async fn a_long_command_job_keeps_its_exit_line_in_job_status() {
        let _guard = JOB_TEST_GUARD.lock().await;
        reset_job_table();
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let ctx = ctx_in(cwd);

        // Past the header reservation even after the display bound's
        // marker: the exit line must still survive the rendering.
        let command = format!("echo {} && echo marker-done", "x".repeat(600));
        let out = BashTool
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
            let out = JobsTool
                .call(json!({ "operation": "job_status", "job_id": id }), &ctx)
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
        let ctx = ctx_in(cwd);

        let command = format!("echo {}", "y".repeat(600));
        BashTool
            .call(json!({ "command": command, "background": true }), &ctx)
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

        let out = JobsTool
            .call(json!({ "operation": "jobs" }), &ctx)
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
        let ctx = ctx_in(cwd);

        for _ in 0..(MAX_TERMINAL_JOBS + 5) {
            BashTool
                .call(json!({ "command": "true", "background": true }), &ctx)
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
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let ctx = ctx_in(cwd);

        // Spawn two quick background jobs through Bash.
        BashTool
            .call(json!({ "command": "echo a", "background": true }), &ctx)
            .await
            .unwrap();
        BashTool
            .call(json!({ "command": "echo b", "background": true }), &ctx)
            .await
            .unwrap();

        // Wait for completion.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // List.
        let list_out = JobsTool
            .call(json!({ "operation": "jobs" }), &ctx)
            .await
            .unwrap();
        assert!(!list_out.is_error);
        assert!(
            !list_out.text_content().contains("[exit"),
            "the listing must summarize, not inline payloads"
        );

        // Cleanup.
        let clean_out = JobsTool
            .call(json!({ "operation": "cleanup_jobs" }), &ctx)
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
        let ctx = ctx_in(cwd);

        // Spawn a background job that sleeps longer than its timeout.
        let spawn_input = json!({ "command": "sleep 30", "background": true, "timeout": 1 });
        let out = BashTool.call(spawn_input, &ctx).await.unwrap();
        let text = out.text_content();
        let id: u64 = text
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        // Wait long enough for the timeout to fire and update the job table.
        tokio::time::sleep(Duration::from_secs(3)).await;

        let out = JobsTool
            .call(json!({ "operation": "job_status", "job_id": id }), &ctx)
            .await
            .unwrap();
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
        let ctx = ctx_in(cwd);

        // Both streams past the per-stream cap: the joined payload
        // reaches twice the render cap, so an uncapped store would push
        // the exit line out of any head-only cut.
        let spawn_input = json!({
            "command": "head -c 1200000 /dev/zero | tr '\\0' 'x'; \
                        head -c 1200000 /dev/zero | tr '\\0' 'e' >&2; exit 3",
            "background": true
        });
        let out = BashTool.call(spawn_input, &ctx).await.unwrap();
        let id: u64 = out
            .text_content()
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let deadline = Instant::now() + Duration::from_secs(10);
        let text = loop {
            let out = JobsTool
                .call(json!({ "operation": "job_status", "job_id": id }), &ctx)
                .await
                .unwrap();
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
        let ctx = ctx_in(cwd);

        // A quick command with a short custom timeout should complete fine.
        let spawn_input = json!({ "command": "echo bgok", "background": true, "timeout": 5 });
        let out = BashTool.call(spawn_input, &ctx).await.unwrap();
        let text = out.text_content();
        let id: u64 = text
            .split("job ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        tokio::time::sleep(Duration::from_millis(500)).await;

        let out = JobsTool
            .call(json!({ "operation": "job_status", "job_id": id }), &ctx)
            .await
            .unwrap();
        let text = out.text_content();
        assert!(
            text.contains("Completed") && text.contains("bgok"),
            "the job must reach the terminal status with its payload: {text}"
        );
    }

    #[tokio::test]
    async fn unknown_operation_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ctx_in(tmp.path().to_str().unwrap());
        let err = JobsTool
            .call(json!({ "operation": "frobnicate" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn job_status_without_id_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ctx_in(tmp.path().to_str().unwrap());
        let err = JobsTool
            .call(json!({ "operation": "job_status" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_operation_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ctx_in(tmp.path().to_str().unwrap());
        let err = JobsTool.call(json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn a_missing_job_reports_not_found_without_erroring() {
        let _guard = JOB_TEST_GUARD.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ctx_in(tmp.path().to_str().unwrap());
        let out = JobsTool
            .call(
                json!({ "operation": "job_status", "job_id": 999_999 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.text_content().contains("not found"),
            "an unknown id reads as a report, not a tool failure: {}",
            out.text_content()
        );
    }
}
