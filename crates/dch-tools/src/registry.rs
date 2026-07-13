//! Construction of the builtin tool registry.

use loopctl::tool::ToolRegistry;

use crate::bash::BashTool;
use crate::read::ReadTool;

/// Build a [`ToolRegistry`] populated with every builtin tool.
///
/// Each builtin tool is registered here. Downstream callers (the runner)
/// invoke this once at startup. Later tool tasks append their registrations.
#[must_use]
pub fn builtin_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ReadTool);
    registry.register(BashTool);
    registry
}
