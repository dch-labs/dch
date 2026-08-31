//! The runner context extension stored on each `loopctl::tool::ToolContext`.
//!
//! Tools retrieve it with [`runner_ctx`] to reach per-call, tool-facing state:
//! the working directory, the agent's per-run todo list, the channel slot
//! for asking the user interactive questions, and the session's file-read
//! history that backs the Write tool's staleness check.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::SystemTime;

use loopctl::tool::ToolError;

use crate::question::QuestionRequest;
use crate::state::FileBaselines;
use crate::todo::TodoEntry;

/// Per-call, tool-facing context attached to every `ToolContext`.
///
/// Carries what a tool invocation needs that is specific to this run and isn't
/// already on loopctl's `ToolContext`: the working directory (as a `PathBuf`,
/// the form the file tools prefer), the agent's per-run todo list, the
/// optional channel a tool uses during its call to ask the user a question,
/// and the model's file-baseline map backing the Write tool's staleness
/// check. Stored as a typed extension via
/// [`ToolContext::set_extension`](loopctl::tool::ToolContext::set_extension)
/// and retrieved with [`runner_ctx`].
///
/// Cloning is cheap — `todos`, `question_tx`, and `file_baselines` are
/// behind `Arc`s, so clones share the same mutable list, the same channel
/// slot, and the same map rather than copying them. This is how multiple
/// tool invocations within one run observe each other's todo-list mutations
/// (and how a Write sees what a prior Read recorded).
#[derive(Clone)]
pub struct RunnerContext {
    /// The working directory the agent operates within.
    ///
    /// Every tool that touches the filesystem resolves relative paths against
    /// this directory. It is the absolute root for file operations (`Read`,
    /// `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`, etc.). Set once at runner
    /// construction from the configured or CLI-supplied working directory.
    pub cwd: PathBuf,

    /// The agent's current todo list.
    ///
    /// Replaced wholesale by the `TodoWrite` tool on each call — the model
    /// sends the complete desired list, not a delta. The `Arc<Mutex<>>`
    /// wrapper lets concurrent tool calls read and mutate it safely; cloning
    /// [`RunnerContext`] shares the same list (no copy). Per-run: the runner
    /// clears it at the top of each `run()` so a new prompt starts fresh.
    pub todos: Arc<Mutex<Vec<TodoEntry>>>,

    /// Channel for asking the user interactive questions, behind a shared slot.
    ///
    /// Used by the asking tool to send a [`QuestionRequest`] to the UI (a TUI
    /// overlay or a headless reader). The slot starts empty; the asking tool
    /// returns an error instead of blocking when no channel is installed. A
    /// host that can prompt installs its sender before the first run (or
    /// between runs) — every tool dispatch reads the slot live, so a later
    /// installation affects subsequent dispatches immediately. Cloning
    /// [`RunnerContext`] shares the same slot.
    pub question_tx: Arc<Mutex<Option<mpsc::Sender<QuestionRequest>>>>,

    /// The model's latest known `mtime` per touched file.
    ///
    /// Read records what it observes; a successful write records the
    /// post-write `mtime` (see [`FileBaselines`]). The Write tool's
    /// detect-on-write conflict check compares the target's current `mtime`
    /// against this record before overwriting. Cloning [`RunnerContext`]
    /// shares the same map.
    pub file_baselines: Arc<Mutex<FileBaselines>>,
}

impl RunnerContext {
    /// Create a context for `cwd` with an empty todo list, no question
    /// channel, and no recorded file baselines.
    ///
    /// A relative `cwd` is anchored to the process's current directory.
    /// [`resolve_path`](crate::util::resolve_path) decides containment by
    /// comparing lexical prefixes, and a bare `.` normalizes to nothing —
    /// left un-anchored, it would reject every relative target. `.` and
    /// trailing separators are stripped; symlinks are not resolved, matching
    /// the lexical philosophy applied to targets. On the rare failure of the
    /// current-directory probe, `cwd` is stored as given.
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
        Self {
            cwd,
            todos: Arc::new(Mutex::new(Vec::new())),
            question_tx: Arc::new(Mutex::new(None)),
            file_baselines: Arc::new(Mutex::new(FileBaselines::default())),
        }
    }

    /// Record `mtime` as the model's latest known state of `path`.
    ///
    /// Thin locking wrapper over [`record`](crate::state::record), which owns
    /// the semantics (latest touch wins; an unmeasurable touch clears the
    /// baseline).
    pub(crate) fn record_baseline(&self, path: &Path, mtime: Option<SystemTime>) {
        let mut baselines = self
            .file_baselines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::state::record(&mut baselines, path, mtime);
    }

    /// The recorded baseline for `path`, when the latest touch measured an
    /// `mtime`.
    ///
    /// Thin locking wrapper over [`baseline`](crate::state::baseline).
    pub(crate) fn baseline_for(&self, path: &Path) -> Option<SystemTime> {
        let baselines = self
            .file_baselines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::state::baseline(&baselines, path)
    }
}

