//! The caches the app keeps between frames.
//!
//! The bake pass's accumulators and the composer's wrap cache —
//! shapes whose lifetime spans frames rather than a single render.

use std::collections::HashMap;
use std::sync::Arc;

use ratatui::text::Line;

/// The accumulators one conversation render pass fills.
///
/// A render of a message range produces the styled rows themselves
/// and the five index structures the app's press resolution and
/// lane overlays read — one output, always filled together, so it
/// folds into a single struct the pass fills. The bake pass owns
/// the half the app keeps between frames; the tail pass starts
/// empty each time and merges its result over the baked half.
#[derive(Default)]
pub(super) struct BakeOutputs {
    /// The rendered rows, in conversation order.
    ///
    /// One entry per visual line; every index the metadata carries
    /// keys positions in this vector.
    pub(super) lines: Vec<Line<'static>>,

    /// The pass's index metadata, in pass-relative rows.
    ///
    /// The same shape the baked prefix keeps: a pass fills it beside
    /// its rows and the frame adopts or rebases it whole, so the
    /// field set lives once.
    pub(super) meta: BakeMeta,
}

/// The bake's own index metadata, in bake-absolute rows.
///
/// The shape [`BakeOutputs`] embeds for one render pass, kept for
/// the baked prefix alone: the index structures the frame's merged view
/// products are rebuilt from every frame, each rebased by the bake's
/// line count before the painters or the click paths ever see them.
#[derive(Default, Clone)]
pub(super) struct BakeMeta {
    /// Row spans registered under their tool-call id.
    ///
    /// Every row a foldable baked tool block produced maps to the
    /// call it belongs to, so a press on any of them resolves the
    /// toggle.
    pub(super) tool_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their thinking section's key.
    ///
    /// The baked thinking sections' fold rows, keyed the way the
    /// tool rows are for the same press resolution.
    pub(super) thinking_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their compaction notice's key.
    ///
    /// A baked notice's rows map to its message key so a press
    /// anywhere on the notice resolves its own toggle.
    pub(super) compaction_summary_lines: HashMap<usize, String>,

    /// Each baked user turn's row span, with its mark row.
    ///
    /// `(tint_start, turn_end, mark_row)` triples in bake-absolute
    /// rows, grown as turns bake and never holding the tail's
    /// relative rows.
    pub(super) user_turn_ranges: Vec<(usize, usize, usize)>,

    /// The rows where baked submitted prompts' texts begin.
    ///
    /// Bake-absolute rows, the prefix-only half of the merged marks
    /// the frame's products rebuild each render.
    pub(super) user_prompt_marks: Vec<usize>,

    /// The row spans of the baked conversation's inline todo blocks.
    ///
    /// Whole-block ranges in bake-absolute rows, for the scroll's
    /// whole-or-nothing todo-block rule.
    pub(super) todo_block_ranges: Vec<std::ops::Range<usize>>,
}

impl BakeMeta {
    /// Adopt a completed pass's metadata into the baked prefix's.
    ///
    /// The incremental bake's tail handshake: a pass's indexes land
    /// after the rows they name, so its maps extend the baked ones
    /// and its spans append.
    pub(super) fn adopt(&mut self, pass: BakeMeta) {
        self.tool_summary_lines.extend(pass.tool_summary_lines);
        self.thinking_summary_lines
            .extend(pass.thinking_summary_lines);
        self.compaction_summary_lines
            .extend(pass.compaction_summary_lines);
        self.user_turn_ranges.extend(pass.user_turn_ranges);
        self.user_prompt_marks.extend(pass.user_prompt_marks);
        self.todo_block_ranges.extend(pass.todo_block_ranges);
    }

    /// The frame's merged indexes: the baked prefix's plus the tail's,
    /// rebased past the prefix's rows.
    ///
    /// Every tail index is pass-relative; `tail_base` is the prefix's
    /// row count, and each entry shifts by it onto the merged line
    /// space the frame's products read.
    pub(super) fn merged_over(self, tail: BakeMeta, tail_base: usize) -> Self {
        let mut merged = self;
        for (index, id) in tail.tool_summary_lines {
            merged
                .tool_summary_lines
                .insert(index.saturating_add(tail_base), id);
        }
        for (index, id) in tail.thinking_summary_lines {
            merged
                .thinking_summary_lines
                .insert(index.saturating_add(tail_base), id);
        }
        for (index, id) in tail.compaction_summary_lines {
            merged
                .compaction_summary_lines
                .insert(index.saturating_add(tail_base), id);
        }
        merged
            .user_turn_ranges
            .extend(tail.user_turn_ranges.into_iter().map(|(start, end, mark)| {
                (
                    start.saturating_add(tail_base),
                    end.saturating_add(tail_base),
                    mark.saturating_add(tail_base),
                )
            }));
        merged.user_prompt_marks.extend(
            tail.user_prompt_marks
                .into_iter()
                .map(|mark| mark.saturating_add(tail_base)),
        );
        merged
            .todo_block_ranges
            .extend(tail.todo_block_ranges.into_iter().map(|range| {
                range.start.saturating_add(tail_base)..range.end.saturating_add(tail_base)
            }));
        merged
    }
}

/// The composer's cached wrap.
///
/// One buffer mutation at one width: the rows the buffer wraps to
/// and the caret's resolved cell among them, kept until the
/// editor's stamp or the pane width moves. The answers a frame's
/// sizing, painting, and status tag all need, computed once per
/// change instead of once per frame.
pub(super) struct InputWrapCache {
    /// The editor mutation stamp the wrap was computed at.
    ///
    /// The cache holds exactly while this matches the editor's
    /// current stamp; every accepted edit and caret move advances
    /// it, which is what tells an untouched frame to reuse the
    /// grid.
    pub(super) stamp: u64,

    /// The wrap width the grid was built for.
    ///
    /// A resize re-wraps rather than reuses — wrapped rows are
    /// width-shaped, and a pane of another width would clip rows
    /// baked for the old one.
    pub(super) width: u16,

    /// The wrapped rows, shared by `Arc`.
    ///
    /// The same grid `display_rows` would rebuild; a cache hit
    /// clones the reference, not the grid — an idle frame pays
    /// neither the wrap nor a copy of its tens of thousands of
    /// rows.
    pub(super) rows: Arc<Vec<String>>,

    /// The caret's cell on that grid.
    ///
    /// Resolved, the empty buffer's `(0, 0)` parking spot included,
    /// so a hit answers both questions the frame asks without
    /// touching the editor again.
    pub(super) caret: (u16, u16),
}
