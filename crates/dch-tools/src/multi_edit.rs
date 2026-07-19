//! The `MultiEdit` tool — apply a batch of text edits across one or more files.
//!
//! Every edit in the batch is validated before any file is written. One
//! invalid edit (missing file, `old_text` not found or not unique, target is a
//! symlink, linter rejects the merged content, two edits overlap in the same
//! file) aborts the entire batch — no file is touched. `dry_run: true` previews
//! diffs without writing.
//!
//! The atomicity guarantee covers *validation*: if any edit is invalid, nothing
//! is written. It does **not** cover crashes mid-write-batch — see the
//! "Atomicity scope" section on [`MultiEditTool`]'s call docs.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;

use crate::context::RunnerContext;
use crate::context::runner_ctx;
use crate::diff::format_file_change;
use crate::edit::FindResult;
use crate::edit::locate_unique;
use crate::edit::splice;
use crate::linter::lint_content;
use crate::util::is_url;
use crate::util::resolve_path;
use crate::write::format_lint_failure;

/// Maximum number of edits permitted in a single call.
const MAX_EDITS: usize = 50;

/// Edit multiple files atomically. All edits are validated before any writes.
/// Use `dry_run=true` to preview changes without writing.
///
/// Not concurrency-safe and not read-only: it mutates files, and two
/// concurrent batches touching overlapping paths would race.
pub struct MultiEditTool;

impl Tool for MultiEditTool {
    fn name(&self) -> &'static str {
        "MultiEdit"
    }

    fn description(&self) -> &'static str {
        "Edit multiple files atomically. All edits are validated before any \
         writes. Use dry_run=true to preview changes without writing."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "description": "Array of edit operations to perform",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file_path": { "type": "string", "description": "The path to the file to edit" },
                                "old_text": { "type": "string", "description": "The text to replace" },
                                "new_text": { "type": "string", "description": "The replacement text" }
                            },
                            "required": ["file_path", "old_text", "new_text"]
                        },
                        "minItems": 1,
                        "maxItems": MAX_EDITS
                    },
                    "dry_run": { "type": "boolean", "description": "Preview changes without writing files", "default": false },
                    "skip_linter": { "type": "boolean", "description": "Skip syntax validation", "default": false }
                },
                "required": ["edits"]
            }),
        }
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let rc = runner_ctx(ctx).cloned();
        Box::pin(self.multi_edit_inner(input, rc))
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }
}

impl MultiEditTool {
    /// Body of [`Tool::call`].
    ///
    /// Orchestrates the five-phase pipeline: parse → read+locate+symlink →
    /// overlap-detect → lint → preview/write. Recoverable conditions (text not
    /// found, ambiguous match, overlap, symlink target, linter failure) are
    /// surfaced as soft [`ToolOutput`] errors; hard failures (bad args,
    /// missing file, I/O fault) become [`ToolError`].
    ///
    /// # Atomicity scope
    ///
    /// The all-or-nothing guarantee covers *validation*: by the time any write
    /// happens, every edit has been validated and every file's merged content
    /// has passed the linter. Writes are individual atomic file replacements
    /// (temp-then-rename via [`atomic_write`](crate::fs::atomic_write)), but
    /// the batch of writes is **not** a single filesystem transaction — a
    /// process crash mid-batch could leave some files written and others not.
    /// The partial result is at worst *incomplete*, never syntactically
    /// corrupt, because every file's content already passed the linter.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for a missing `edits` array, an
    /// empty array, more than `MAX_EDITS` edits, a missing field, an empty
    /// `old_text`, or a URL `file_path`. Returns [`ToolError::FileNotFound`]
    /// when a target does not exist. Returns [`ToolError::Execution`] on a
    /// genuine I/O fault or a missing [`RunnerContext`].
    async fn multi_edit_inner(
        &self,
        input: Value,
        rc: Option<RunnerContext>,
    ) -> Result<ToolOutput, ToolError> {
        let cwd = rc
            .as_ref()
            .ok_or_else(|| {
                ToolError::Execution(
                    "RunnerContext extension is not installed on the ToolContext".to_string(),
                )
            })?
            .cwd
            .clone();

        // Phase 0: parse + bounds-check.
        let parsed = parse_input(&input)?;
        let operations = build_operations(parsed.edits, &cwd)?;

        // Phase 1: read each distinct file once, pre-check symlinks.
        if let Some(reason) = dup_path_check(&operations) {
            return Ok(reason.into_output());
        }
        let originals = read_files(&operations).await?;
        if let Some(reason) = symlink_check(&operations) {
            return Ok(reason.into_output());
        }

        // Phase 1.5: overlap detection. Runs in both dry_run and apply modes
        // so the preview shows exactly what the apply would catch.
        if let Some(reason) = overlap_check(&operations, &originals) {
            return Ok(reason.into_output());
        }

        // Phase 2: merge each file's edits sequentially (array order, each
        // seeing the prior's output), locating old_text in the *running*
        // content (unique check). This is also where edit #N's old_text that
        // only exists after edit #N-1 is validated.
        let finals = match merge_per_file(&operations, &originals) {
            Ok(f) => f,
            Err(reason) => return Ok(reason.into_output()),
        };
        if !parsed.skip_linter {
            if let Some(reason) = lint_all(&operations, &finals) {
                return Ok(reason.into_output());
            }
        }

        // Phase 3: build the preview/diff block (always).
        let summary = build_preview(&operations, &originals, &finals, parsed.dry_run);
        if parsed.dry_run {
            return Ok(ToolOutput::text(summary));
        }

        // Phase 4: write each distinct physical file once.
        let mut written: std::collections::HashSet<&Path> = std::collections::HashSet::new();
        for op in &operations {
            if !written.insert(&op.full_path) {
                continue;
            }
            if let Some(final_content) = finals.get(&op.file_path) {
                crate::fs::atomic_write(&op.full_path, final_content)?;
            }
        }

        let applied: Vec<&str> = finals.keys().map(String::as_str).collect();
        let message = apply_summary(&summary, &applied, &operations);
        Ok(ToolOutput::text(message))
    }
}

/// Parsed top-level `MultiEdit` input.
///
/// Produced by [`parse_input`]. Carries the validated edits array plus the two
/// option flags, consumed by the rest of the pipeline.
#[derive(Debug)]
struct ParsedInput<'a> {
    /// The edits array, borrowed from the caller's input. Validated non-empty
    /// and within `MAX_EDITS` by [`parse_input`] before this struct is built.
    edits: &'a [Value],
    /// Whether to preview without writing.
    dry_run: bool,
    /// Whether to skip the linter gate on the merged content.
    skip_linter: bool,
}

