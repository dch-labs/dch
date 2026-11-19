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
    /// One entry per visual line; every index structure below keys
    /// positions in this vector.
    pub(super) lines: Vec<Line<'static>>,

    /// Row spans registered under their tool-call id.
    ///
    /// Every row a foldable tool block produced maps to the call it
    /// belongs to, so a press on any of them resolves the toggle.
    pub(super) tool_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their thinking section's key.
    ///
    /// The completed thinking sections' fold rows, keyed the way the
    /// tool rows are for the same press resolution.
    pub(super) thinking_summary_lines: HashMap<usize, String>,

    /// Row spans registered under their compaction notice's key.
    ///
    /// A notice's rows map to its message key so a press anywhere on
    /// the notice resolves its own toggle.
    pub(super) compaction_summary_lines: HashMap<usize, String>,

    /// Each user turn's row span, with its mark row.
    ///
    /// `(tint_start, turn_end, mark_row)` triples: the span the
    /// caller tints as the turn's box, and the row the prompt mark
    /// draws on.
    pub(super) user_turn_ranges: Vec<(usize, usize, usize)>,

    /// The rows where submitted prompts' texts begin.
    ///
    /// One entry per user message the user actually sent — steered
    /// and unsent rows stay out — at the row the prompt's first text
    /// line lands on.
    pub(super) user_prompt_marks: Vec<usize>,

    /// The row spans of the conversation's inline todo blocks.
    ///
    /// Whole-block ranges, so the scroll can keep an inline plan
    /// render visible in full or not at all.
    pub(super) todo_block_ranges: Vec<std::ops::Range<usize>>,
}

/// The bake's own index metadata, in bake-absolute rows.
///
/// What [`BakeOutputs`] carries for one render pass, kept for the
/// baked prefix alone: the index structures the frame's merged view
/// products are rebuilt from every frame, each rebased by the bake's
/// line count before the painters or the click paths ever see them.
#[derive(Default)]
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
