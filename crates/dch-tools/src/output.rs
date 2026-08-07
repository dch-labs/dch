//! Large-output shaping for search-style tools.
//!
//! When a tool's result is small enough (at or under [`MAX_INLINE_OUTPUT_BYTES`])
//! it is returned inline as plain text. When it exceeds the limit, the result
//! is written to a fresh temp file and the tool returns a short message naming
//! the path plus a preview and a pointer telling the caller how to read the
//! full content. This keeps a single oversized search from blowing out a
//! model's context window.
//!
//! Spilled files land in a per-session subdir (see [`session_temp_dir`]) so one
//! session's results don't intermingle with another's. The subdir is created on
//! first spill; cleanup on session shutdown is the runner's responsibility (the
//! `Loop` trait has no shutdown hook today — see the dch `SESSION-MODEL-AUDIT`
//! and loopctl Change 13).
//!
//! Any failure along the spill path (the temp dir cannot be created, the file
//! cannot be written) degrades gracefully to inline truncation: the caller
//! always gets a usable result, never an error from this module.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use loopctl::tool::ToolOutput;

/// Default inline-output limit (50 `KiB`); results over this spill to a temp file.
pub const MAX_INLINE_OUTPUT_BYTES: usize = 50 * 1024;

/// Preview size when output spills/truncates (~10 `KiB`), sliced on a char boundary.
const PREVIEW_BYTES: usize = 10 * 1024;

/// Resolve a per-session temp directory under `base_temp_dir`.
///
/// Returns `{base_temp_dir}/dch-spill-{session_id}`. Each agent session gets
/// its own subdir so spilled search results stay isolated from other sessions
/// — giving a future runner-shutdown cleanup (or a manual `rm -rf`) one clear
/// target per session instead of a flat pile of files in the OS temp dir.
/// `truncate_or_write_to_temp` creates the subdir on first spill via
/// `create_dir_all`; this function only computes the path.
///
/// `session_id` is generic over [`Display`](std::fmt::Display) so the helper
/// does not depend on a specific id type (loopctl's `Uuid` formats straight in).
#[must_use]
pub fn session_temp_dir(base_temp_dir: &Path, session_id: impl std::fmt::Display) -> PathBuf {
    base_temp_dir.join(format!("dch-spill-{session_id}"))
}

/// Return `content` as a tool result, spilling to a temp file when oversized.
///
/// If `content.len()` is at or under `threshold`, it is returned inline. If it
/// exceeds `threshold`, it is written to a fresh file under `temp_dir` and the
/// returned text names the path, carries a `preview` of the first ~10 `KiB`,
/// and points the caller at the file-viewer tool for the full content. The
/// spilled file is persisted via `NamedTempFile::keep`, which releases the
/// open handle and marks the file non-deletable on drop, so the caller can
/// read the path later.
///
/// `threshold` is a parameter (not a const read inside) so tests can drive the
/// spill path with a tiny fixture instead of generating 50 `KiB` of content.
/// Production callers pass [`MAX_INLINE_OUTPUT_BYTES`].
///
/// Every failure mode (the temp dir cannot be created, the temp file cannot be
/// written, the disk fills) degrades to inline truncation with a note — the
/// function never returns an error and never panics.
#[must_use]
pub fn truncate_or_write_to_temp(
    content: String,
    tool_name: &str,
    temp_dir: &Path,
    threshold: usize,
) -> ToolOutput {
    if content.len() <= threshold {
        return ToolOutput::text(content);
    }

    if let Err(e) = std::fs::create_dir_all(temp_dir) {
        tracing::warn!(target: "dch_tools::output", path = %temp_dir.display(), error = %e, "failed to create temp dir; falling back to inline truncation");
        return ToolOutput::text(truncate_inline(&content));
    }

    let mut named = match tempfile::NamedTempFile::new_in(temp_dir) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(target: "dch_tools::output", path = %temp_dir.display(), error = %e, "failed to create temp file; falling back to inline truncation");
            return ToolOutput::text(truncate_inline(&content));
        }
    };
    if let Err(e) = named.write_all(content.as_bytes()) {
        tracing::warn!(target: "dch_tools::output", error = %e, "failed to write temp file; falling back to inline truncation");
        return ToolOutput::text(truncate_inline(&content));
    }

    let path = match named.keep() {
        Ok((_file, path)) => path,
        Err(e) => {
            tracing::warn!(target: "dch_tools::output", error = %e, "failed to persist temp file; falling back to inline truncation");
            return ToolOutput::text(truncate_inline(&content));
        }
    };

    let total_lines = content.lines().count();
    let total_size = content.len();
    let preview = preview_slice(&content);

    ToolOutput::text(format!(
        "{tool_name} result too large: {total_size} bytes, {total_lines} lines.\n\
         Full output written to: {path}\n\n\
         Preview (first ~10KB):\n\
         {preview}\n\n\
         [Use FileViewer to read the full result: file_path=\"{path}\"]",
        path = path.display(),
    ))
}

