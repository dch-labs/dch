//! Construction of the builtin tool registry.

use loopctl::tool::ToolRegistry;

use crate::bash::BashTool;
use crate::code_search::CodeSearchTool;
use crate::edit::EditTool;
use crate::file_viewer::FileViewerTool;
use crate::glob::GlobTool;
use crate::grep::GrepTool;
use crate::multi_edit::MultiEditTool;
use crate::read::ReadTool;
use crate::tree::TreeTool;
use crate::write::WriteTool;

/// Build a [`ToolRegistry`] populated with every builtin tool.
///
/// Each builtin tool is registered here. Downstream callers (the runner)
/// invoke this once at startup. Later tool tasks append their registrations.
#[must_use]
pub fn builtin_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ReadTool);
    registry.register(BashTool);
    registry.register(WriteTool);
    registry.register(EditTool);
    registry.register(MultiEditTool);
    registry.register(FileViewerTool);
    registry.register(GlobTool);
    registry.register(GrepTool);
    registry.register(CodeSearchTool);
    registry.register(TreeTool);
    registry
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_composition_is_deterministic() {
        // The runner hands one registry to the engine and builds the dispatch
        // pipeline's core from a second construction of this function; both
        // positions must expose the identical tool set and schemas, so two
        // constructions must agree exactly.
        let first = builtin_registry();
        let second = builtin_registry();

        let first_names = first.tool_names();
        let second_names = second.tool_names();
        assert_eq!(first_names, second_names, "tool sets must match");
        assert!(!first_names.is_empty(), "registry must not be empty");

        for name in &first_names {
            let (Some(a), Some(b)) = (first.get(name), second.get(name)) else {
                panic!("{name} must resolve in both registries");
            };
            let schema_a = serde_json::to_string(&a.schema()).expect("schema serializes");
            let schema_b = serde_json::to_string(&b.schema()).expect("schema serializes");
            assert_eq!(schema_a, schema_b, "{name} schema must match");
        }
    }
}
