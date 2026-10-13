//! Refuse write-family calls whose target is not a regular file.
//!
//! The write family replaces a file's content, and a destination that holds
//! no content is not something replacement means anything by. A whole-file
//! write to a pipe path settles by renaming an ordinary file into place, so
//! it unlinks the endpoint another process is waiting on; a character or
//! block device reads without end, so a tool that consults the target first
//! never finishes with it. Neither is a file the model can be understood to
//! have meant, so this middleware settles the target's kind from
//! metadata — its own verdict opens nothing, which is what keeps the
//! gate from ever waiting on the destination it is judging — and
//! refuses the call there, pointing at the shell for a destination
//! that is genuinely a pipe or a device — writing to
//! one is an ordinary act there and an impossible one here. What the
//! gate cannot settle is the moment after its verdict: an entry
//! swapped between this refusal-check and the guarded tool's own open
//! is a window only the reading side can close, and these tools are
//! the engine's. It installs
//! beside [`with_redaction`](crate::redaction_guard::with_redaction) and
//! therefore guards a session started with redaction on; a session
//! running with it off keeps the raw route, and `FileViewer` counting
//! lines over a FIFO stays outside this gate.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolDispatchResult;
use loopctl::middleware::ToolMiddleware;
use loopctl::middleware::ToolPipeline;

use crate::redaction_guard::write_target;

/// The registered name of the whole-file write tool this gate covers.
///
/// A dispatch under this name replaces its target's entire content, which
/// begins by reading the target whole — the route a non-regular destination
/// stalls or exhausts. Matched by name, so a registry that renames the tool
/// must carry this constant in step.
const WRITE_TOOL: &str = "Write";

/// The registered name of the single-substitution edit tool.
///
/// An edit rewrites the whole file after reading it, so a FIFO target
/// stalls this call exactly as it stalls a whole-file write.
const EDIT_TOOL: &str = "Edit";

/// The registered name of the batched edit tool.
///
/// One call names several targets, each read before it is written, so the
/// gate examines every path the batch carries rather than the first.
const MULTI_EDIT_TOOL: &str = "MultiEdit";

/// The input field carrying the file a `Write` or `Edit` call replaces.
///
/// Both tools name their single destination here, which is what lets one
/// lookup serve them; a call whose input omits it has no target for the
/// gate to settle, so it passes through for the tool to reject.
const FILE_PATH_FIELD: &str = "file_path";

/// The input field carrying a `MultiEdit` call's edit operations.
///
/// A batch spans files, so this tool carries no top-level destination —
/// each entry of the array names its own, and the gate walks the array for
/// the same reason the tool does.
const EDITS_FIELD: &str = "edits";

/// Refuse write-family calls whose target exists and is not a regular file.
///
/// Installed by [`with_redaction`](crate::redaction_guard::with_redaction)
/// outside every other layer, so a call at an unreplaceable destination is
/// answered here rather than at the tool. The gate holds no state: its
/// verdict comes from the target's metadata at dispatch time, and the
/// gate's own route to that verdict performs no open — a refusal cannot
/// stall on what it refuses. The entry may still be swapped before the
/// guarded tool opens it moments later; that window belongs to the tool's
/// own pre-read, outside this type.
#[derive(Debug, Default)]
pub(crate) struct WriteTargetGate;

impl WriteTargetGate {
    /// Build the gate.
    ///
    /// Nothing to configure — the classification reads each target's
    /// metadata as the call arrives.
    pub(crate) const fn new() -> Self {
        Self
    }

    /// The refused-dispatch result for a target the write family cannot
    /// replace.
    ///
    /// A soft error in the overwrite guard's idiom, so the model reads why
    /// and can redirect: naming the kind found is what makes the guidance
    /// actionable, since writing to a pipe is a legitimate act through the
    /// shell and an impossible one through a file-replacement tool.
    fn refusal(ctx: &ToolDispatchContext, target: &str, kind: &str) -> ToolDispatchResult {
        let mut refusal = ToolDispatchResult::err(
            &ctx.tool_name,
            format!(
                "refused: `{target}` is {kind}, not a regular file. The write family replaces \
                 file content, and a destination that holds none is not what this call can \
                 mean — a write here would rename an ordinary file over it. Reach a pipe or \
                 device through Bash instead."
            ),
            Duration::ZERO,
        );
        refusal.tool_call_id.clone_from(&ctx.call_id);
        refusal
    }
}

impl ToolMiddleware for WriteTargetGate {
    fn name(&self) -> &'static str {
        "write-target-gate"
    }

    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ToolDispatchContext,
        next: &'a ToolPipeline,
    ) -> Pin<Box<dyn Future<Output = ToolDispatchResult> + Send + 'a>> {
        Box::pin(async move {
            let cwd = Path::new(&ctx.tool_context.cwd).to_path_buf();
            for target in dispatch_targets(ctx) {
                if let Some(kind) = non_regular_kind(&write_target(&target, &cwd)) {
                    return Self::refusal(ctx, &target, kind);
                }
            }
            next.dispatch(ctx).await
        })
    }
}

/// The file paths a write-family dispatch would overwrite, in call order.
///
/// `Write` and `Edit` name one target at the top level; a `MultiEdit` call
/// carries a path per entry of its `edits` array, because one batch spans
/// several files. Any other dispatch returns nothing, which is how the gate
/// stays out of every call it was not asked to police.
fn dispatch_targets(ctx: &ToolDispatchContext) -> Vec<String> {
    match ctx.tool_name.as_str() {
        WRITE_TOOL | EDIT_TOOL => ctx
            .input
            .get(FILE_PATH_FIELD)
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .into_iter()
            .collect(),
        MULTI_EDIT_TOOL => ctx
            .input
            .get(EDITS_FIELD)
            .and_then(|value| value.as_array())
            .map(|edits| {
                edits
                    .iter()
                    .filter_map(|edit| edit.get(FILE_PATH_FIELD))
                    .filter_map(|value| value.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The kind of an existing target the write family cannot replace.
///
/// Metadata answers this without an open, which is the whole point: opening
/// a FIFO is what blocks. An absent target is `None`, because creating a
/// new file is the ordinary route and the tool decides; a symlink resolves
/// to whatever it leads to, so a link at a pipe is refused and a link at an
/// ordinary file passes to the guard that reads content.
fn non_regular_kind(path: &Path) -> Option<&'static str> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return None;
    };
    if metadata.is_file() {
        return None;
    }
    Some(target_kind(&metadata))
}

/// Name what a non-regular target is, for the refusal the model reads.
///
/// The kinds are ordered by how each one fails, so a FIFO — the destination
/// that blocks — is distinguished from a device, which instead reads without
/// end. Something the classification does not name still gets a truthful
/// label rather than a wrong one.
#[cfg(unix)]
fn target_kind(metadata: &std::fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt as _;

    let file_type = metadata.file_type();
    if file_type.is_fifo() {
        "a FIFO"
    } else if file_type.is_char_device() {
        "a character device"
    } else if file_type.is_block_device() {
        "a block device"
    } else if file_type.is_socket() {
        "a socket"
    } else if file_type.is_dir() {
        "a directory"
    } else {
        "not a regular file"
    }
}

/// Name what a non-regular target is, where the typed kinds are unavailable.
///
/// A directory is the one non-regular kind portable metadata recognizes;
/// everything else is reported by the property that matters — it is not a
/// file the write family can replace — rather than guessed at.
#[cfg(not(unix))]
fn target_kind(metadata: &std::fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        "a directory"
    } else {
        "not a regular file"
    }
}