/// Extract the top-level `MultiEdit` arguments and bounds-check the edits array.
///
/// `edits` must be a non-empty array of length ≤ `MAX_EDITS`; `dry_run` and
/// `skip_linter` default to `false`.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] for a missing `edits` array, an empty
/// array, or more than `MAX_EDITS` edits.
fn parse_input(input: &Value) -> Result<ParsedInput<'_>, ToolError> {
    let edits = input
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidInput("Missing edits array".to_string()))?;
    if edits.is_empty() {
        return Err(ToolError::InvalidInput(
            "Edits array cannot be empty".to_string(),
        ));
    }
    if edits.len() > MAX_EDITS {
        return Err(ToolError::InvalidInput(format!(
            "Too many edits: maximum {MAX_EDITS} allowed, got {}",
            edits.len()
        )));
    }
    let dry_run = input
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let skip_linter = input
        .get("skip_linter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ParsedInput {
        edits,
        dry_run,
        skip_linter,
    })
}

/// One parsed edit operation, with the resolved absolute path.
///
/// Built by [`build_operations`] from each item in the caller's `edits` array.
/// The `file_path` is kept verbatim (pre-resolution) for error and preview
/// messages; `full_path` is what every read/write actually targets.
#[derive(Debug, Clone)]
struct EditOperation {
    /// The caller-supplied path (pre-resolution), used in messages so the model
    /// sees the path it named, not the canonicalized form.
    file_path: String,
    /// The path resolved against the runner's `cwd` — what every read/write
    /// and the duplicate-path / symlink checks actually target.
    full_path: PathBuf,
    /// The text to find in the file.
    old_text: String,
    /// The text to replace `old_text` with.
    new_text: String,
}

/// Parse each edit item into an [`EditOperation`], validating fields and
/// rejecting empty `old_text` and URL `file_path`.
///
/// Relative paths are resolved against `cwd`; absolute paths are used as-is.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] for a missing `file_path`/`old_text`/
/// `new_text`, an empty `old_text`, or a URL `file_path`.
fn build_operations(edits: &[Value], cwd: &Path) -> Result<Vec<EditOperation>, ToolError> {
    let mut operations = Vec::with_capacity(edits.len());
    for edit_value in edits {
        let file_path = edit_value
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing file_path in edit".to_string()))?;
        let old_text = edit_value
            .get("old_text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing old_text in edit".to_string()))?;
        let new_text = edit_value
            .get("new_text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing new_text in edit".to_string()))?;

        if old_text.is_empty() {
            return Err(ToolError::InvalidInput(
                "old_text must not be empty".to_string(),
            ));
        }
        if is_url(file_path) {
            return Err(ToolError::InvalidInput(
                "URLs are not supported by the MultiEdit tool. Use WebFetch for URLs.".to_string(),
            ));
        }

        let full_path = normalize_path(&resolve_path(file_path, cwd));
        operations.push(EditOperation {
            file_path: file_path.to_string(),
            full_path,
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }
    Ok(operations)
}

/// Lexically normalize a path: collapse `.` components and resolve `..`
/// against preceding components, without touching the filesystem.
///
/// This makes path aliases like `a.rs` and `./a.rs` (or `src/../src/a.rs`)
/// compare equal, so [`dup_path_check`] catches them as duplicates. It does
/// **not** follow symlinks (unlike [`std::fs::canonicalize`]); symlink
/// detection is [`symlink_check`]'s job.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Read each distinct target file once into a map keyed by `file_path`.
///
/// Returns the original contents keyed by the caller-supplied path (so the
/// preview can address files the way the model named them).
///
/// # Errors
///
/// Returns [`ToolError::FileNotFound`] when a target does not exist, and
/// [`ToolError::Execution`] on any other read fault (including non-UTF-8).
async fn read_files(operations: &[EditOperation]) -> Result<BTreeMap<String, String>, ToolError> {
    let mut originals = BTreeMap::new();
    for op in operations {
        if originals.contains_key(&op.file_path) {
            continue;
        }
        if !tokio::fs::try_exists(&op.full_path)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?
        {
            return Err(ToolError::FileNotFound(op.file_path.clone()));
        }
        let content = tokio::fs::read_to_string(&op.full_path)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        originals.insert(op.file_path.clone(), content);
    }
    Ok(originals)
}

/// A recoverable reason a batch was aborted before any write.
///
/// Each variant carries a pre-formatted message and is converted to a soft
/// [`ToolOutput`] via [`AbortReason::into_output`]. Distinct from a hard
/// [`ToolError`], which is reserved for bad arguments, missing files, and I/O
/// faults that the model cannot simply retry around.
#[derive(Debug)]
enum AbortReason {
    /// One edit's target is a symbolic link (named in the message). Produced
    /// by the Phase 1 symlink pre-check, before any file is read or written.
    Symlink(String),
    /// Two edits resolve to the same physical file under different path
    /// aliases (named in the message) — the result would be ambiguous.
    /// Produced by the Phase 1 duplicate-path check.
    DupPath(String),
    /// One edit's `old_text` is absent or not unique in the running content
    /// (named in the message). Produced by [`merge_per_file`] during Phase 2.
    Locate(String),
    /// Two edits' byte-ranges overlap in the same file (named in the message).
    /// Produced by the Phase 1.5 overlap check (skipped in `dry_run`).
    Overlap(String),
    /// The linter rejected one file's merged content (named in the message).
    /// Produced by the Phase 2 linter gate (skipped when `skip_linter`).
    Lint(String),
}

impl AbortReason {
    /// Format this reason as the soft [`ToolOutput`] returned to the loop.
    ///
    /// Every variant carries a pre-formatted human-readable message; this just
    /// wraps it as an error output so the model can read the reason and retry.
    fn into_output(self) -> ToolOutput {
        match self {
            AbortReason::Symlink(msg)
            | AbortReason::DupPath(msg)
            | AbortReason::Locate(msg)
            | AbortReason::Overlap(msg)
            | AbortReason::Lint(msg) => ToolOutput::error_text(msg),
        }
    }
}

