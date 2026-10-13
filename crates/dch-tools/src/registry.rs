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
use crate::read_files::ReadFilesTool;
use crate::submit::SubmitTool;
use crate::todo::TodoTool;
use crate::tree::TreeInput;
use crate::webfetch::WebFetchTool;

/// Window ceilings the read family applies to one file view.
///
/// Both the single-file `Read` tool and every file inside a `ReadFiles`
/// batch share these ceilings, so a batched read and the per-file read
/// it replaces cost the same context per file. The defaults fit a long
/// documentation file in one call, which keeps an orientation over a
/// mandated doc list from spending a model round-trip per paging
/// continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadWindows {
    /// Maximum lines one file view may return.
    ///
    /// Beyond this the view is cut with a marker naming the continuation
    /// offset. The underlying builder floors values below 1 to 1.
    pub max_lines: usize,

    /// Maximum bytes of joined, line-numbered output one file view may
    /// return.
    ///
    /// The real bound — it cuts long-line content a line ceiling cannot;
    /// the underlying builder floors values below 1 to 1.
    pub max_bytes: usize,

    /// Byte ceiling on one `ReadFiles` call's joined output.
    ///
    /// One budget serves the whole batch rather than one per file, so a
    /// multi-file read cannot hand back more than a single tool result
    /// is meant to carry. `0` means unbudgeted: every file in the batch
    /// is served.
    pub max_batch_bytes: usize,
}

impl Default for ReadWindows {
    fn default() -> Self {
        Self {
            max_lines: 1200,
            max_bytes: DEFAULT_READ_MAX_BYTES,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
        }
    }
}

/// The shipped default per-file read ceiling, kept under the batch budget.
///
/// Forty-eight kibibytes sits deliberately below the sixty-four of
/// `DEFAULT_MAX_BATCH_BYTES`: one file must never exhaust the budget its
/// whole batch shares, so the order of the pair is load-bearing. Raise the
/// per-file bound past the batch budget and the guarantee inverts
/// silently — every file stays individually coherent while every batch
/// degrades to its longest file plus budget markers.
const DEFAULT_READ_MAX_BYTES: usize = 49_152;

/// The shipped default batch budget, matching the conversation's ceiling on
/// one tool result.
///
/// One budget serves one result, so a batch can never outspend the single
/// reads it replaces. This is the larger half of the pair with
/// `DEFAULT_READ_MAX_BYTES` above, and the pair's order is what keeps
/// every file of a batch reachable: the largest served file still costs
/// less than the budget, so it cannot spend what the files behind it need.
const DEFAULT_MAX_BATCH_BYTES: usize = 65_536;

impl ReadWindows {
    /// Build a `ReadTool` over the session's file source under these
    /// windows.
    ///
    /// The one construction path every read-family registration goes
    /// through, so the single-file tool, the batched tool, and any future
    /// reader cannot drift apart on window size. The offset-only default
    /// page matches the line ceiling, keeping successive windows the same
    /// size as a fresh read.
    #[must_use]
    fn session_reader(self, session: &FileSession) -> ReadTool<FileSource> {
        ReadTool::new(FileSource::new(session.clone()))
            .with_max_lines(self.max_lines)
            .with_max_bytes(self.max_bytes)
            .with_default_offset_limit(self.max_lines)
    }
}

/// Build a [`ToolRegistry`] populated with every builtin tool.
///
/// The filesystem family arrives from loopctl, wired through
/// `session`: its working directory, containment policy, and baseline
/// map govern `read`, `Write`, `Edit`, `MultiEdit`, and `FileViewer`
/// alike, and dch's `LintGate` holds the syntax bar on every writing
/// tool — a bar the model can lift per call with the family's
/// `skip_linter` flag. Downstream callers (the runner) invoke this once
/// for the engine registry and again for the dispatch pipeline's core,
/// so the two positions agree by construction. The read family serves
/// its file views under the passed [`ReadWindows`] ceilings.
#[must_use]
pub fn builtin_registry(session: &FileSession, windows: ReadWindows) -> ToolRegistry {
    let lint: Arc<dyn ContentValidator> = Arc::new(LintGate);
    let mut registry = ToolRegistry::new();
    let reader = windows.session_reader(session);
    registry.register(reader.clone());
    registry.register(ReadFilesTool::new(reader, windows.max_batch_bytes));
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
        let first = builtin_registry(&session, ReadWindows::default());
        let second = builtin_registry(&session, ReadWindows::default());

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
