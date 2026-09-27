//! Coding-assistant tool implementations for `dch`, built on `loopctl::tool`.
//!
//! The runner context ([`RunnerContext`]) is installed as a typed extension on
//! each `ToolContext`; tools retrieve it with [`runner_ctx`] to reach the
//! working directory, the per-run todo list, the interactive question
//! channel, and the path-containment policy the search tools resolve under.
//! The filesystem family's own state — baselines, containment, and working
//! root — lives in loopctl's `FileSession`, attached beside it per dispatch.

#![warn(missing_docs)]

pub mod ask;
pub mod bash;
pub mod code_search;
pub mod context;
pub mod glob;
pub mod grep;
pub mod input;
pub mod jobs;
pub mod linter;
pub mod lsp;
pub mod lsp_tool;
pub mod output;
pub mod permission;
pub mod question;
pub mod regex_cache;
pub mod registry;
pub mod search;
pub mod submit;
pub mod todo;
pub mod tree;
pub mod util;
pub mod walk;
pub mod webfetch;

pub use ask::AskTool;
pub use bash::BashTool;
pub use code_search::CodeSearchInput;
pub use context::RunnerContext;
pub use context::runner_ctx;
pub use glob::GlobInput;
pub use grep::GrepInput;
pub use jobs::JobsTool;
pub use linter::LinterError;
pub use linter::LinterResult;
pub use lsp::LspServerConfig;
pub use lsp::get_server_for_file;
pub use lsp::supported_extensions;
pub use lsp_tool::LspTool;
pub use permission::PermissionMode;
pub use permission::PermissionOutcome;
pub use permission::ToolCategory;
pub use permission::decide;
pub use permission::tool_category;
pub use question::Question;
pub use question::QuestionOption;
pub use question::QuestionRequest;
pub use question::QuestionResponse;
pub use registry::builtin_registry;
pub use submit::SubmitTool;
pub use todo::TodoEntry;
pub use todo::TodoStatus;
pub use todo::TodoTool;
pub use tree::TreeInput;
pub use util::ResolvePolicy;
pub use webfetch::WebFetchTool;
