//! Composition layer wiring `loopctl`, `dch-tools`, and `dch-config` into the `dch` loop.

#![warn(missing_docs)]

pub mod console_observer;
pub mod context_probe;
pub mod error;
pub mod known_windows;
pub mod permission;
pub mod project;
pub mod prompt;
pub mod provider;
pub mod question_bridge;
pub mod redaction_guard;
pub mod runner;
pub(crate) mod todo_nudge;
pub(crate) mod write_target_gate;

pub use console_observer::ConsoleObserver;
pub use dch_config::Verbosity;
pub use dch_config::{ApiConfig, ApiType, DchConfigError, TechProfile};
pub use error::RunnerError;
pub use loopctl::middleware::AskResolverFn;
pub use permission::permission_layer;
pub use project::MessageAnalysis;
pub use project::analyze_message;
pub use project::detect_tech_stack;
pub use project::merge_by_language;
pub use project::render_techs;
pub use prompt::build_system_prompt;
pub use prompt::with_context;
pub use prompt::with_role;
pub use provider::DchClient;
pub use provider::create_client;
pub use question_bridge::QuestionPath;
pub use question_bridge::{PromptOption, PromptedQuestion, spawn_question_bridge};
pub use runner::Runner;
pub use runner::RunnerBuilder;
pub use todo_nudge::history_without_nudge;
pub use todo_nudge::is_nudge_wording;
