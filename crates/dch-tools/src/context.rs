//! The runner context extension stored on each `loopctl::tool::ToolContext`.
//!
//! Tools retrieve it with [`runner_ctx`] to reach per-call, tool-facing state:
//! the working directory, the agent's per-run todo list, and the optional
//! channel for asking the user interactive questions. Session-lifetime records
//! (file-touch history for staleness detection, etc.) are owned by the outer
//! runner layer and recorded via an observer on tool dispatch — not by the
//! tools and not via this context.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::question::QuestionRequest;
use crate::todo::TodoEntry;

/// Per-call, tool-facing context attached to every `ToolContext`.
///
/// Carries what a tool invocation needs that is specific to this run and isn't
/// already on loopctl's `ToolContext`: the working directory (as a `PathBuf`,
/// the form the file tools prefer), the agent's per-run todo list, and the
/// optional channel a tool uses during its call to ask the user a question.
/// Stored as a typed extension via
/// [`ToolContext::set_extension`](loopctl::tool::ToolContext::set_extension)
/// and retrieved with [`runner_ctx`].
///
/// Cloning is cheap — `todos` is behind an `Arc`, so clones share the same
/// mutable list rather than copying it. This is how multiple tool invocations
/// within one run observe each other's todo-list mutations.
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

    /// Optional channel for asking the user interactive questions.
    ///
    /// Used by the `AskUserQuestion` tool to send a [`QuestionRequest`] to the
    /// UI (TUI overlay or headless reader). `None` when prompting is impossible
    /// or unimplemented — the headless runner constructs `RunnerContext` with
    /// `None` here, and an interactive entrypoint (TUI) replaces it before the
    /// first run; the asking tool returns an error instead of blocking when the
    /// channel is absent.
    pub question_tx: Option<mpsc::Sender<QuestionRequest>>,
}

impl fmt::Debug for RunnerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerContext")
            .field("cwd", &self.cwd)
            .field("todos", &self.todos)
            .field("question_tx", &self.question_tx.is_some())
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
/// let rc = RunnerContext {
///     cwd: ".".into(),
///     todos: Default::default(),
///     question_tx: None,
/// };
/// ctx.set_extension(rc);
/// assert!(runner_ctx(&ctx).is_some());
/// ```
#[must_use]
pub fn runner_ctx(ctx: &loopctl::tool::ToolContext) -> Option<&RunnerContext> {
    ctx.get_extension::<RunnerContext>()
}

// Statically asserts `RunnerContext: Send + Sync`, the bound required to store
// it as a `ToolContext` extension. `Arc<Mutex<Vec<TodoEntry>>>` is
// `Send + Sync`, `Option<mpsc::Sender<_>>` is `Send + Sync`, and `PathBuf` is
// trivially so.
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
        RunnerContext {
            cwd: PathBuf::from("/tmp/workspace"),
            todos: Arc::new(Mutex::new(Vec::new())),
            question_tx: None,
        }
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
    fn question_tx_round_trips_a_request_and_none_is_default() {
        // Default is None (headless); when Some, a sent QuestionRequest reaches
        // the receiver. This is the plumbing AskUserQuestion (T-38) will use.
        assert!(sample().question_tx.is_none());

        let (tx, rx) = mpsc::channel::<QuestionRequest>();
        let rc = RunnerContext {
            question_tx: Some(tx),
            ..sample()
        };
        let twin = rc.clone();
        // The cloned context shares the sender (mpsc::Sender is Clone) — both
        // can send, both reach the same receiver.
        let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
        rc.question_tx
            .as_ref()
            .expect("tx set")
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
        twin.question_tx
            .as_ref()
            .expect("tx set on clone")
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
    fn debug_reports_question_tx_presence_not_the_channel() {
        // mpsc::Sender is not Debug; the Debug impl must report presence as a
        // bool rather than the channel itself.
        let with_tx = {
            let (tx, _rx) = mpsc::channel::<QuestionRequest>();
            RunnerContext {
                question_tx: Some(tx),
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
