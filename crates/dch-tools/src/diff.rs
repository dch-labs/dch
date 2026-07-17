//! Plain-text diff renderer for file-write/edit success messages.

use std::fmt::Write;

/// Maximum product of old × new line counts for the LCS algorithm.
///
/// The LCS dynamic-programming table is O(m × n) in both time and memory.
/// Beyond this threshold, the table becomes too expensive to compute and a
/// bounded before/after preview is produced instead via
/// [`format_large_diff`].
///
/// At 1,000,000 the DP table is ~8 MB of `usize` cells and completes in
/// milliseconds. Files up to roughly 1,000 × 1,000 lines receive the full
/// line-by-line diff; larger files get the truncated preview governed by
/// [`LARGE_DIFF_PREVIEW_LINES`].
const MAX_LCS_PRODUCT: usize = 1_000_000;

/// Maximum lines to show per side in the large-file fallback preview.
///
/// When the old × new line-count product exceeds [`MAX_LCS_PRODUCT`], the full
/// LCS diff is skipped and a truncated before/after preview is produced
/// instead. This constant bounds how many lines of old content (prefixed
/// `- `) and new content (prefixed `+ `) appear in that preview. Lines beyond
/// the limit are summarized as `... N more lines`.
const LARGE_DIFF_PREVIEW_LINES: usize = 1000;

/// One line in an LCS diff, produced by [`compute_lcs_diff`].
///
/// Each variant carries the line's text content. The diff algorithm walks
/// old and new line lists and classifies each line as unchanged (present in
/// both), deleted (only in old), or inserted (only in new).
#[derive(Debug)]
enum LineDiff {
    /// A line present in both old and new content.
    Unchanged(String),
    /// A line only in old content — removed by the change.
    Deleted(String),
    /// A line only in new content — added by the change.
    Inserted(String),
}

/// Format a file change for the tool's success message.
///
/// The output differs by change type:
///
/// - **New file** (`old_content` is `None`): prints a `Created:` header
///   followed by the first 10 lines, each prefixed with `+ `. If the file
///   exceeds 10 lines, a `... +N more lines` summary is appended.
/// - **Edit** (`old_content` is `Some`): prints a `Changed:` header followed
///   by an LCS-based line diff with 3 lines of context around each change
///   region. Unchanged context lines are prefixed with two spaces; inserted
///   lines with `+ `; deleted lines with `- `.
///
/// The diff format is plain text (no ANSI color codes) so it renders cleanly
/// in both TUI and headless output.
///
/// # Examples
///
/// ```
/// use dch_tools::diff::format_file_change;
///
/// // New file.
/// let msg = format_file_change("notes.txt", None, "hello\nworld\n");
/// assert!(msg.contains("Created: notes.txt (new file)"));
/// assert!(msg.contains("+ hello"));
///
/// // Edit.
/// let msg = format_file_change("main.rs", Some("fn main() {}\n"), "fn main() { todo!() }\n");
/// assert!(msg.contains("Changed: main.rs (modified)"));
/// ```
#[must_use]
pub fn format_file_change(file_path: &str, old_content: Option<&str>, new_content: &str) -> String {
    if let Some(old) = old_content {
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new_content.lines().collect();

        if old_lines.len().saturating_mul(new_lines.len()) > MAX_LCS_PRODUCT {
            return format_large_diff(file_path, &old_lines, &new_lines);
        }

        let diff = compute_lcs_diff(&old_lines, &new_lines);
        let mut result = format!("Changed: {file_path} (modified)\n");
        if let Some(summary) = change_summary(&diff) {
            result.push_str(&summary);
        }
        if let Some(note) = eof_change_note(old, new_content) {
            result.push_str(&note);
        }
        result.push_str(&format_diff_with_context(&diff, 3));
        result
    } else {
        let lines: Vec<&str> = new_content.lines().collect();
        let preview_count = lines.len().min(10);
        let mut result = format!("Created: {file_path} (new file)\n");
        for line in lines.iter().take(preview_count) {
            writeln!(result, "+ {line}").ok();
        }
        if lines.len() > preview_count {
            let more = lines.len().saturating_sub(preview_count);
            writeln!(result, "... +{more} more lines").ok();
        }
        result
    }
}