impl fmt::Debug for RunnerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let has_channel = self
            .question_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        let baselines = self
            .file_baselines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        f.debug_struct("RunnerContext")
            .field("cwd", &self.cwd)
            .field("todos", &self.todos)
            .field("question_tx", &has_channel)
            .field("file_baselines", &baselines)
            .finish()
    }
}

/// Downcast the `ToolContext` extension to a [`RunnerContext`] reference.
///
/// Returns `None` when no `RunnerContext` extension is installed on `ctx`;
/// callers should handle that case rather than unwrapping.
///
/// # Examples
///
/// ```
/// use dch_tools::RunnerContext;
/// use dch_tools::runner_ctx;
///
/// let mut ctx = loopctl::tool::ToolContext::default();
/// assert!(runner_ctx(&ctx).is_none());
///
/// ctx.set_extension(RunnerContext::new(".".into()));
/// assert!(runner_ctx(&ctx).is_some());
/// ```
#[must_use]
pub fn runner_ctx(ctx: &loopctl::tool::ToolContext) -> Option<&RunnerContext> {
    ctx.get_extension::<RunnerContext>()
}

/// Extract the working directory from an optional [`RunnerContext`].
///
/// Every tool's dispatch starts with this resolution: the extension is
/// installed by the runner before each tool call, so its absence means the
/// tool ran outside a dch-composed pipeline.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] naming the missing extension when
/// `runner_context` is `None`.
pub fn require_cwd(runner_context: Option<RunnerContext>) -> Result<PathBuf, ToolError> {
    runner_context.map(|rc| rc.cwd).ok_or_else(|| {
        ToolError::Execution(
            "RunnerContext extension is not installed on the ToolContext".to_string(),
        )
    })
}