/// Truncate `content` to a preview when temp-file writing is impossible.
///
/// The message is distinct from the spill-path message so a reader can tell
/// "the result was big and we spilled it" (preview names a temp file) apart
/// from "the result was big and we couldn't spill it" (preview is all you
/// get).
fn truncate_inline(content: &str) -> String {
    let total_lines = content.lines().count();
    let total_size = content.len();
    let preview = preview_slice(content);
    format!(
        "Result truncated: {total_size} bytes, {total_lines} lines.\n\n\
         Preview:\n\
         {preview}\n\n\
         [Result was truncated because output exceeded limit and temp file write failed]"
    )
}

/// Take the first `PREVIEW_BYTES` bytes of `content`, sliced on a character
/// boundary so the preview never ends mid-code-point.
fn preview_slice(content: &str) -> &str {
    let cutoff = PREVIEW_BYTES.min(content.len());
    let boundary = floor_char_boundary(content, cutoff);
    &content[..boundary]
}

/// Largest byte index `<= target` that lands on a UTF-8 character boundary.
///
/// Manual implementation of `str::floor_char_boundary`, which is stable only
/// from Rust 1.91 — our MSRV is earlier, so we walk back until the byte at
/// `target` is not a UTF-8 continuation byte (i.e. `(b & 0xC0) != 0x80`).
fn floor_char_boundary(s: &str, target: usize) -> usize {
    let mut i = target.min(s.len());
    while i > 0 {
        let byte = s.as_bytes().get(i).copied().unwrap_or(0);
        if (byte & 0xC0) != 0x80 {
            break;
        }
        i = i.saturating_sub(1);
    }
    i
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn session_temp_dir_joins_session_id() {
        let dir = session_temp_dir(Path::new("/tmp"), "abc-123");
        assert_eq!(dir, PathBuf::from("/tmp/dch-spill-abc-123"));
    }

    #[test]
    fn session_temp_dir_distinct_per_session() {
        let a = session_temp_dir(Path::new("/tmp"), "session-a");
        let b = session_temp_dir(Path::new("/tmp"), "session-b");
        assert_ne!(a, b);
    }

    #[test]
    fn inline_when_under_threshold() {
        let out = truncate_or_write_to_temp("hello".to_string(), "grep", Path::new("/tmp"), 64);
        assert!(!out.is_error);
        assert_eq!(out.text_content(), "hello");
    }

    #[test]
    fn inline_when_exactly_at_threshold() {
        let body = "a".repeat(64);
        let out = truncate_or_write_to_temp(body.clone(), "grep", Path::new("/tmp"), 64);
        assert_eq!(out.text_content(), body);
    }

    #[test]
    fn spills_when_over_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "match line\n".repeat(20); // 220 bytes
        let out = truncate_or_write_to_temp(body, "grep", tmp.path(), 64);
        let text = out.text_content();
        assert!(!out.is_error, "{text}");
        assert!(text.contains("result too large"), "{text}");
        assert!(text.contains("Full output written to:"), "{text}");
        assert!(text.contains("Use FileViewer"), "{text}");
        // The spilled file should exist on disk.
        let start = text
            .find("written to: ")
            .map(|i| i + "written to: ".len())
            .unwrap();
        let end = text[start..].find('\n').map(|e| start + e).unwrap();
        let path_str = text[start..end].trim();
        assert!(
            Path::new(path_str).is_file(),
            "spilled file should exist at {path_str}"
        );
    }

    #[test]
    fn write_failure_degrades_to_inline_truncation() {
        let body = "x".repeat(200);
        let out =
            truncate_or_write_to_temp(body, "grep", Path::new("/proc/dch_should_not_exist"), 64);
        let text = out.text_content();
        assert!(!out.is_error, "degraded output is still a success: {text}");
        assert!(text.contains("Result truncated"), "{text}");
        assert!(text.contains("temp file write failed"), "{text}");
    }

    #[test]
    fn preview_slice_respects_char_boundary() {
        let s = "abcdefghij";
        assert_eq!(preview_slice(s), "abcdefghij");
        let ascii_prefix = "a".repeat(PREVIEW_BYTES - 1);
        let content = format!("{ascii_prefix}ééé");
        assert!(content.len() > PREVIEW_BYTES);
        let preview = preview_slice(&content);
        assert_eq!(preview, ascii_prefix);
    }
}