/// Reject the batch if two edits resolve to the same physical file under
/// different path aliases (e.g. `a.rs` and `./a.rs`).
///
/// Each edit would otherwise be merged independently against the same original,
/// and the second write would silently clobber the first — losing one
/// edit-set. Refusing is safer than picking a winner. Multiple edits sharing
/// both `file_path` and `full_path` (the normal multi-edit-to-one-file case)
/// are allowed.
fn dup_path_check(operations: &[EditOperation]) -> Option<AbortReason> {
    // Map each resolved path to the first caller-supplied path seen for it.
    let mut owner: std::collections::HashMap<&Path, &str> = std::collections::HashMap::new();
    for op in operations {
        match owner.get(op.full_path.as_path()) {
            Some(&existing) if existing != op.file_path => {
                return Some(AbortReason::DupPath(format!(
                    "Edits target the same file via two different paths: '{}' and '{}' both \
                     resolve to '{}'. Combine them into one set of edits.",
                    existing,
                    op.file_path,
                    op.full_path.display()
                )));
            }
            None => {
                owner.insert(op.full_path.as_path(), &op.file_path);
            }
            _ => {}
        }
    }
    None
}

/// Reject the batch if any distinct target — or any of its ancestor
/// directories — is a symbolic link.
///
/// `atomic_write`'s own symlink guard fires at write time and checks only the
/// final component, too late for the atomic contract (file #1 could already be
/// written before file #2's symlink errors). This pre-check walks every
/// ancestor of each target with `symlink_metadata` (no follow) during the read
/// pass, before any write, so a symlinked parent directory is caught too.
fn symlink_check(operations: &[EditOperation]) -> Option<AbortReason> {
    let mut seen = std::collections::HashSet::new();
    for op in operations {
        // Walk from the target up through each ancestor, without following links.
        for ancestor in op.full_path.ancestors() {
            if !seen.insert(ancestor) {
                continue;
            }
            if std::fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
                return Some(AbortReason::Symlink(format!(
                    "Refusing to write: {} crosses a symbolic link ({}). \
                     Resolve it and pass the real path.",
                    op.file_path,
                    ancestor.display()
                )));
            }
        }
    }
    None
}

/// A pair of edits whose `old_text` byte-ranges overlap in the same file.
///
/// Produced by [`detect_edit_conflicts`]. Each field names the two edits (by
/// their 0-indexed position in the batch and a truncated snippet of their
/// `old_text`) so the abort message can point the caller at both.
#[derive(Debug, Clone)]
struct EditConflict {
    /// The file both edits target.
    ///
    /// Stored as the caller-supplied path (pre-resolution), not the normalized
    /// form, so the abort message addresses the file the way the model named it.
    file_path: String,

    /// 0-indexed position of the first edit in the batch's `edits` array.
    ///
    /// The raw value is used for the dedup-by-sorted-pair step in
    /// [`detect_edit_conflicts`]; the abort message renders it 1-indexed
    /// (`edit_index_a + 1`) for human readability.
    edit_index_a: usize,

    /// 0-indexed position of the second edit in the batch's `edits` array.
    ///
    /// Paired with [`edit_index_a`](Self::edit_index_a) to identify the
    /// conflicting pair; rendered 1-indexed in the abort message.
    edit_index_b: usize,

    /// Truncated `old_text` of the first edit, for the abort message.
    ///
    /// Produced by [`truncate_str`] so a long needle doesn't flood the output;
    /// the caller sees enough to recognize the edit without the full text.
    snippet_a: String,

    /// Truncated `old_text` of the second edit, for the abort message.
    ///
    /// Produced by [`truncate_str`]; paired with [`snippet_a`](Self::snippet_a)
    /// so the message shows both halves of the overlapping pair.
    snippet_b: String,
}

/// Detect pairs of edits whose `old_text` ranges overlap in the same file.
///
/// Two edits to one file conflict when one's matched byte-range intersects the
/// other's (one contains the other, or they share text). Applying either first
/// would invalidate the other's match. Different files never conflict, even
/// with identical `old_text`. Ported from the salvage source.
fn detect_edit_conflicts(
    operations: &[EditOperation],
    file_contents: &BTreeMap<String, String>,
) -> Vec<EditConflict> {
    let mut conflicts = Vec::new();

    // Group edits by file.
    let mut file_edits: BTreeMap<String, Vec<(usize, &str)>> = BTreeMap::new();
    for (i, op) in operations.iter().enumerate() {
        file_edits
            .entry(op.file_path.clone())
            .or_default()
            .push((i, &op.old_text));
    }

    for (file_path, edits) in &file_edits {
        // Only check files with 2+ edits.
        if edits.len() < 2 {
            continue;
        }
        let Some(content) = file_contents.get(file_path) else {
            continue;
        };

        // One range per edit (uniqueness already enforced by locate_unique).
        let mut ranges: Vec<(usize, usize, usize, &str)> = Vec::new();
        for (edit_idx, old_text) in edits {
            if let Some(start) = content.find(old_text) {
                let end = start.saturating_add(old_text.len());
                let snippet = truncate_str(old_text, 60);
                ranges.push((start, end, *edit_idx, snippet));
            }
        }

        // Sort by start position.
        ranges.sort_by_key(|r| r.0);

        // Check for overlapping ranges between different edits.
        for i in 0..ranges.len() {
            let Some(&(_start_a, end_a, idx_a, snippet_a)) = ranges.get(i) else {
                continue;
            };
            for &entry in ranges.iter().skip(i.saturating_add(1)) {
                let (start_b, _end_b, idx_b, snippet_b) = entry;
                if idx_a == idx_b {
                    continue;
                }
                if start_b < end_a {
                    conflicts.push(EditConflict {
                        file_path: file_path.clone(),
                        edit_index_a: idx_a,
                        edit_index_b: idx_b,
                        snippet_a: snippet_a.to_string(),
                        snippet_b: snippet_b.to_string(),
                    });
                }
            }
        }
    }

    // Deduplicate by sorted index pair.
    let mut seen = std::collections::HashSet::new();
    conflicts.retain(|c| {
        let key = (
            c.edit_index_a.min(c.edit_index_b),
            c.edit_index_a.max(c.edit_index_b),
        );
        seen.insert(key)
    });

    conflicts
}

/// Truncate to at most `max_len` bytes, landing on a UTF-8 char boundary.
///
/// Used to keep `old_text` snippets short in conflict messages. If `max_len`
/// falls inside a multibyte char, the cut backs up to the preceding boundary
/// so the result is always valid UTF-8.
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end = end.saturating_sub(1);
        }
        s.get(..end).unwrap_or(s)
    }
}

