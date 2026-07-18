//! Coding-assistant tool implementations for `dch`, built on `loopctl::tool`.
//!
//! The runner context ([`RunnerContext`]) is installed as a typed extension on
//! each `ToolContext`; tools retrieve it with [`runner_ctx`] to reach shared
//! session state, runtime settings, and the interactive question channel.

#![warn(missing_docs)]

pub mod bash;
pub mod context;
pub mod diff;
pub mod edit;
pub mod file_viewer;
pub mod fs;
pub mod linter;
pub mod multi_edit;
pub mod question;
pub mod read;
pub mod registry;
pub mod runtime;
pub mod state;
pub mod util;
pub mod write;

pub use bash::BashTool;
pub use context::RunnerContext;
pub use context::runner_ctx;
pub use edit::EditTool;
pub use file_viewer::FileViewerTool;
pub use linter::LinterError;
pub use linter::LinterResult;
pub use linter::lint_content;
pub use multi_edit::MultiEditTool;
pub use question::Question;
pub use question::QuestionOption;
pub use question::QuestionRequest;
pub use question::QuestionResponse;
pub use read::ReadTool;
pub use registry::builtin_registry;
pub use runtime::PermissionMode;
pub use runtime::RuntimeConfig;
pub use runtime::Verbosity;
pub use state::FileReadEntry;
pub use state::MemoryEntry;
pub use state::SessionState;
pub use state::TodoEntry;
pub use state::TodoStatus;
pub use state::ToolStats;
pub use write::WriteTool;