/// Build a one-line `N lines removed, M added` summary for a line diff.
///
/// Returns `None` when the diff has no removed and no inserted lines (a no-op
/// edit), so an unchanged file renders header-only with no spurious summary.
/// Counts are pluralized: `1 line` vs `2 lines`.
fn change_summary(diff: &[LineDiff]) -> Option<String> {
    let removed = diff
        .iter()
        .filter(|d| matches!(d, LineDiff::Deleted(_)))
        .count();
    let added = diff
        .iter()
        .filter(|d| matches!(d, LineDiff::Inserted(_)))
        .count();
    if removed == 0 && added == 0 {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if removed > 0 {
        parts.push(format!("{removed} line{} removed", plural(removed)));
    }
    if added > 0 {
        parts.push(format!("{added} line{} added", plural(added)));
    }
    Some(format!("│ {}\n", parts.join(", ")))
}

/// Empty string for a count of one, `"s"` otherwise — for pluralization.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Describe a trailing-newline-at-EOF change, when that is the *only* change.
///
/// `str::lines()` drops a file's final `\n`, so two contents that differ only
/// in their trailing newline (e.g. `"a\n"` → `"a"`) produce identical line
/// vectors and an empty LCS diff. Without this note such a write would render
/// as a bare header with no indication anything changed.
///
/// Returns `None` when the line content actually differs (the LCS already
/// covers it) or when the trailing-newline state is unchanged.
fn eof_change_note(old: &str, new: &str) -> Option<String> {
    if old.lines().ne(new.lines()) {
        return None;
    }
    let had = old.ends_with('\n');
    let has = new.ends_with('\n');
    let note = match (had, has) {
        (true, false) => "No newline at end of file",
        (false, true) => "Newline added at end of file",
        _ => return None,
    };
    Some(format!("│ {note}\n"))
}

/// Format a diff for files too large for the LCS algorithm (product exceeds
/// [`MAX_LCS_PRODUCT`]).
///
/// Shows a truncated before/after preview instead of computing the full diff:
/// up to [`LARGE_DIFF_PREVIEW_LINES`] lines of old content (prefixed `- `) and
/// the same number of new content (prefixed `+ `), with `... N more lines`
/// summaries for each side.
fn format_large_diff(file_path: &str, old_lines: &[&str], new_lines: &[&str]) -> String {
    let mut result = format!("Changed: {file_path} (modified, large file)\n");
    let old_preview = old_lines.len().min(LARGE_DIFF_PREVIEW_LINES);

    result.push_str("Before:\n");

    for line in old_lines.iter().take(old_preview) {
        writeln!(result, "- {line}").ok();
    }

    if old_lines.len() > old_preview {
        let more = old_lines.len().saturating_sub(old_preview);
        writeln!(result, "... {more} more lines").ok();
    }

    let new_preview = new_lines.len().min(LARGE_DIFF_PREVIEW_LINES);
    result.push_str("After:\n");

    for line in new_lines.iter().take(new_preview) {
        writeln!(result, "+ {line}").ok();
    }

    if new_lines.len() > new_preview {
        let more = new_lines.len().saturating_sub(new_preview);
        writeln!(result, "... {more} more lines").ok();
    }

    result
}

/// Read a cell from the flat DP table at row `i`, column `j`.
///
/// The table is stored as a single `Vec<usize>` with row-major layout: cell
/// `(i, j)` lives at index `i * stride + j`, where `stride` is the number of
/// columns. Returns `0` if the computed index is out of bounds (which cannot
/// happen for indices derived from valid line counts, but the safe `.get()`
/// guards against arithmetic edge cases).
fn dp_get(dp: &[usize], stride: usize, i: usize, j: usize) -> usize {
    dp.get(i.saturating_mul(stride).saturating_add(j))
        .copied()
        .unwrap_or(0)
}

/// Write a cell in the flat DP table at row `i`, column `j`.
///
/// Counterpart to [`dp_get`]. If the computed index is out of bounds the
/// write is silently skipped — again, this cannot happen for indices derived
/// from valid line counts.
fn dp_set(dp: &mut [usize], stride: usize, i: usize, j: usize, val: usize) {
    if let Some(slot) = dp.get_mut(i.saturating_mul(stride).saturating_add(j)) {
        *slot = val;
    }
}

/// Compute an LCS-based diff between two line lists.
///
/// Uses the classic dynamic-programming longest-common-subsequence algorithm
/// to classify each line as unchanged, deleted, or inserted. The result is in
/// document order (top to bottom).
///
/// Empty inputs are fast-pathed: if `old_lines` is empty, every new line is
/// [`Inserted`](LineDiff::Inserted); if `new_lines` is empty, every old line
/// is [`Deleted`](LineDiff::Deleted).
///
/// The DP table is a flat `Vec<usize>` accessed through [`dp_get`] and
/// [`dp_set`] to keep the code clippy-safe without per-function `#[allow]`
/// attributes.
fn compute_lcs_diff(old_lines: &[&str], new_lines: &[&str]) -> Vec<LineDiff> {
    let m = old_lines.len();
    let n = new_lines.len();
    if m == 0 {
        return new_lines
            .iter()
            .map(|l| LineDiff::Inserted(l.to_string()))
            .collect();
    }
    if n == 0 {
        return old_lines
            .iter()
            .map(|l| LineDiff::Deleted(l.to_string()))
            .collect();
    }

    let stride = n.saturating_add(1);
    let dp = build_lcs_dp(old_lines, new_lines, stride);
    let mut result = backtrack_lcs(&dp, stride, old_lines, new_lines);
    result.reverse();
    result
}

/// Fill the Longest Common Subsequence dynamic-programming table.
///
/// Returns a flat `Vec<usize>` where cell `(i, j)` is at index
/// `i * stride + j`, holding the LCS length of `old_lines[..i]` and
/// `new_lines[..j]`.
fn build_lcs_dp(old_lines: &[&str], new_lines: &[&str], stride: usize) -> Vec<usize> {
    let dp_len = old_lines.len().saturating_add(1).saturating_mul(stride);
    let mut dp = vec![0usize; dp_len];
    for (i, old_line) in old_lines.iter().enumerate() {
        let next_i = i.saturating_add(1);
        for (j, new_line) in new_lines.iter().enumerate() {
            let next_j = j.saturating_add(1);
            let val = if old_line == new_line {
                dp_get(&dp, stride, i, j).saturating_add(1)
            } else {
                dp_get(&dp, stride, i, next_j).max(dp_get(&dp, stride, next_i, j))
            };
            dp_set(&mut dp, stride, next_i, next_j, val);
        }
    }
    dp
}

/// Walk the DP table backwards from `(m, n)` to reconstruct the diff.
///
/// Starting at the bottom-right cell, moves up and left one step at a time.
/// At each position `(i, j)`:
///
/// - If `old_lines[i-1] == new_lines[j-1]`, the line is unchanged: emit
///   [`Unchanged`](LineDiff::Unchanged) and move diagonally to `(i-1, j-1)`.
/// - Otherwise, compare the two neighbours — `dp[i][j-1]` (skip new line) vs
///   `dp[i-1][j]` (skip old line) — and follow the higher value:
///   - Skip a new line → emit [`Inserted`](LineDiff::Inserted), move left.
///   - Skip an old line → emit [`Deleted`](LineDiff::Deleted), move up.
///
/// The loop terminates when both `i` and `j` reach zero.
///
/// Because we walk from the end, lines are collected in reverse document
/// order; the caller must call `.reverse()` before use.
fn backtrack_lcs(
    dp: &[usize],
    stride: usize,
    old_lines: &[&str],
    new_lines: &[&str],
) -> Vec<LineDiff> {
    let mut result = Vec::new();
    let mut i = old_lines.len();
    let mut j = new_lines.len();

    while i > 0 || j > 0 {
        let old_line = i.checked_sub(1).and_then(|idx| old_lines.get(idx));
        let new_line = j.checked_sub(1).and_then(|idx| new_lines.get(idx));

        if let (Some(old), Some(new)) = (old_line, new_line)
            && old == new
        {
            result.push(LineDiff::Unchanged(old.to_string()));
            i = i.saturating_sub(1);
            j = j.saturating_sub(1);
            continue;
        }

        let is_inserted = is_new_line_inserted(dp, stride, i, j);
        if is_inserted {
            if let Some(line) = new_line {
                result.push(LineDiff::Inserted(line.to_string()));
            }
            j = j.saturating_sub(1);
        } else {
            if let Some(line) = old_line {
                result.push(LineDiff::Deleted(line.to_string()));
            }
            i = i.saturating_sub(1);
        }
    }
    result
}

/// Determine whether the new line at position `j-1` is an insertion (not part
/// of the common subsequence).
///
/// Returns `true` if the LCS value to the left `dp[i][j-1]` is >= the value
/// above `dp[i-1][j]`, meaning the new line has no match in old.
fn is_new_line_inserted(dp: &[usize], stride: usize, i: usize, j: usize) -> bool {
    if j == 0 {
        return false; // no new line to consider
    }
    if i == 0 {
        return true; // no old lines left — everything in new is inserted
    }
    let left = dp_get(dp, stride, i, j.saturating_sub(1));
    let up = dp_get(dp, stride, i.saturating_sub(1), j);
    left >= up
}

/// Format a diff with `context` lines of surrounding context around each
/// change region.
///
/// Walks the [`LineDiff`] sequence and renders it as plain text:
///
/// - Unchanged context lines are prefixed with two spaces (`  `).
/// - Inserted lines are prefixed with `+ `.
/// - Deleted lines are prefixed with `- `.
///
/// To keep the output readable for large changes, only `context` unchanged
/// lines are shown before and after each run of insertions/deletions. Runs of
/// unchanged lines longer than `context` are elided.
///
/// Called by [`format_file_change`] with `context = 3`.
fn format_diff_with_context(line_diff: &[LineDiff], context: usize) -> String {
    let mut result = String::new();
    let mut pending_context: Vec<String> = Vec::new();
    let mut in_change = false;
    let mut context_after = 0usize;

    for diff in line_diff {
        match diff {
            LineDiff::Unchanged(line) => {
                if in_change {
                    pending_context.push(line.clone());
                    context_after = context_after.saturating_add(1);
                    if context_after >= context {
                        for ctx_line in &pending_context {
                            writeln!(result, "  {ctx_line}").ok();
                        }
                        pending_context.clear();
                        in_change = false;
                        context_after = 0;
                    }
                } else {
                    pending_context.push(line.clone());
                    if pending_context.len() > context {
                        pending_context.remove(0);
                    }
                }
            }
            LineDiff::Deleted(line) => {
                for ctx_line in &pending_context {
                    writeln!(result, "  {ctx_line}").ok();
                }
                pending_context.clear();
                context_after = 0;
                writeln!(result, "- {line}").ok();
                in_change = true;
            }
            LineDiff::Inserted(line) => {
                for ctx_line in &pending_context {
                    writeln!(result, "  {ctx_line}").ok();
                }
                pending_context.clear();
                context_after = 0;
                writeln!(result, "+ {line}").ok();
                in_change = true;
            }
        }
    }
    for ctx_line in &pending_context {
        writeln!(result, "  {ctx_line}").ok();
    }
    result
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::format_collect)]
mod tests {
    use super::*;