/// Statically asserts `RunnerContext: Send + Sync`, the bound required to store it as a `ToolContext` extension.
///
/// `Arc<Mutex<Vec<TodoEntry>>>` is `Send + Sync`,
/// `Arc<Mutex<Option<mpsc::Sender<_>>>>` is `Send + Sync`, and `PathBuf` is
/// trivially so.
const _: fn() = || {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<RunnerContext>();
};

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::expect_used,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::question::Question;
    use crate::question::QuestionOption;
    use crate::question::QuestionRequest;
    use crate::todo::TodoEntry;
    use crate::todo::TodoStatus;
    use loopctl::tool::ToolContext;

    fn sample() -> RunnerContext {
        RunnerContext::new(PathBuf::from("/tmp/workspace"))
    }

    #[test]
    fn new_makes_a_relative_cwd_absolute() {
        let rc = RunnerContext::new(PathBuf::from("."));
        assert!(
            rc.cwd.is_absolute(),
            "a bare `.` must anchor to the process cwd: {:?}",
            rc.cwd
        );
        assert_eq!(rc.cwd, std::env::current_dir().unwrap());
    }

    #[test]
    fn a_relative_cwd_resolves_relative_targets() {
        let rc = RunnerContext::new(PathBuf::from("."));
        let resolved = crate::util::resolve_path("src/main.rs", &rc.cwd).unwrap();
        assert!(
            resolved.is_absolute(),
            "the target must resolve against the anchored cwd: {resolved:?}"
        );
        assert!(resolved.ends_with(Path::new("src/main.rs")));
    }

    #[test]
    fn extension_roundtrip() {
        let mut ctx = ToolContext::default();
        ctx.set_extension(sample());
        let got = ctx.get_extension::<RunnerContext>();
        assert!(got.is_some());
        assert_eq!(
            got.map(|r| r.cwd.clone()).unwrap_or_default(),
            PathBuf::from("/tmp/workspace")
        );
    }

    #[test]
    fn runner_ctx_present() {
        let mut ctx = ToolContext::default();
        ctx.set_extension(sample());
        let rc = runner_ctx(&ctx).expect("extension was set");
        assert_eq!(rc.cwd, PathBuf::from("/tmp/workspace"));
    }

    #[test]
    fn runner_ctx_absent() {
        let ctx = ToolContext::default();
        assert!(runner_ctx(&ctx).is_none());
    }

    #[test]
    fn shared_todos_visible_across_clones() {
        let rc = sample();
        let twin = rc.clone();
        rc.todos
            .lock()
            .expect("todos lock not poisoned")
            .push(TodoEntry {
                id: "1".to_string(),
                subject: "Ship it".to_string(),
                description: String::new(),
                status: TodoStatus::Pending,
                active_form: None,
            });
        let observed = twin.todos.lock().expect("todos lock not poisoned").len();
        assert_eq!(observed, 1);
    }

    #[test]
    fn todos_default_empty_and_can_be_cleared_to_reset_run() {
        // The per-run reset is a clear of the vec. Verify the default is empty
        // and that clearing works (the runner does this at the top of each run).
        let rc = sample();
        assert!(rc.todos.lock().expect("todos lock not poisoned").is_empty());
        rc.todos
            .lock()
            .expect("todos lock not poisoned")
            .push(TodoEntry {
                id: "x".to_string(),
                subject: "task".to_string(),
                description: String::new(),
                status: TodoStatus::Pending,
                active_form: None,
            });
        rc.todos.lock().expect("todos lock not poisoned").clear();
        assert!(rc.todos.lock().expect("todos lock not poisoned").is_empty());
    }

    #[test]
    fn question_tx_round_trips_a_request_and_empty_is_default() {
        // Default is an empty slot (headless); once a sender is installed, a
        // sent QuestionRequest reaches the receiver. This is the plumbing the
        // asking tool uses.
        assert!(sample().question_tx.lock().expect("slot").is_none());

        let (tx, rx) = mpsc::channel::<QuestionRequest>();
        let rc = RunnerContext {
            question_tx: Arc::new(Mutex::new(Some(tx))),
            ..sample()
        };
        let twin = rc.clone();
        // The cloned context shares the slot — senders cloned from it reach
        // the same receiver.
        let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
        let first = rc
            .question_tx
            .lock()
            .expect("slot")
            .clone()
            .expect("sender set");
        first
            .send(QuestionRequest {
                questions: vec![Question {
                    question: "ok?".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "Yes".to_string(),
                        description: None,
                    }],
                    multi_select: false,
                    response_tx: resp_tx,
                }],
            })
            .expect("receiver alive");
        let second = twin
            .question_tx
            .lock()
            .expect("slot on clone")
            .clone()
            .expect("sender set on clone");
        second
            .send(QuestionRequest {
                questions: Vec::new(),
            })
            .expect("receiver still alive");
        // try_recv (non-blocking) twice: confirms both sends arrived without
        // hanging on iter(), which would block forever waiting for senders to
        // drop.
        assert!(rx.try_recv().is_ok(), "first send missing");
        assert!(rx.try_recv().is_ok(), "second send missing");
        assert!(rx.try_recv().is_err(), "more than two sends arrived");
    }

    #[test]
    fn debug_reports_channel_presence_not_the_channel() {
        // The slot holds an mpsc::Sender, which is not Debug; the Debug impl
        // must report presence as a bool rather than the channel itself.
        let with_tx = {
            let (tx, _rx) = mpsc::channel::<QuestionRequest>();
            RunnerContext {
                question_tx: Arc::new(Mutex::new(Some(tx))),
                ..sample()
            }
        };
        let without_tx = sample();
        assert!(
            format!("{with_tx:?}").contains("question_tx: true"),
            "Debug should report question_tx present"
        );
        assert!(
            format!("{without_tx:?}").contains("question_tx: false"),
            "Debug should report question_tx absent"
        );
    }
}
