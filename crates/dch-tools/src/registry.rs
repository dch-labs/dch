//! Construction of the builtin tool registry.

use std::sync::Arc;

use loopctl::tool::ToolRegistry;
use loopctl::tool::builtin::fs::ContentValidator;
use loopctl::tool::builtin::fs::EditTool;
use loopctl::tool::builtin::fs::FileSession;
use loopctl::tool::builtin::fs::FileSource;
use loopctl::tool::builtin::fs::FileViewerTool;
use loopctl::tool::builtin::fs::MultiEditTool;
use loopctl::tool::builtin::fs::WriteTool;
use loopctl::tool::builtin::read::ReadTool;

use crate::ask::AskTool;
use crate::bash::BashTool;
use crate::code_search::CodeSearchInput;
use crate::glob::GlobInput;
use crate::grep::GrepInput;
use crate::jobs::JobsTool;
use crate::linter::LintGate;
use crate::lsp_tool::LspTool;
use crate::submit::SubmitTool;
use crate::todo::TodoTool;
use crate::tree::TreeInput;
use crate::webfetch::WebFetchTool;

/// Build a [`ToolRegistry`] populated with every builtin tool.
///
/// The filesystem family arrives from loopctl, wired through
/// `session`: its working directory, containment policy, and baseline
/// map govern `read`, `Write`, `Edit`, `MultiEdit`, and `FileViewer`
/// alike, and dch's `LintGate` holds the syntax bar on every writing
/// tool — a bar the model can lift per call with the family's
/// `skip_linter` flag. Downstream callers (the runner) invoke this once
/// for the engine registry and again for the dispatch pipeline's core,
/// so the two positions agree by construction.
#[must_use]
pub fn builtin_registry(session: &FileSession) -> ToolRegistry {
    let lint: Arc<dyn ContentValidator> = Arc::new(LintGate);
    let mut registry = ToolRegistry::new();
    registry.register(ReadTool::new(FileSource::new(session.clone())));
    registry.register(BashTool);
    registry.register(JobsTool);
    registry.register(WriteTool::new().with_validator(Arc::clone(&lint)));
    registry.register(EditTool::new().with_validator(Arc::clone(&lint)));
    registry.register(MultiEditTool::new().with_validator(Arc::clone(&lint)));
    registry.register(FileViewerTool);
    registry.register(GlobInput::default());
    registry.register(GrepInput::default());
    registry.register(CodeSearchInput::default());
    registry.register(TreeInput::default());
    registry.register(TodoTool);
    registry.register(WebFetchTool);
    registry.register(AskTool);
    registry.register(LspTool);
    registry.register(SubmitTool);
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
        let session = FileSession::new(std::path::PathBuf::from("."));
        let first = builtin_registry(&session);
        let second = builtin_registry(&session);

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