    #[test]
    fn new_file_preview() {
        let content = "line 1\nline 2\nline 3\n";
        let result = format_file_change("test.rs", None, content);
        assert!(result.contains("Created: test.rs (new file)"));
        assert!(result.contains("+ line 1"));
        assert!(result.contains("+ line 2"));
        assert!(result.contains("+ line 3"));
    }

    #[test]
    fn new_file_truncates_long_preview() {
        let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let result = format_file_change("test.rs", None, &content);
        assert!(result.contains("... +10 more lines"));
        assert!(!result.contains("+ line 11"));
    }

    #[test]
    fn edit_shows_inserted_lines() {
        let old = "a\nb\nc\n";
        let new = "a\nb\nNEW\nc\n";
        let result = format_file_change("test.rs", Some(old), new);
        assert!(result.contains("Changed: test.rs (modified)"));
        assert!(result.contains("+ NEW"));
        assert!(result.contains("  a"));
        assert!(result.contains("  b"));
    }

    #[test]
    fn edit_shows_deleted_lines() {
        let old = "a\nOLD\nb\nc\n";
        let new = "a\nb\nc\n";
        let result = format_file_change("test.rs", Some(old), new);
        assert!(result.contains("- OLD"));
    }

    #[test]
    fn edit_context_limit() {
        let old: String = (1..=10)
            .map(|i| format!("keep {i}\n"))
            .chain(std::iter::once("OLD\n".to_string()))
            .chain((1..=10).map(|i| format!("tail {i}\n")))
            .collect();
        let new: String = (1..=10)
            .map(|i| format!("keep {i}\n"))
            .chain(std::iter::once("NEW\n".to_string()))
            .chain((1..=10).map(|i| format!("tail {i}\n")))
            .collect();
        let result = format_file_change("test.rs", Some(&old), &new);
        assert!(result.contains("  keep 10"));
        assert!(
            !result.contains("  keep 1\n"),
            "should not contain 'keep 1' but got: {result}"
        );
        assert!(result.contains("+ NEW"));
        assert!(result.contains("- OLD"));
    }

