//! Composition layer wiring `loopctl`, `dch-tools`, and `dch-config` into the `dch` loop.

#![warn(missing_docs)]

pub mod error;
pub mod project;
pub mod prompt;
pub mod provider;

pub use dch_config::{ApiConfig, ApiType, DchConfigError, TechProfile};
pub use error::RunnerError;
pub use project::MessageAnalysis;
pub use project::analyze_message;
pub use project::detect_tech_stack;
pub use project::merge_by_language;
pub use project::render_techs;
pub use prompt::build_system_prompt;
pub use prompt::with_context;
pub use prompt::with_role;
pub use provider::create_client;
