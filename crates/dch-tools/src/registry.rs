//! Construction of the builtin tool registry.

use loopctl::tool::ToolRegistry;

use crate::bash::BashTool;
use crate::code_search::CodeSearchInput;
use crate::edit::EditInput;
use crate::file_viewer::FileViewerInput;
use crate::glob::GlobInput;
use crate::grep::GrepInput;
use crate::multi_edit::MultiEditTool;
use crate::read::ReadInput;
use crate::tree::TreeInput;
use crate::write::WriteInput;

/// Build a [`ToolRegistry`] populated with every builtin tool.
///
/// Each builtin tool is registered here. Downstream callers (the runner)
/// invoke this once for the engine registry and again for the dispatch
/// pipeline's core, so the two positions agree by construction.
#[must_use]
pub fn builtin_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ReadInput::default());
    registry.register(BashTool);
    registry.register(WriteInput::default());
    registry.register(EditInput::default());
    registry.register(MultiEditTool);
    registry.register(FileViewerInput::default());
    registry.register(GlobInput::default());
    registry.register(GrepInput::default());
    registry.register(CodeSearchInput::default());
    registry.register(TreeInput::default());
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

        let first_schemas: Vec<_> = first
            .all_tools()
            .into_iter()
            .map(|tool| serde_json::to_string(&tool.schema()).expect("schema serializes"))
            .collect();
        let second_schemas: Vec<_> = second
            .all_tools()
            .into_iter()
            .map(|tool| serde_json::to_string(&tool.schema()).expect("schema serializes"))
            .collect();
        assert_eq!(
            first_schemas, second_schemas,
            "registries must agree in order"
        );
        assert!(!first_schemas.is_empty(), "registry must not be empty");
    }
}