    #[test]
    fn context_shows_lines_around_change() {
        let diff = vec![
            LineDiff::Unchanged("ctx1".into()),
            LineDiff::Unchanged("ctx2".into()),
            LineDiff::Inserted("NEW".into()),
            LineDiff::Unchanged("ctx3".into()),
            LineDiff::Unchanged("ctx4".into()),
        ];
        let result = format_diff_with_context(&diff, 3);
        // Leading context (up to 3) + insertion + trailing context.
        assert!(result.contains("  ctx1"));
        assert!(result.contains("  ctx2"));
        assert!(result.contains("+ NEW"));
        assert!(result.contains("  ctx3"));
        assert!(result.contains("  ctx4"));
    }

    #[test]
    fn context_elides_distant_unchanged_lines() {
        let diff: Vec<LineDiff> = (1..=10)
            .map(|i| LineDiff::Unchanged(format!("far {i}")))
            .chain(std::iter::once(LineDiff::Deleted("OLD".into())))
            .collect();
        let result = format_diff_with_context(&diff, 3);
        // Only the last 3 context lines before the change should appear.
        assert!(result.contains("  far 10"));
        assert!(result.contains("  far 9"));
        assert!(result.contains("  far 8"));
        assert!(!result.contains("  far 7"));
        assert!(!result.contains("  far 1\n"));
        assert!(result.contains("- OLD"));
    }

