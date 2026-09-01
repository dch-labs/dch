//! Coding-assistant tool implementations for `dch`, built on `loopctl::tool`.
//!
//! The runner context ([`RunnerContext`]) is installed as a typed extension on
//! each `ToolContext`; tools retrieve it with [`runner_ctx`] to reach the
//! working directory, the per-run todo list, the interactive question
//! channel, and the file-baseline map that backs the Write tool's staleness
//! check.

#![warn(missing_docs)]

pub mod bash;
pub mod code_search;
pub(crate) mod conflict;
pub mod context;
pub mod diff;
pub mod edit;
pub mod file_viewer;
pub mod fs;
pub mod glob;
pub mod grep;
pub mod input;
pub mod linter;
pub mod multi_edit;
pub mod output;
pub mod question;
pub mod read;
pub mod regex_cache;
pub mod registry;
pub mod search;
pub mod state;
pub mod todo;
pub mod tree;
pub mod util;
pub mod walk;
pub mod write;

pub use bash::BashTool;
pub use code_search::CodeSearchInput;
pub use context::RunnerContext;
pub use context::runner_ctx;
pub use edit::EditInput;
pub use file_viewer::FileViewerInput;
pub use glob::GlobInput;
pub use grep::GrepInput;
pub use linter::LinterError;
pub use linter::LinterResult;
pub use multi_edit::MultiEditTool;
pub use question::Question;
pub use question::QuestionOption;
pub use question::QuestionRequest;
pub use question::QuestionResponse;
pub use read::ReadInput;
pub use registry::builtin_registry;
pub use todo::TodoEntry;
pub use todo::TodoStatus;
pub use todo::TodoTool;
pub use tree::TreeInput;
pub use util::ResolvePolicy;
pub use write::WriteInput;