/// Reject the batch if any two edits' byte-ranges overlap in the same file.
///
/// Delegates the detection to [`detect_edit_conflicts`] and, on the first
/// conflict, formats a message naming both edits (by 1-indexed position and a
/// truncated `old_text` snippet) with a hint to split the batch or dry-run.
fn overlap_check(
    operations: &[EditOperation],
    originals: &BTreeMap<String, String>,
) -> Option<AbortReason> {
    use std::fmt::Write;
    let conflicts = detect_edit_conflicts(operations, originals);
    if conflicts.is_empty() {
        return None;
    }
    let mut msg =
        "Edit conflict detected — the following edits overlap in the same file:\n".to_string();
    for conflict in &conflicts {
        writeln!(
            msg,
            "  - File '{}': edit #{} and edit #{} target overlapping text regions",
            conflict.file_path,
            conflict.edit_index_a.saturating_add(1),
            conflict.edit_index_b.saturating_add(1)
        )
        .ok();
        writeln!(
            msg,
            "    Edit #{} old_text: {:?}...",
            conflict.edit_index_a.saturating_add(1),
            conflict.snippet_a
        )
        .ok();
        writeln!(
            msg,
            "    Edit #{} old_text: {:?}...",
            conflict.edit_index_b.saturating_add(1),
            conflict.snippet_b
        )
        .ok();
    }
    msg.push_str(
        "\nResolve by: (1) splitting into separate calls, or (2) use dry_run=true to preview first.",
    );
    Some(AbortReason::Overlap(msg))
}