    #[test]
    fn consecutive_changes_flush_context_between() {
        let diff = vec![
            LineDiff::Unchanged("ctx1".into()),
            LineDiff::Inserted("ADD1".into()),
            LineDiff::Inserted("ADD2".into()),
            LineDiff::Unchanged("gap".into()),
            LineDiff::Deleted("DEL".into()),
        ];
        let result = format_diff_with_context(&diff, 3);
        assert!(result.contains("+ ADD1"));
        assert!(result.contains("+ ADD2"));
        // The "gap" line is context after the first change and before the second.
        assert!(result.contains("  gap"));
        assert!(result.contains("- DEL"));
    }

    #[test]
    fn change_at_start_has_no_leading_context() {
        let diff = vec![
            LineDiff::Inserted("FIRST".into()),
            LineDiff::Unchanged("after".into()),
        ];
        let result = format_diff_with_context(&diff, 3);
        assert!(result.contains("+ FIRST"));
        assert!(result.contains("  after"));
        // No leading context lines before the insertion.
        let first_line = result.lines().next();
        assert_eq!(first_line, Some("+ FIRST"));
    }

    #[test]
    fn trailing_unchanged_lines_flushed_at_end() {
        let diff = vec![
            LineDiff::Deleted("OLD".into()),
            LineDiff::Unchanged("end1".into()),
            LineDiff::Unchanged("end2".into()),
        ];
        let result = format_diff_with_context(&diff, 3);
        assert!(result.contains("- OLD"));
        assert!(result.contains("  end1"));
        assert!(result.contains("  end2"));
    }

    #[test]
    fn empty_diff_produces_empty_output() {
        let diff: Vec<LineDiff> = vec![];
        let result = format_diff_with_context(&diff, 3);
        assert!(result.is_empty());
    }

