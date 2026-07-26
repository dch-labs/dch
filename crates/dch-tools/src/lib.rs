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
pub mod glob;
pub mod grep;
pub mod linter;
pub mod multi_edit;
pub mod output;
pub mod question;
pub mod read;
pub mod regex_cache;
pub mod registry;
pub mod runtime;
pub mod state;
pub mod util;
pub mod walk;
pub mod write;

pub use bash::BashTool;
pub use context::RunnerContext;
pub use context::runner_ctx;
pub use edit::EditTool;
pub use file_viewer::FileViewerTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use linter::LinterError;
pub use linter::LinterResult;
pub use linter::lint_content;
pub use multi_edit::MultiEditTool;
pub use output::MAX_INLINE_OUTPUT_BYTES;
pub use output::truncate_or_write_to_temp;
pub use question::Question;
pub use question::QuestionOption;
pub use question::QuestionRequest;
pub use question::QuestionResponse;
pub use read::ReadTool;
pub use regex_cache::get_or_compile;
pub use regex_cache::get_or_compile_case_insensitive;
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
pub use walk::likely_binary;
pub use walk::matches_any_glob;
pub use walk::walk_files;
pub use walk::wildcard_match;
pub use write::WriteTool;
