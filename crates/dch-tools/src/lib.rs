//! Coding-assistant tool implementations for `dch`, built on `loopctl::tool`.
//!
//! The runner context ([`RunnerContext`]) is installed as a typed extension on
//! each `ToolContext`; tools retrieve it with [`runner_ctx`] to reach shared
//! session state, runtime settings, and the interactive question channel.

#![warn(missing_docs)]

pub mod context;
pub mod question;
pub mod runtime;
pub mod state;

pub use context::RunnerContext;
pub use context::runner_ctx;
pub use question::Question;
pub use question::QuestionOption;
pub use question::QuestionRequest;
pub use question::QuestionResponse;
pub use runtime::PermissionMode;
pub use runtime::RuntimeConfig;
pub use runtime::Verbosity;
pub use state::FileReadEntry;
pub use state::MemoryEntry;
pub use state::SessionState;
pub use state::TodoEntry;
pub use state::TodoStatus;
pub use state::ToolStats;