    #[test]
    fn large_diff_uses_bounded_preview() {
        // Product = 1500 × 1500 = 2,250,000 > MAX_LCS_PRODUCT (1,000,000).
        let old: String = (1..=1500).map(|i| format!("old line {i}\n")).collect();
        let new: String = (1..=1500).map(|i| format!("new line {i}\n")).collect();
        let result = format_file_change("big.rs", Some(&old), &new);

        assert!(result.contains("large file"), "{}", result);
        assert!(result.contains("Before:"));
        assert!(result.contains("After:"));
        // LARGE_DIFF_PREVIEW_LINES (1000) shown, rest summarized.
        assert!(result.contains("... 500 more lines"));
        // Lines beyond the preview are not shown.
        assert!(!result.contains("- old line 1001"));
        assert!(!result.contains("+ new line 1001"));
    }

    #[test]
    fn small_diff_still_uses_lcs() {
        // Product = 100 × 100 = 10,000 < MAX_LCS_PRODUCT.
        let old: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let new: String = (1..=99)
            .map(|i| format!("line {i}\n"))
            .chain(std::iter::once("CHANGED\n".to_string()))
            .collect();
        let result = format_file_change("small.rs", Some(&old), &new);
        assert!(result.contains("Changed: small.rs (modified)"));
        assert!(!result.contains("large file"));
        assert!(result.contains("- line 100"));
        assert!(result.contains("+ CHANGED"));
    }

    #[test]
    fn summary_line_present_for_mixed_edit() {
        let old = "a\nb\nc\n";
        let new = "a\nX\nY\nc\n";
        let result = format_file_change("f.rs", Some(old), new);
        // 1 removed (b), 2 added (X, Y).
        assert!(result.contains("1 line removed"), "{result}");
        assert!(result.contains("2 lines added"), "{result}");
        assert!(result.contains("- b"));
        assert!(result.contains("+ X"));
    }

    #[test]
    fn summary_line_absent_for_noop_edit() {
        let content = "a\nb\nc\n";
        let result = format_file_change("f.rs", Some(content), content);
        // No summary line and no +/- lines for a no-op edit.
        assert!(result.starts_with("Changed: f.rs (modified)\n"));
        assert!(!result.contains("line removed"));
        assert!(!result.contains("line added"));
        assert!(!result.contains("\n+ "));
        assert!(!result.contains("\n- "));
    }

    #[test]
    fn summary_line_pure_deletion() {
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        let result = format_file_change("f.rs", Some(old), new);
        assert!(result.contains("1 line removed"), "{result}");
        assert!(!result.contains("added"));
    }

    #[test]
    fn summary_line_empty_old_content_render_without_panic() {
        let result = format_file_change("f.rs", Some(""), "x\n");
        assert!(result.contains("Changed: f.rs (modified)"));
        assert!(result.contains("1 line added"));
        assert!(result.contains("+ x"));
    }

    #[test]
    fn trailing_newline_removed_is_visible() {
        // "a\n" -> "a": line vectors are identical, so the LCS shows nothing.
        // The EOF note must surface the change.
        let result = format_file_change("f.txt", Some("a\n"), "a");
        assert!(result.contains("Changed: f.txt (modified)"));
        assert!(result.contains("No newline at end of file"), "{result}");
        // No spurious line +/- from the LCS.
        assert!(!result.contains("\n+ "));
        assert!(!result.contains("\n- "));
    }

    #[test]
    fn trailing_newline_added_is_visible() {
        let result = format_file_change("f.txt", Some("a"), "a\n");
        assert!(result.contains("Newline added at end of file"), "{result}");
    }

    #[test]
    fn unchanged_trailing_newline_emits_no_eof_note() {
        // "a\n" -> "a\n": genuinely identical, including the newline.
        let result = format_file_change("f.txt", Some("a\n"), "a\n");
        assert!(!result.contains("end of file"), "{result}");
    }

    #[test]
    fn real_line_change_does_not_trigger_eof_note() {
        // When lines actually differ, the LCS covers it; no EOF note.
        let result = format_file_change("f.txt", Some("a\n"), "b\n");
        assert!(result.contains("- a"));
        assert!(result.contains("+ b"));
        assert!(!result.contains("end of file"), "{result}");
    }
}
