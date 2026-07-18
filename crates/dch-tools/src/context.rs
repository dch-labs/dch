//! The runner context extension stored on each `loopctl::tool::ToolContext`.
//!
//! Tools retrieve it with [`runner_ctx`] to reach shared session state, runtime
//! settings, and the interactive question channel.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::question::QuestionRequest;
use crate::runtime::RuntimeConfig;
use crate::state::SessionState;

/// Per loop instance context attached to every `ToolContext`.
///
/// Carries everything a tool invocation needs that is specific to this loop
/// instance run: the working directory, shared mutable session state, the optional
/// channel for asking the user questions, and runtime display/permission
/// settings. Stored as a typed extension via
/// [`ToolContext::set_extension`](loopctl::tool::ToolContext::set_extension)
/// and retrieved with [`runner_ctx`].
#[derive(Clone)]
pub struct RunnerContext {
    /// Working directory the dch operates within. Every tool resolves relative
    /// paths against this; it is the absolute root for file operations.
    pub cwd: PathBuf,
    /// Shared, mutable session state (todos, memory, stats, ...). The
    /// `Arc<Mutex<>>` lets tool calls in one loop run read and mutate it
    /// concurrently; cloning [`RunnerContext`] shares the same store.
    pub session_state: Arc<Mutex<SessionState>>,
    /// Optional channel for asking the user interactive questions. `None` in
    /// headless mode, where prompting is impossible and asking tools error.
    pub question_tx: Option<mpsc::Sender<QuestionRequest>>,
    /// Runtime-derived display and permission settings (verbosity, `no_color`,
    /// permission mode), read by tools that format output or gate writes.
    pub runtime: RuntimeConfig,
}

impl fmt::Debug for RunnerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerContext")
            .field("cwd", &self.cwd)
            .field("session_state", &self.session_state)
            .field("question_tx", &self.question_tx.is_some())
            .field("runtime", &self.runtime)
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
///     session_state: Default::default(),
///     question_tx: None,
///     runtime: Default::default(),
/// };
/// ctx.set_extension(rc);
/// assert!(runner_ctx(&ctx).is_some());
/// ```
#[must_use]
pub fn runner_ctx(ctx: &loopctl::tool::ToolContext) -> Option<&RunnerContext> {
    ctx.get_extension::<RunnerContext>()
}

// Statically asserts `RunnerContext: Send + Sync`, the bound required to store
// it as a `ToolContext` extension. `Arc<Mutex<SessionState>>` is `Send + Sync`,
// `Option<mpsc::Sender<_>>` is `Send + Sync`, and the remaining fields are
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
    use crate::state::TodoEntry;
    use crate::state::TodoStatus;
    use loopctl::tool::ToolContext;

    fn sample() -> RunnerContext {
        RunnerContext {
            cwd: PathBuf::from("/tmp/workspace"),
            session_state: Arc::new(Mutex::new(SessionState::default())),
            question_tx: None,
            runtime: RuntimeConfig::default(),
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
    fn shared_mutation_visible_across_clones() {
        let rc = sample();
        let twin = rc.clone();

        {
            let mut state = rc
                .session_state
                .lock()
                .expect("session state lock not poisoned");
            state.todos.push(TodoEntry {
                id: "1".to_string(),
                subject: "Ship it".to_string(),
                description: String::new(),
                status: TodoStatus::Pending,
                active_form: None,
            });
        }

        let observed = twin
            .session_state
            .lock()
            .expect("session state lock not poisoned")
            .todos
            .len();
        assert_eq!(observed, 1);
    }

    #[test]
    fn question_tx_survives_clone() {
        let (tx, rx) = mpsc::channel::<QuestionRequest>();
        let rc = RunnerContext {
            question_tx: Some(tx),
            ..sample()
        };
        let twin = rc.clone();

        // Build the only QuestionRequest we can without driving a UI: the
        // response channel is created and immediately dropped, and the
        // request is sent on the question channel.
        let (resp_tx, _) = tokio::sync::oneshot::channel();
        let req = QuestionRequest {
            questions: vec![crate::question::Question {
                question: "ok?".to_string(),
                header: None,
                options: vec![],
                multi_select: false,
                response_tx: resp_tx,
            }],
        };
        rc.question_tx
            .as_ref()
            .expect("question_tx set on rc")
            .send(req)
            .expect("receiver alive");
        drop(twin);
        assert!(rx.recv().is_ok());
    }

    #[test]
    fn debug_elides_question_tx() {
        let rc = sample();
        let rendered = format!("{rc:?}");
        // The Sender is not Debug; the rendered summary reports presence as a
        // bool rather than the channel itself.
        assert!(rendered.contains("question_tx: false"));
    }
}
