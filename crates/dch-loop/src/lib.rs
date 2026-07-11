//! Composition layer wiring `loopctl`, `dch-tools`, and `dch-config` into the `dch` loop.

#![warn(missing_docs)]

pub mod error;
pub mod provider;

pub use dch_config::{ApiConfig, ApiType, DchConfigError};
pub use error::RunnerError;
pub use provider::create_client;