/// Merge each file's edits sequentially (array order, each seeing the prior's
/// output) into a final content map, validating each edit's `old_text` is
/// unique in the *running* content at that point.
///
/// A later edit to the same file may target text that only exists after an
/// earlier edit runs — so the locate check must be against the accumulated
/// content, not the original.
///
/// # Errors
///
/// Returns `Err(AbortReason::Locate)` on the first edit whose `old_text` is
/// absent or not unique in the running content at that point in the sequence.
fn merge_per_file(
    operations: &[EditOperation],
    originals: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, AbortReason> {
    let mut finals: BTreeMap<String, String> = BTreeMap::new();
    for op in operations {
        let entry = finals
            .entry(op.file_path.clone())
            .or_insert_with(|| originals.get(&op.file_path).cloned().unwrap_or_default());
        match locate_unique(entry, &op.old_text) {
            FindResult::NotFound => {
                return Err(AbortReason::Locate(format!(
                    "Old text not found in file: {}",
                    op.file_path
                )));
            }
            FindResult::Ambiguous { count } => {
                return Err(AbortReason::Locate(format!(
                    "old_text appears {count} times in file {}; it must be unique. \
                     Add surrounding context to disambiguate, or use Edit.",
                    op.file_path
                )));
            }
            FindResult::Unique(range) => {
                *entry = splice(entry, range, &op.new_text);
            }
        }
    }
    Ok(finals)
}

/// Lint each distinct file's final merged content; return the first failure.
///
/// Iterates the operations but lints each physical file only once (deduped by
/// `full_path`). A failure produces [`AbortReason::Lint`] carrying the
/// formatted diagnostics plus the "No files were modified" trailer.
fn lint_all(
    operations: &[EditOperation],
    finals: &BTreeMap<String, String>,
) -> Option<AbortReason> {
    let mut seen = std::collections::HashSet::new();
    for op in operations {
        if !seen.insert(&op.full_path) {
            continue;
        }
        let Some(final_content) = finals.get(&op.file_path) else {
            continue;
        };
        let result = lint_content(&op.full_path, final_content);
        if !result.is_valid {
            return Some(AbortReason::Lint(format!(
                "{}\n\nNo files were modified.",
                format_lint_failure(&op.full_path, &result).trim_end()
            )));
        }
    }
    None
}

/// Build the per-file diff preview block, with a header chosen by `dry_run`.
///
/// Lists each distinct file once (in first-seen order across the batch), then
/// the indented output of [`format_file_change`](crate::diff::format_file_change)
/// comparing that file's *original* content against its *fully merged* final
/// content from `finals` — so multiple edits to one file render cumulatively,
/// not as isolated fragments. The dry-run path appends a footer telling the
/// caller how to apply; the apply path reuses this block as the summary header.
fn build_preview(
    operations: &[EditOperation],
    originals: &BTreeMap<String, String>,
    finals: &BTreeMap<String, String>,
    dry_run: bool,
) -> String {
    let mut lines = Vec::new();
    if dry_run {
        lines.push("Dry Run Preview — No files will be modified".to_string());
    } else {
        lines.push("Multi-File Edit Summary".to_string());
    }
    lines.push(String::new());

    // Distinct files in first-seen order, so the preview follows the batch.
    let mut seen = std::collections::HashSet::new();
    let mut index = 1usize;
    for op in operations {
        if !seen.insert(&op.file_path) {
            continue;
        }
        lines.push(format!("File {index}: {}", op.file_path));
        let original = originals.get(&op.file_path).map_or("", String::as_str);
        let final_content = finals.get(&op.file_path).map_or("", String::as_str);
        let diff = format_file_change(&op.file_path, Some(original), final_content);
        for line in diff.lines() {
            lines.push(format!("  {line}"));
        }
        lines.push(String::new());
        index = index.saturating_add(1);
    }

    if dry_run {
        lines.push("Use dry_run=false to apply these changes.".to_string());
    }
    lines.join("\n")
}

/// Append the applied-files summary to the preview block.
///
/// Called only on the apply path (not dry-run). Lists each written file once,
/// with its edit count when more than one edit targeted it.
fn apply_summary(preview: &str, applied: &[&str], operations: &[EditOperation]) -> String {
    let mut result = preview.to_string();
    result.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    writeln!(result, "Applied: {} file(s)", applied.len()).ok();
    for path in applied {
        let count = operations
            .iter()
            .filter(|o| &o.file_path.as_str() == path)
            .count();
        if count > 1 {
            writeln!(result, "  + {path} ({count} edits)").ok();
        } else {
            writeln!(result, "  + {path}").ok();
        }
    }
    result
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::field_reassign_with_default,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use crate::runtime::RuntimeConfig;
    use crate::state::SessionState;
    use loopctl::tool::ToolContext;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Builds a `ToolContext` with a `RunnerContext` pointing at `cwd`.
    fn ctx_in(cwd: &str) -> ToolContext {
        let mut ctx = ToolContext::default();
        ctx.cwd = cwd.to_string();
        let rc = RunnerContext {
            cwd: PathBuf::from(cwd),
            session_state: Arc::new(Mutex::new(SessionState::default())),
            question_tx: None,
            runtime: RuntimeConfig::default(),
        };
        ctx.set_extension(rc);
        ctx
    }

    fn edit(file_path: &str, old_text: &str, new_text: &str) -> Value {
        json!({ "file_path": file_path, "old_text": old_text, "new_text": new_text })
    }

    #[test]
    fn max_edits_in_range() {
        const {
            assert!(MAX_EDITS > 1);
            assert!(MAX_EDITS <= 100);
        }
    }

    #[test]
    fn conflict_overlapping_edits_to_one_file() {
        let mut originals = BTreeMap::new();
        originals.insert(
            "test.rs".to_string(),
            "fn main() {\n    println!(\"hello\");\n    println!(\"world\");\n}\n".to_string(),
        );
        let ops = vec![
            EditOperation {
                file_path: "test.rs".to_string(),
                full_path: PathBuf::from("test.rs"),
                old_text: "println!(\"hello\");\n    println!(\"world\")".to_string(),
                new_text: "println!(\"hi\")".to_string(),
            },
            EditOperation {
                file_path: "test.rs".to_string(),
                full_path: PathBuf::from("test.rs"),
                old_text: "println!(\"world\")".to_string(),
                new_text: "println!(\"universe\")".to_string(),
            },
        ];
        let conflicts = detect_edit_conflicts(&ops, &originals);
        assert_eq!(conflicts.len(), 1, "got {conflicts:?}");
        assert_eq!(conflicts.first().unwrap().file_path, "test.rs");
    }

    #[test]
    fn conflict_non_overlapping_edits_to_one_file() {
        let mut originals = BTreeMap::new();
        originals.insert(
            "test.rs".to_string(),
            "fn foo() {}\nfn bar() {}\nfn baz() {}\n".to_string(),
        );
        let ops = vec![
            EditOperation {
                file_path: "test.rs".to_string(),
                full_path: PathBuf::from("test.rs"),
                old_text: "fn foo() {}".to_string(),
                new_text: "fn foo(x: i32) {}".to_string(),
            },
            EditOperation {
                file_path: "test.rs".to_string(),
                full_path: PathBuf::from("test.rs"),
                old_text: "fn baz() {}".to_string(),
                new_text: "fn baz(x: i32) {}".to_string(),
            },
        ];
        let conflicts = detect_edit_conflicts(&ops, &originals);
        assert!(conflicts.is_empty(), "got {conflicts:?}");
    }

    #[test]
    fn conflict_identical_old_text_different_files_no_conflict() {
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "hello world".to_string());
        originals.insert("b.rs".to_string(), "hello world".to_string());
        let ops = vec![
            EditOperation {
                file_path: "a.rs".to_string(),
                full_path: PathBuf::from("a.rs"),
                old_text: "hello".to_string(),
                new_text: "hi".to_string(),
            },
            EditOperation {
                file_path: "b.rs".to_string(),
                full_path: PathBuf::from("b.rs"),
                old_text: "hello".to_string(),
                new_text: "hey".to_string(),
            },
        ];
        let conflicts = detect_edit_conflicts(&ops, &originals);
        assert!(conflicts.is_empty(), "got {conflicts:?}");
    }

    #[test]
    fn truncate_str_respects_char_boundary() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
        // Multibyte: must not split a char.
        let truncated = truncate_str("héllo", 2);
        assert_eq!(truncated, "h");
    }

    /// Build an `EditOperation` for a helper test (path not resolved).
    fn op(file_path: &str, old_text: &str, new_text: &str) -> EditOperation {
        EditOperation {
            file_path: file_path.to_string(),
            full_path: PathBuf::from(file_path),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn parse_input_valid_defaults_flags() {
        let input = json!({ "edits": [edit("a.rs", "x", "y")] });
        let parsed = parse_input(&input).unwrap();
        assert_eq!(parsed.edits.len(), 1);
        assert!(!parsed.dry_run);
        assert!(!parsed.skip_linter);
    }

    #[test]
    fn parse_input_honors_flags() {
        let input =
            json!({ "edits": [edit("a.rs", "x", "y")], "dry_run": true, "skip_linter": true });
        let parsed = parse_input(&input).unwrap();
        assert!(parsed.dry_run);
        assert!(parsed.skip_linter);
    }

    #[test]
    fn parse_input_missing_array_is_invalid() {
        assert!(matches!(
            parse_input(&json!({})).unwrap_err(),
            ToolError::InvalidInput(_)
        ));
    }

    #[test]
    fn parse_input_empty_array_is_invalid() {
        assert!(matches!(
            parse_input(&json!({ "edits": [] })).unwrap_err(),
            ToolError::InvalidInput(_)
        ));
    }

    #[test]
    fn parse_input_too_many_is_invalid() {
        let too_many: Vec<Value> = (0..=MAX_EDITS).map(|_| edit("a.rs", "x", "y")).collect();
        assert!(matches!(
            parse_input(&json!({ "edits": too_many })).unwrap_err(),
            ToolError::InvalidInput(_)
        ));
    }

    #[test]
    fn build_operations_resolves_relative_path() {
        let edits = vec![edit("a.rs", "x", "y")];
        let ops = build_operations(&edits, Path::new("/work")).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].file_path, "a.rs");
        assert_eq!(ops[0].full_path, PathBuf::from("/work/a.rs"));
    }

    #[test]
    fn build_operations_keeps_absolute_path() {
        let edits = vec![edit("/abs/a.rs", "x", "y")];
        let ops = build_operations(&edits, Path::new("/work")).unwrap();
        assert_eq!(ops[0].full_path, PathBuf::from("/abs/a.rs"));
    }

    #[test]
    fn build_operations_missing_fields_are_invalid() {
        let cwd = Path::new("/work");
        assert!(matches!(
            build_operations(&[json!({ "old_text": "x", "new_text": "y" })], cwd).unwrap_err(),
            ToolError::InvalidInput(_)
        ));
        assert!(matches!(
            build_operations(&[json!({ "file_path": "a", "new_text": "y" })], cwd).unwrap_err(),
            ToolError::InvalidInput(_)
        ));
        assert!(matches!(
            build_operations(&[json!({ "file_path": "a", "old_text": "x" })], cwd).unwrap_err(),
            ToolError::InvalidInput(_)
        ));
    }

    #[test]
    fn build_operations_empty_old_text_is_invalid() {
        let cwd = Path::new("/work");
        let err = build_operations(&[edit("a.rs", "", "y")], cwd).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(ref s) if s.contains("empty")));
    }

    #[test]
    fn build_operations_url_is_invalid() {
        let cwd = Path::new("/work");
        let err = build_operations(&[edit("https://e.com/x", "a", "b")], cwd).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")));
    }

    #[test]
    fn dup_path_check_distinct_paths_ok() {
        let ops = vec![op("a.rs", "x", "y"), op("b.rs", "z", "w")];
        assert!(dup_path_check(&ops).is_none());
    }

    #[test]
    fn dup_path_check_same_file_twice_ok() {
        // Same file_path AND full_path — the normal multi-edit case.
        let ops = vec![op("a.rs", "x", "y"), op("a.rs", "z", "w")];
        assert!(dup_path_check(&ops).is_none());
    }

    #[test]
    fn dup_path_check_alias_conflict_aborts() {
        // Two caller-supplied paths resolving to the same physical file.
        let mut ops = vec![op("a.rs", "x", "y"), op("./a.rs", "z", "w")];
        // Force the same full_path for both (alias resolution).
        ops[1].full_path = PathBuf::from("a.rs");
        let reason = dup_path_check(&ops).expect("should abort");
        assert!(matches!(reason, AbortReason::DupPath(_)));
        assert!(dup_path_check(&ops).unwrap().into_output().is_error);
    }

    #[tokio::test]
    async fn read_files_missing_is_file_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ops = vec![EditOperation {
            file_path: "x.rs".to_string(),
            full_path: tmp.path().join("x.rs"),
            old_text: "a".to_string(),
            new_text: "b".to_string(),
        }];
        let err = read_files(&ops).await.unwrap_err();
        assert!(matches!(err, ToolError::FileNotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn read_files_non_utf8_is_execution() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("bin.dat");
        std::fs::write(&target, b"\xFF\xFE").unwrap();
        let ops = vec![EditOperation {
            file_path: "bin.dat".to_string(),
            full_path: target,
            old_text: "a".to_string(),
            new_text: "b".to_string(),
        }];
        let err = read_files(&ops).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "{err:?}");
    }

    #[tokio::test]
    async fn read_files_reads_each_distinct_file_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "content\n").unwrap();
        // Two edits to the same file_path — must read once.
        let ops = vec![
            EditOperation {
                file_path: "a.rs".to_string(),
                full_path: f.clone(),
                old_text: "a".to_string(),
                new_text: "b".to_string(),
            },
            EditOperation {
                file_path: "a.rs".to_string(),
                full_path: f.clone(),
                old_text: "c".to_string(),
                new_text: "d".to_string(),
            },
        ];
        let map = read_files(&ops).await.unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("a.rs").unwrap(), "content\n");
    }

    #[test]
    fn symlink_check_regular_file_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "x\n").unwrap();
        let ops = vec![EditOperation {
            file_path: "a.rs".to_string(),
            full_path: f,
            old_text: "x".to_string(),
            new_text: "y".to_string(),
        }];
        assert!(symlink_check(&ops).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_check_symlink_aborts() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.rs");
        let link = tmp.path().join("link.rs");
        std::fs::write(&real, "x\n").unwrap();
        symlink(&real, &link).unwrap();
        let ops = vec![EditOperation {
            file_path: "link.rs".to_string(),
            full_path: link,
            old_text: "x".to_string(),
            new_text: "y".to_string(),
        }];
        let reason = symlink_check(&ops).expect("should abort");
        assert!(matches!(reason, AbortReason::Symlink(_)));
    }

    #[test]
    fn overlap_check_disjoint_ok() {
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "fn foo() {}\nfn bar() {}\n".to_string());
        let ops = vec![
            op("a.rs", "fn foo() {}", "x"),
            op("a.rs", "fn bar() {}", "y"),
        ];
        assert!(overlap_check(&ops, &originals).is_none());
    }

    #[test]
    fn overlap_check_overlapping_aborts() {
        let mut originals = BTreeMap::new();
        originals.insert(
            "a.rs".to_string(),
            "fn main() { hello; world; }\n".to_string(),
        );
        let ops = vec![
            op("a.rs", "hello; world", "hi"),
            op("a.rs", "world", "universe"),
        ];
        let reason = overlap_check(&ops, &originals).expect("should abort");
        assert!(matches!(reason, AbortReason::Overlap(_)));
    }

    #[test]
    fn merge_per_file_unique_applies() {
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "fn one() {}\n".to_string());
        let ops = vec![op("a.rs", "fn one() {}", "fn one(x: i32) {}")];
        let finals = merge_per_file(&ops, &originals).unwrap();
        assert!(finals.get("a.rs").unwrap().contains("i32"));
    }

    #[test]
    fn merge_per_file_not_found_aborts() {
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "fn one() {}\n".to_string());
        let ops = vec![op("a.rs", "absent", "y")];
        let reason = merge_per_file(&ops, &originals).unwrap_err();
        assert!(matches!(reason, AbortReason::Locate(_)));
    }

    #[test]
    fn merge_per_file_ambiguous_aborts() {
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "dup\ndup\n".to_string());
        let ops = vec![op("a.rs", "dup", "x")];
        let reason = merge_per_file(&ops, &originals).unwrap_err();
        assert!(matches!(reason, AbortReason::Locate(_)));
    }

    #[test]
    fn merge_per_file_chained_edits_apply_sequentially() {
        // edit #2's old_text only exists after edit #1 runs.
        let mut originals = BTreeMap::new();
        originals.insert("a.txt".to_string(), "alpha\n".to_string());
        let ops = vec![
            op("a.txt", "alpha", "alpha\ngamma"),
            op("a.txt", "gamma", "delta"),
        ];
        let finals = merge_per_file(&ops, &originals).unwrap();
        let merged = finals.get("a.txt").unwrap();
        assert!(merged.contains("delta"));
        assert!(merged.contains("alpha"));
    }

    #[test]
    fn lint_all_valid_content_ok() {
        let mut finals = BTreeMap::new();
        finals.insert("a.rs".to_string(), "fn main() {}\n".to_string());
        let ops = vec![EditOperation {
            file_path: "a.rs".to_string(),
            full_path: PathBuf::from("a.rs"),
            old_text: "x".to_string(),
            new_text: "y".to_string(),
        }];
        assert!(lint_all(&ops, &finals).is_none());
    }

    #[test]
    fn lint_all_invalid_content_aborts() {
        let mut finals = BTreeMap::new();
        finals.insert("a.rs".to_string(), "fn main() { let x = ; }\n".to_string());
        let ops = vec![EditOperation {
            file_path: "a.rs".to_string(),
            full_path: PathBuf::from("a.rs"),
            old_text: "x".to_string(),
            new_text: "y".to_string(),
        }];
        let reason = lint_all(&ops, &finals).expect("should abort");
        assert!(matches!(reason, AbortReason::Lint(_)));
        assert!(
            reason
                .into_output()
                .text_content()
                .contains("No files were modified")
        );
    }

    #[tokio::test]
    async fn atomic_abort_on_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("missing.rs", "x", "y"),
            ]
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::FileNotFound(_)), "{err:?}");
        // File #1 untouched.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
    }

    #[tokio::test]
    async fn atomic_abort_on_old_text_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&f2, "fn two() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "absent", "y"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        // Neither file written.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "fn two() {}\n");
    }

    #[tokio::test]
    async fn atomic_abort_on_ambiguous_old_text() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&f2, "dup\ndup\ndup\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "dup", "x"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(out.text_content().contains('3'), "{}", out.text_content());
        // Neither file written.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "dup\ndup\ndup\n");
    }

    #[tokio::test]
    async fn atomic_abort_on_linter_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&f2, "fn two() { let x = 1; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "let x = 1;", "let x = ;"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("No files were modified"),
            "{}",
            out.text_content()
        );
        // File #1 untouched.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
    }

    #[tokio::test]
    async fn atomic_abort_on_overlapping_edits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn main() { hello; world; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "hello; world", "hi"),
                edit("a.rs", "world", "universe"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("overlap"),
            "{}",
            out.text_content()
        );
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "fn main() { hello; world; }\n"
        );
    }

    #[tokio::test]
    async fn atomic_abort_on_duplicate_resolved_path() {
        // Two path aliases resolving to the same physical file would silently
        // clobber one edit-set; the batch must refuse instead.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn one() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let abs = tmp.path().join("a.rs");
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit(abs.to_str().unwrap(), "fn one() {}", "fn one(y: u32) {}"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("same file"),
            "{}",
            out.text_content()
        );
        // Nothing written.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "fn one() {}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_abort_on_symlink_target() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let real = tmp.path().join("real.rs");
        let link = tmp.path().join("link.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&real, "fn real() {}\n").unwrap();
        symlink(&real, &link).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("link.rs", "fn real() {}", "fn real(x: i32) {}"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("symbolic link"),
            "{}",
            out.text_content()
        );
        // File #1 untouched — proves the pre-check fired before any write.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
    }

    #[tokio::test]
    async fn happy_path_multi_file_one_edit_each() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&f2, "fn two() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "fn two() {}", "fn two(x: i32) {}"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("Applied: 2 file(s)"),
            "{}",
            out.text_content()
        );
        assert!(std::fs::read_to_string(&f1).unwrap().contains("i32"));
        assert!(std::fs::read_to_string(&f2).unwrap().contains("i32"));
    }

    #[tokio::test]
    async fn happy_path_multiple_edits_one_file_sequential() {
        let tmp = tempfile::TempDir::new().unwrap();
        // .txt — the test is about sequential application, not linting.
        let f = tmp.path().join("a.txt");
        // edit #2's old_text only appears after edit #1 runs.
        std::fs::write(&f, "alpha\nbeta\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.txt", "alpha", "alpha\ngamma"),
                edit("a.txt", "gamma", "delta"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let written = std::fs::read_to_string(&f).unwrap();
        assert!(written.contains("delta"), "{written}");
        assert!(written.contains("alpha"), "{written}");
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        let orig1 = "fn one() {}\n";
        let orig2 = "fn two() {}\n";
        std::fs::write(&f1, orig1).unwrap();
        std::fs::write(&f2, orig2).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "fn two() {}", "fn two(x: i32) {}"),
            ],
            "dry_run": true
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("Dry Run Preview"),
            "{}",
            out.text_content()
        );
        // Nothing written.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), orig1);
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), orig2);
    }

    #[tokio::test]
    async fn skip_linter_bypasses_phase_2() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn one() { let x = 1; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [ edit("a.rs", "let x = 1;", "let x = ;") ],
            "skip_linter": true
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "fn one() { let x = ; }\n"
        );
    }

    #[tokio::test]
    async fn max_edits_enforced() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "x\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let too_many: Vec<Value> = (0..=MAX_EDITS).map(|_| edit("a.rs", "x", "y")).collect();
        let input = json!({ "edits": too_many });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn empty_edits_array_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let err = tool.call(json!({ "edits": [] }), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn empty_old_text_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "content\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({ "edits": [ edit("a.rs", "", "y") ] });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("empty")),
            "{err:?}"
        );
        // No read happened.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "content\n");
    }

    #[tokio::test]
    async fn url_file_path_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [ edit("https://example.com/x", "a", "b") ]
        });
        let err = tool.call(input, &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("WebFetch")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn relative_path_resolved_against_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("src");
        std::fs::create_dir(&nested).unwrap();
        let f = nested.join("lib.rs");
        std::fs::write(&f, "fn old() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [ edit("src/lib.rs", "fn old() {}", "fn new() {}") ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        assert!(std::fs::read_to_string(&f).unwrap().contains("new()"));
    }

    #[test]
    fn trait_contract_and_registry() {
        let tool = MultiEditTool;
        assert!(!tool.is_read_only());
        assert!(!tool.is_concurrency_safe());
        let reg = crate::registry::builtin_registry();
        assert!(reg.get("MultiEdit").is_some(), "MultiEdit registered");
    }

    // ---- build_preview (pure) ----

    #[test]
    fn build_preview_dry_run_header_and_footer() {
        let ops = vec![op("a.rs", "fn one() {}", "fn one(x: i32) {}")];
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "fn one() {}\n".to_string());
        let mut finals = BTreeMap::new();
        finals.insert("a.rs".to_string(), "fn one(x: i32) {}\n".to_string());
        let out = build_preview(&ops, &originals, &finals, true);
        assert!(
            out.starts_with("Dry Run Preview — No files will be modified"),
            "{out}"
        );
        assert!(
            out.contains("Use dry_run=false to apply these changes."),
            "{out}"
        );
    }

    #[test]
    fn build_preview_apply_header() {
        let ops = vec![op("a.rs", "x", "y")];
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "x\n".to_string());
        let mut finals = BTreeMap::new();
        finals.insert("a.rs".to_string(), "y\n".to_string());
        let out = build_preview(&ops, &originals, &finals, false);
        assert!(out.starts_with("Multi-File Edit Summary"), "{out}");
        assert!(!out.contains("dry_run=false"));
    }

    #[test]
    fn build_preview_lists_each_edit_with_file_index() {
        let ops = vec![op("a.rs", "foo", "bar"), op("b.rs", "baz", "qux")];
        let mut originals = BTreeMap::new();
        originals.insert("a.rs".to_string(), "foo\n".to_string());
        originals.insert("b.rs".to_string(), "baz\n".to_string());
        let mut finals = BTreeMap::new();
        finals.insert("a.rs".to_string(), "bar\n".to_string());
        finals.insert("b.rs".to_string(), "qux\n".to_string());
        let out = build_preview(&ops, &originals, &finals, false);
        assert!(out.contains("File 1: a.rs"), "{out}");
        assert!(out.contains("File 2: b.rs"), "{out}");
        // Each file's diff is indented under its header.
        assert!(out.contains("  Changed: a.rs"), "{out}");
        assert!(out.contains("  Changed: b.rs"), "{out}");
    }

    // ---- apply_summary (pure) ----

    #[test]
    fn apply_summary_counts_applied_files() {
        let preview = "Multi-File Edit Summary\n";
        let ops = vec![op("a.rs", "x", "y"), op("b.rs", "z", "w")];
        let out = apply_summary(preview, &["a.rs", "b.rs"], &ops);
        assert!(out.contains("Applied: 2 file(s)"), "{out}");
        assert!(out.contains("  + a.rs"), "{out}");
        assert!(out.contains("  + b.rs"), "{out}");
    }

    #[test]
    fn apply_summary_shows_edit_count_when_multiple() {
        let preview = "Multi-File Edit Summary\n";
        // Two edits to a.rs, one to b.rs.
        let ops = vec![
            op("a.rs", "x", "y"),
            op("a.rs", "z", "w"),
            op("b.rs", "q", "r"),
        ];
        let out = apply_summary(preview, &["a.rs", "b.rs"], &ops);
        assert!(out.contains("  + a.rs (2 edits)"), "{out}");
        // Single-edit file shows no count suffix.
        assert!(out.contains("  + b.rs\n"), "{out}");
        assert!(!out.contains("b.rs ("), "{out}");
    }

    #[test]
    fn apply_summary_preserves_preview_prefix() {
        let preview = "header line\nbody\n";
        let out = apply_summary(preview, &[], &[]);
        assert!(out.starts_with("header line\nbody\n"), "{out}");
        assert!(out.contains("━━━"), "{out}");
    }

    // ---- multi_edit_inner (async, output-shape assertions) ----

    #[tokio::test]
    async fn multi_edit_inner_dry_run_returns_preview_not_summary() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn one() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [ edit("a.rs", "fn one() {}", "fn one(x: i32) {}") ],
            "dry_run": true
        });
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Dry Run Preview"), "{text}");
        assert!(!text.contains("Applied:"), "{text}");
        assert!(!text.contains("━━━"), "{text}");
    }

    #[tokio::test]
    async fn multi_edit_inner_apply_returns_summary_with_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&f2, "fn two() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "fn two() {}", "fn two(x: i32) {}"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Multi-File Edit Summary"), "{text}");
        assert!(text.contains("Applied: 2 file(s)"), "{text}");
        assert!(text.contains("  + a.rs"), "{text}");
        assert!(text.contains("  + b.rs"), "{text}");
    }

    #[tokio::test]
    async fn multi_edit_inner_abort_message_names_failing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("a.rs", "absent_text", "whatever"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("Old text not found in file: a.rs"), "{text}");
        // Nothing written.
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
    }

    #[tokio::test]
    async fn multi_edit_inner_overlap_message_cites_both_edits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn main() { hello; world; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "hello; world", "hi"),
                edit("a.rs", "world", "universe"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("edit #1"), "{text}");
        assert!(text.contains("edit #2"), "{text}");
        assert!(text.contains("overlap"), "{text}");
    }

    #[tokio::test]
    async fn multi_edit_inner_linter_abort_says_no_files_modified() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f1 = tmp.path().join("a.rs");
        let f2 = tmp.path().join("b.rs");
        std::fs::write(&f1, "fn one() {}\n").unwrap();
        std::fs::write(&f2, "fn two() { let x = 1; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "fn one() {}", "fn one(x: i32) {}"),
                edit("b.rs", "let x = 1;", "let x = ;"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Syntax validation failed"), "{text}");
        assert!(text.contains("No files were modified"), "{text}");
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "fn one() {}\n");
    }

    #[tokio::test]
    async fn dry_run_catches_overlap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn main() { hello; world; }\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "hello; world", "hi"),
                edit("a.rs", "world", "universe"),
            ],
            "dry_run": true
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("overlap"),
            "{}",
            out.text_content()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_parent_directory_rejected() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let realdir = tmp.path().join("realdir");
        std::fs::create_dir(&realdir).unwrap();
        let target = realdir.join("a.rs");
        std::fs::write(&target, "fn one() {}\n").unwrap();
        let linkdir = tmp.path().join("linkdir");
        symlink(&realdir, &linkdir).unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [ edit("linkdir/a.rs", "fn one() {}", "fn one(x: i32) {}") ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("symbolic link"),
            "{}",
            out.text_content()
        );
        // Target untouched.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn one() {}\n");
    }

    #[test]
    fn build_preview_shows_cumulative_merged_content() {
        // Two edits to one file — the preview must diff original vs the
        // fully merged final, not vs each edit's fragment in isolation.
        let ops = vec![
            op("a.txt", "alpha", "alpha\ngamma"),
            op("a.txt", "gamma", "delta"),
        ];
        let mut originals = BTreeMap::new();
        originals.insert("a.txt".to_string(), "alpha\n".to_string());
        let mut finals = BTreeMap::new();
        finals.insert("a.txt".to_string(), "alpha\ndelta\n".to_string());
        let out = build_preview(&ops, &originals, &finals, false);
        // The file appears once (distinct), and the diff reflects the merged
        // result containing "delta", not the intermediate "gamma".
        assert!(out.contains("delta"), "{out}");
        // Exactly one File header for a.txt (not two).
        assert_eq!(out.matches("File 1: a.txt").count(), 1, "{out}");
    }

    #[test]
    fn normalize_path_collapses_dot_and_dotdot() {
        assert_eq!(normalize_path(Path::new("./a.rs")), PathBuf::from("a.rs"));
        assert_eq!(
            normalize_path(Path::new("src/../a.rs")),
            PathBuf::from("a.rs")
        );
        assert_eq!(
            normalize_path(Path::new("/work/./b/../a.rs")),
            PathBuf::from("/work/a.rs")
        );
    }

    #[tokio::test]
    async fn dup_path_check_catches_dot_alias() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "content\n").unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let tool = MultiEditTool;
        let ctx = ctx_in(cwd);
        let input = json!({
            "edits": [
                edit("a.rs", "content", "x"),
                edit("./a.rs", "content", "y"),
            ]
        });
        let out = tool.call(input, &ctx).await.unwrap();
        assert!(out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("same file"),
            "{}",
            out.text_content()
        );
        // Nothing written.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "content\n");
    }
}
