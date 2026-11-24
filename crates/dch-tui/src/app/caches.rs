//! The caches the app keeps between frames.
//!
//! The bake pass's accumulators and the composer's wrap cache —
//! shapes whose lifetime spans frames rather than a single render.

use ratatui::text::Line;
use std::collections::HashMap;
use std::sync::Arc;

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
    /// TranscriptBake-absolute rows, the prefix-only half of the merged marks
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

/// The live lane's parked rows, as the frame last rendered them.
///
/// The four segments that render below the settled conversation —
/// the seam, the thinking rows, the compaction row, the running
/// tools — are rebuilt by the frame and parked here so the paths
/// that run between frames, a selection's copy and the selectable
/// window's measure among them, read exactly the document the pane
/// showed rather than a fresh rebuild the user never saw.
#[derive(Default)]
pub(super) struct LaneCaches {
    /// The live lane's seam row, as the frame last rendered it.
    ///
    /// The blank row between the settled conversation and the live
    /// lane belongs to the rendered document a selection walks, so
    /// the frame parks its copy here: the copy path runs between
    /// frames and must read the lane exactly as the frame the user
    /// is looking at drew it.
    pub(super) live_seam_lines: Vec<Line<'static>>,

    /// The live thinking block's rows, as the frame last rendered them.
    ///
    /// Parked beside the seam row for the same reason — the
    /// selection's line space covers the thinking rows, and a copy
    /// made between frames reads them from this cache rather than
    /// re-deriving the block from shared state.
    pub(super) live_thinking_cache: Vec<Line<'static>>,

    /// The running compaction row, as the frame last rendered it.
    ///
    /// One row while the pass holds and none otherwise; parked with
    /// the other lane segments so the selection's copy path sees the
    /// same document the pane showed.
    pub(super) live_compaction_cache: Vec<Line<'static>>,

    /// The running tool blocks' rows, as the frame last rendered them.
    ///
    /// Rebuilt every frame from the active-tool list, with any
    /// expanded call's input and output rows included; parked here so
    /// a selection covering a running block copies what the pane
    /// showed at the press, not a fresh rebuild.
    pub(super) live_tools_cache: Vec<Line<'static>>,
}

/// The settled conversation's cache and its incremental bake.
///
/// The cache is the bake plus the last message's fresh tail,
/// recomposed whenever a mutation or a width change lands; the bake
/// itself only grows, appending each message's rendered rows once a
/// newer message follows. The indexes the frame's merged view
/// carries — tool rows, thinking sections, compaction notices,
/// user-turn spans, prompt marks, todo blocks — are rebuilt from the
/// bake's metadata and the tail's on every frame that draws.
pub(super) struct TranscriptBake {
    /// Rendered lines of the settled conversation.
    ///
    /// The conversation changes only when a message graduates, a
    /// claimed prompt takes its row, a tool result drains, or an error
    /// lands — everything else a frame does to it is re-rendering
    /// identical markdown. The cache rebuilds on those mutations and
    /// on a width change; frames clone it, the same per-frame linear
    /// trait the streaming freeze cache has.
    pub(super) conversation_cache: Vec<Line<'static>>,

    /// The baked prefix of [`conversation_cache`], growing by append.
    ///
    /// [`conversation_cache`] is this bake plus the last message's
    /// fresh tail, recomposed per rebuild — the bake is the only
    /// long-lived buffer, so a graduation never re-renders the baked
    /// lines' sources, only composes them beside a fresh tail.
    pub(super) conversation_bake: Vec<Line<'static>>,

    /// The pane width the conversation cache was built for.
    ///
    /// Settled markdown re-flows with the pane; a width change
    /// invalidates the cache wholesale so the next frame rebuilds
    /// every block at the new wrap.
    pub(super) conversation_cache_width: u16,

    /// Monotonic count of settled-conversation mutations.
    ///
    /// Every message push and verbosity switch bumps it; the render
    /// cache detects staleness by falling behind, which keeps the
    /// comparison against the width key uniform.
    pub(super) conversation_generation: u64,

    /// The generation the conversation cache was built at.
    ///
    /// Paired with
    /// [`conversation_generation`](Self::conversation_generation):
    /// the cache is stale exactly when the live count has moved
    /// past this stamp, whatever caused the bump.
    pub(super) conversation_cache_generation: u64,

    /// How many leading messages the cache has baked, never to
    /// re-render.
    ///
    /// Messages are immutable once a newer one follows — only the
    /// last is ever patched — so the bake grows by appending, and a
    /// graduation re-renders only the messages after it. A width
    /// change or a truncation below it resets the bake whole.
    pub(super) conversation_baked_through: usize,

    /// The speaker of the last baked message, for the seam rule.
    ///
    /// The blank row between speaker changes is decided by the
    /// previous message's kind; the incremental bake needs the baked
    /// prefix's answer to render the next message's seam correctly.
    pub(super) conversation_baked_prev_user: Option<bool>,

    /// The bake's own index metadata, in bake-absolute rows.
    ///
    /// Grown only as messages bake and cleared with the bake itself;
    /// the per-frame view products the painters and click paths read
    /// are rebuilt from it and the rebased tail every frame.
    pub(super) conversation_bake_meta: BakeMeta,

    /// Conversation-cache line ranges spanned by user turns, as
    /// `(tint_start, tint_end, mark_row)`.
    ///
    /// The render paints each range as a full-width raised block on the
    /// theme's surface — the prompt box's own color — around the turn's
    /// text, its padding rows included, and overlays the chevron at the
    /// turn's first text row. Chrome beside and beneath the content,
    /// never spans inside it, so selections and copies walk only the
    /// text. Rebuilt together with the conversation cache: the bake's
    /// absolute ranges plus the tail's, rebased onto the cache's own
    /// rows, so a tail user turn is boxed the frame it lands.
    pub(super) user_turn_ranges: Vec<(usize, usize, usize)>,

    /// The cache rows where each submitted prompt's text begins.
    ///
    /// Alt+Up and Alt+Down walk these: every prompt the user actually
    /// sent — steered echoes and unsent drafts excluded, the same
    /// population the session listing's MSGS column counts — seated at
    /// the viewport's top by its first text row. Rebuilt with the
    /// conversation cache; conversation rows open the layout, so the
    /// marks are absolute row indexes into the whole transcript.
    pub(super) user_prompt_marks: Vec<usize>,

    /// Conversation lines that sit inside a thinking section, by key.
    ///
    /// Mirrors [`tool_summary_lines`](Self::tool_summary_lines): every
    /// line a click can toggle, mapped to the key it belongs to.
    pub(super) thinking_summary_lines: HashMap<usize, String>,

    /// Transcript row to compaction-notice key, rebuilt per layout.
    ///
    /// The press handler's map for the notice rows, mirroring the
    /// thinking map beside it.
    pub(super) compaction_summary_lines: HashMap<usize, String>,

    /// Conversation line index to call id, for tool block rows.
    ///
    /// The click targets: every line of each completed block that
    /// has retained detail — summary and expansion alike, so a
    /// click anywhere on an open block folds it. Rebuilt with the
    /// conversation cache.
    pub(super) tool_summary_lines: HashMap<usize, String>,

    /// The line ranges of the transcript's expanded todo blocks.
    ///
    /// Recorded while the conversation cache is assembled — one range
    /// per `TodoWrite` block whose checkbox rows are on screen — so the
    /// scrolled view can keep each block whole: a severed list next to
    /// the plan badge's standing fraction reads as a count the frame
    /// does not carry. Indexes are cache lines, rebuilt with the cache.
    pub(super) todo_block_ranges: Vec<std::ops::Range<usize>>,
}

impl Default for TranscriptBake {
    fn default() -> Self {
        Self {
            conversation_cache: Vec::new(),
            conversation_bake: Vec::new(),
            conversation_cache_width: 0,
            conversation_generation: 1,
            conversation_cache_generation: 0,
            conversation_baked_through: 0,
            conversation_baked_prev_user: None,
            conversation_bake_meta: BakeMeta::default(),
            user_turn_ranges: Vec::new(),
            user_prompt_marks: Vec::new(),
            thinking_summary_lines: HashMap::new(),
            compaction_summary_lines: HashMap::new(),
            tool_summary_lines: HashMap::new(),
            todo_block_ranges: Vec::new(),
        }
    }
}

impl TranscriptBake {
    /// Invalidate the bake whole, keeping the width.
    ///
    /// For the shapes that change how already-baked messages render —
    /// a verbosity switch re-shapes every tool body, an expansion
    /// click re-renders one block — the bake cannot stand: it froze
    /// the old shape. Rare by nature (a key press, a click), so the
    /// full re-render they cost is the price they always paid.
    pub(super) fn rebake_whole(&mut self) {
        self.conversation_bake.clear();
        self.conversation_bake_meta = BakeMeta::default();
        self.conversation_baked_through = 0;
        self.conversation_baked_prev_user = None;
    }
}

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;
    use crate::cells::{char_cells, str_width};

    use crate::message::{ContentBlock, TuiMessage};

    #[test]
    fn the_auto_expanded_todo_block_folds_when_the_conversation_moves_on() {
        // The plan's inline expansion is momentary: the update
        // expands so the reader sees where the list stands, and the
        // conversation's next step — a reply, another tool's result —
        // folds it back to its summary row. The standing panel carries
        // the list from there.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject
            .expansion
            .expanded_tools
            .insert("call_todo".to_string());
        subject.expansion.auto_expanded_todo = Some("call_todo".to_string());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "working on it".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        assert!(
            !subject.expansion.expanded_tools.contains("call_todo"),
            "the next graduation folds the auto-expanded block"
        );
        assert!(
            subject.expansion.auto_expanded_todo.is_none(),
            "and retires the fold's tracking"
        );
    }

    #[test]
    fn the_auto_expanded_todo_block_folds_when_the_next_tool_starts() {
        // A tool in flight is the conversation moving on even before
        // its result lands: the frame-start settle folds the block the
        // moment something is running.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject
            .expansion
            .expanded_tools
            .insert("call_todo".to_string());
        subject.expansion.auto_expanded_todo = Some("call_todo".to_string());
        subject.settle_todo_expansion();
        assert!(
            subject.expansion.expanded_tools.contains("call_todo"),
            "nothing running — the block still shows the plan"
        );
        subject
            .active_tools()
            .lock()
            .expect("the tools lock")
            .push(crate::message::ActiveTool {
                call_id: "call_next".to_string(),
                name: "Bash".to_string(),
                input_summary: String::new(),
                start: std::time::Instant::now(),
            });
        subject.settle_todo_expansion();
        assert!(
            !subject.expansion.expanded_tools.contains("call_todo"),
            "the next tool starting folds it"
        );
    }

    #[test]
    fn an_automatic_fold_on_push_rebakes_off_the_baked_rows() {
        // The sibling of the eviction pin, from the graduation side: a
        // plan's expansion is armed for folding, and the next message
        // folds it. By then the block sits mid-conversation, past the
        // tail the incremental bake re-renders, so the fold has to
        // retire the rows it baked while the flag stood — a collapsed
        // toggle must not keep painting an expanded block.
        let mut subject = app();
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -1,2 +1,2 @@",
            "  fn main() {",
            "-     old();",
            "+     new();",
            "  }",
        ]
        .join("\n");
        subject.retain_tool_detail("call_todo", "{\"k\":1}", &receipt);
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Tool {
                name: "TodoWrite".to_string(),
                call_id: "call_todo".to_string(),
                input_preview: "src/lib.rs".to_string(),
                output_preview: String::new(),
                retained_input: String::new(),
                success: true,
                elapsed_secs: 0.1,
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        subject.push_message(TuiMessage::User {
            text: "after the block".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        subject
            .expansion
            .expanded_tools
            .insert("call_todo".to_string());
        subject.expansion.auto_expanded_todo = Some("call_todo".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("first draw");
        let expanded = buffer_text(terminal.backend().buffer());
        assert!(
            expanded.contains("old();"),
            "the armed block draws its expanded detail: {expanded}"
        );
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "working on it".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        assert!(
            !subject.expansion.expanded_tools.contains("call_todo"),
            "the next message folds the auto-expanded block"
        );
        terminal
            .draw(|frame| subject.render(frame))
            .expect("redraw");
        let collapsed = buffer_text(terminal.backend().buffer());
        assert!(
            !collapsed.contains("old();"),
            "the fold rebakes — the baked detail rows do not outlive the \
             flag they rode: {collapsed}"
        );
        assert!(
            collapsed.contains("src/lib.rs"),
            "the block's own summary row stands: {collapsed}"
        );
    }

    #[test]
    fn a_manual_expansion_outlives_the_auto_fold() {
        // The reader's own expansions are theirs: only the one block
        // the auto-expand armed ever folds.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject
            .expansion
            .expanded_tools
            .insert("call_mine".to_string());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "next step".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        assert!(
            subject.expansion.expanded_tools.contains("call_mine"),
            "a manual expansion is never folded by the auto rule"
        );
    }

    #[test]
    fn a_graduation_re_renders_only_the_tail() {
        // The bake is the spinner's autonomy: messages are immutable
        // once a newer one follows, so a graduation re-renders only
        // the messages after the bake — a full-transcript re-render
        // cost seconds on markdown-heavy sessions and froze the frame,
        // spinner included, on every tool result that landed.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        for turn in 0..5 {
            subject.push_message(TuiMessage::User {
                text: format!("prompt {turn}"),
                timestamp: now,
                steered: false,
                unsent: false,
            });
            subject.push_message(TuiMessage::Assistant {
                blocks: vec![ContentBlock::Text {
                    text: format!("## reply {turn}\n\n```rust\nfn f{turn}() {{}}\n```"),
                }],
                timestamp: now,
                duration_ms: Some(900),
            });
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let baked = subject.bake.conversation_baked_through;
        let prefix: Vec<String> = subject
            .bake
            .conversation_cache
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(baked, 9, "all but the last message bakes");
        subject.push_message(TuiMessage::User {
            text: "next".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        });
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert_eq!(
            subject.bake.conversation_baked_through, 10,
            "the bake advances by appending — one message re-rendered"
        );
        let after: Vec<String> = subject
            .bake
            .conversation_cache
            .iter()
            .take(prefix.len())
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(
            prefix, after,
            "the baked prefix is byte-identical across the graduation"
        );
    }

    #[test]
    fn no_rendered_cell_carries_a_composition_input_or_a_split_measure() {
        // A streamed reply full of every cluster
        // class the screen cannot size — the joined family that
        // leaked beside its row, a selector cluster, a skin-tone
        // compound, a keycap, a flag pair, a tagged flag — renders
        // with nothing left to compose and no cell whose whole-string
        // width disagrees with its per-character sum. Those two
        // properties together are the no-leak contract: ratatui's
        // diff skip measures the whole string, a terminal advances by
        // its own measure, and no surviving grapheme lets the two
        // diverge.
        let vectors = concat!(
            "family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} rests; ",
            "check \u{2714}\u{fe0f} done; ",
            "tone \u{1f44d}\u{1f3fd} ships; ",
            "key 1\u{fe0f}\u{20e3} holds; ",
            "flag \u{1f1f3}\u{1f1f4} flies; ",
            "tagged \u{1f3f3}\u{fe0f}\u{200d}\u{26a7}\u{fe0f}\u{e0074}\u{e0067} too"
        );
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: vectors.to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: Some(10),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        for cell in terminal.backend().buffer().content.clone() {
            let symbol = cell.symbol();
            let carries_input = symbol
                .chars()
                .any(|ch| matches!(ch, '\u{fe0f}' | '\u{fe0e}' | '\u{200d}' | '\u{20e3}' | '\u{1f3fb}'..='\u{1f3ff}' | '\u{e0020}'..='\u{e007f}'));
            assert!(
                !carries_input,
                "the composition input {symbol:?} reached the screen"
            );
            assert_eq!(
                str_width(symbol),
                symbol.chars().map(char_cells).sum::<usize>(),
                "the cell {symbol:?} splits the whole and summed measures"
            );
        }
        let cached: String = subject
            .bake
            .conversation_cache
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.to_string())
            .collect();
        assert!(
            cached.contains('\u{200d}') && cached.contains('\u{fe0f}'),
            "the copy source keeps the family and the selector the author wrote"
        );
    }

    #[test]
    fn a_row_painted_wider_than_its_raw_score_keeps_its_tail() {
        // The family's raw cluster scores two cells, its stripped
        // render spends six, and a wrap that budgeted by the raw
        // score let the row paint past its column — the tail's
        // letters clipped off the pane. The wrap gates by the
        // post-policy width now, so the same text breaks onto the
        // next row and every letter lands.
        let text =
            "alpha beta \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}tail wording now".to_string();
        let mut subject = app();
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text { text }],
            timestamp: chrono::Utc::now(),
            duration_ms: Some(10),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let screen = buffer_text(terminal.backend().buffer());
        assert!(
            screen.contains("tail") && screen.contains("now"),
            "the row's tail survives the family's painted width: {screen:?}"
        );
    }

    #[test]
    fn the_keyboard_walk_bounds_by_the_painted_width() {
        // Left and Right clamp at the row's painted width — twelve
        // cells for a family row the raw clusters score at eight —
        // so the walk never runs past the letters on screen.
        let mut subject = app();
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "xx \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} yy".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: Some(10),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert_eq!(
            subject.line_width(0),
            Some(12),
            "the walk bounds by the twelve painted cells, not the eight raw ones"
        );
    }

    #[test]
    fn chrome_rows_paint_the_stripped_projection() {
        // The surfaces that embed outside text: the notice row (a
        // user-typed theme argument rides it), the place row (the
        // working directory), and the welcome screen's notice and
        // directory caption — every chrome row that embeds outside
        // text paints the display projection, so no composition input
        // reaches the screen outside the composer and the question
        // draft, the two documented raw surfaces. The first draw catches
        // the welcome's rows, the second the standing furniture once a
        // message retires the welcome.
        let mut subject = app();
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        subject.post_notice(format!("unknown theme 'x{family}'"));
        subject.host.working_dir = Some(format!("/home/dch/{family}-labs"));
        subject.host.git_branch = None;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        for cell in terminal.backend().buffer().content.clone() {
            let symbol = cell.symbol();
            let carries_input = symbol.chars().any(|ch| {
                matches!(
                    ch,
                    '\u{fe0f}' | '\u{fe0e}'
                        | '\u{200d}'
                        | '\u{20e3}'
                        | '\u{1f3fb}'..='\u{1f3ff}'
                        | '\u{e0020}'..='\u{e007f}'
                )
            });
            assert!(
                !carries_input,
                "the composition input {symbol:?} reached a welcome chrome row"
            );
        }
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: Some(10),
        });
        terminal.draw(|frame| subject.render(frame)).unwrap();
        for cell in terminal.backend().buffer().content.clone() {
            let symbol = cell.symbol();
            let carries_input = symbol.chars().any(|ch| {
                matches!(
                    ch,
                    '\u{fe0f}' | '\u{fe0e}'
                        | '\u{200d}'
                        | '\u{20e3}'
                        | '\u{1f3fb}'..='\u{1f3ff}'
                        | '\u{e0020}'..='\u{e007f}'
                )
            });
            assert!(
                !carries_input,
                "the composition input {symbol:?} reached a standing chrome row"
            );
        }
        let screen = buffer_text(terminal.backend().buffer());
        assert!(
            screen.contains('\u{1f468}'),
            "the notice's family paints stripped, not vanished: {screen:?}"
        );
    }

    #[test]
    fn the_incremental_bake_matches_a_full_rebuild() {
        // The optimization may not change what the reader sees: the
        // incrementally baked cache and a from-scratch rebuild of the
        // same conversation render identical lines.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        subject.push_message(TuiMessage::User {
            text: "do it".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        });
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![
                ContentBlock::Text {
                    text: "## done\n\n**bold** and `code`".to_string(),
                },
                ContentBlock::Tool {
                    name: "Read".to_string(),
                    call_id: "c1".to_string(),
                    input_preview: "a.rs".to_string(),
                    success: true,
                    elapsed_secs: 0.5,
                    output_preview: "ok".to_string(),
                    retained_input: String::new(),
                },
            ],
            timestamp: now,
            duration_ms: Some(1200),
        });
        subject.push_message(TuiMessage::User {
            text: "again".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let incremental = subject.bake.conversation_cache.clone();
        let incremental_meta = (
            subject.bake.user_turn_ranges.clone(),
            subject.bake.user_prompt_marks.clone(),
            subject.bake.tool_summary_lines.clone(),
            subject.bake.thinking_summary_lines.clone(),
            subject.bake.compaction_summary_lines.clone(),
            subject.bake.todo_block_ranges.clone(),
        );
        // Force a whole-transcript rebake through the invalidation the
        // re-shaping paths use — no baked prefix, no baked metadata —
        // then lie about the generation so the next draw rebuilds.
        subject.bake.rebake_whole();
        subject.bake.conversation_cache.clear();
        subject.bake.conversation_cache_generation =
            subject.bake.conversation_cache_generation.wrapping_sub(1);
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let full = subject.bake.conversation_cache.clone();
        assert_eq!(
            texts_of(&incremental),
            texts_of(&full),
            "the bake and the rebuild render identical lines"
        );
        let full_meta = (
            subject.bake.user_turn_ranges.clone(),
            subject.bake.user_prompt_marks.clone(),
            subject.bake.tool_summary_lines.clone(),
            subject.bake.thinking_summary_lines.clone(),
            subject.bake.compaction_summary_lines.clone(),
            subject.bake.todo_block_ranges.clone(),
        );
        assert_eq!(
            incremental_meta, full_meta,
            "the bake and the rebuild index the rows identically"
        );
    }

    #[test]
    fn a_truncation_below_the_bake_rebakes_whole() {
        // A correction cut removes messages the bake already rendered:
        // the bake cannot keep lines for messages that no longer
        // exist, so a truncation below it resets and rebakes.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        for turn in 0..4 {
            subject.push_message(TuiMessage::User {
                text: format!("p{turn}"),
                timestamp: now,
                steered: false,
                unsent: false,
            });
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert!(subject.bake.conversation_baked_through >= 3);
        subject.conversation.truncate(2);
        subject.bake.conversation_generation = subject.bake.conversation_generation.wrapping_add(1);
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert_eq!(
            subject.conversation.len(),
            2,
            "the conversation holds the truncated tail"
        );
        let texts: Vec<String> = texts_of(&subject.bake.conversation_cache);
        assert!(
            texts.iter().any(|text| text.contains("p0")),
            "the rebake renders the surviving messages from scratch"
        );
    }

    #[test]
    fn an_adopted_resume_replaces_the_warm_bake_whole() {
        // A live resume over a session that already rendered: the
        // bake's reset keys on width and length alone, so a
        // replacement that grows would splice the abandoned
        // session's rows under the adopted one's tail — the head
        // the resumed transcript opens with would never paint. The
        // adoption drops the bake and the index-keyed expansions
        // whole, so the next frame renders the resumed transcript
        // from its own head and a section the old conversation had
        // expanded stays closed in the new one.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        subject.push_message(TuiMessage::User {
            text: "the abandoned session's prompt".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        });
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![crate::message::ContentBlock::Text {
                text: "the abandoned session's reply".to_string(),
            }],
            timestamp: now,
            duration_ms: None,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        assert!(
            subject.bake.conversation_baked_through >= 1,
            "fixture premise: the abandoned session rendered a warm bake"
        );
        subject
            .expansion
            .expanded_thinking
            .insert("m1b0".to_string());
        subject.adopt_resumed_session(
            "resumed".to_string(),
            vec![
                TuiMessage::User {
                    text: "the resumed session's head".to_string(),
                    timestamp: now,
                    steered: false,
                    unsent: false,
                },
                TuiMessage::Assistant {
                    blocks: vec![crate::message::ContentBlock::Text {
                        text: "the resumed session's reply".to_string(),
                    }],
                    timestamp: now,
                    duration_ms: None,
                },
                TuiMessage::User {
                    text: "the resumed session's follow-up".to_string(),
                    timestamp: now,
                    steered: false,
                    unsent: false,
                },
            ],
            crate::queue::ResumedTokens {
                cumulative_input: 1,
                cumulative_output: 2,
                last_input: 3,
                context_tokens: 4,
            },
        );
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let texts = texts_of(&subject.bake.conversation_cache);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("the resumed session's head")),
            "the resumed transcript renders from its own head"
        );
        assert!(
            texts.iter().all(|text| !text.contains("abandoned")),
            "no row of the abandoned session survives the adoption: {texts:?}"
        );
        assert!(
            subject.expansion.expanded_thinking.is_empty(),
            "an expansion keyed by position dies with the conversation that named it"
        );
    }

    #[test]
    fn an_edit_s_diff_expands_only_on_live_arrival() {
        // The diff is the point of a live edit row: it renders as it
        // happens, without a click. Restored history keeps its
        // collapsed rows — a resumed marathon of past edits would
        // otherwise bury the prose under walls of bands — and neither
        // a read nor a non-diff output expands anywhere.
        let mut subject = app();
        let receipt = [
            "Changed: src/lib.rs (modified)",
            "│ 1 line removed, 1 added",
            "@@ -1,2 +1,2 @@",
            "  fn main() {",
            "-     old();",
            "+     new();",
            "  }",
        ]
        .join("\n");
        // The resume-seeding path: details recorded, nothing expanded.
        subject.retain_tool_detail("call_restored", "{\"k\":1}", &receipt);
        assert!(
            !subject.expansion.expanded_tools.contains("call_restored"),
            "restored history stays collapsed however diff-shaped"
        );
        // The live graduation path: the same receipt expands itself
        // once the app drains the observer's graduation queue.
        subject
            .state
            .graduations
            .lock()
            .expect("the graduation lock")
            .push(crate::observer::Graduation::Tool(
                crate::observer::ToolResultDisplay {
                    name: "Edit".to_string(),
                    call_id: "call_live".to_string(),
                    is_error: false,
                    duration: std::time::Duration::ZERO,
                    input_summary: "src/lib.rs".to_string(),
                    output_preview: receipt.clone(),
                    full_input: "{\"k\":1}".to_string(),
                    full_output: receipt,
                },
            ));
        subject.drain_shared_state();
        assert!(
            subject.expansion.expanded_tools.contains("call_live"),
            "a live parsed edit diff expands its own row"
        );
    }

    #[test]
    fn every_theme_switch_path_rebakes_without_a_screen_clear() {
        // A direct terminal: the switch with no reader to park defers
        // the palette — the pane seat lands without an exchange and is
        // pinned by its own test.
        let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::terminal::set_multiplexed_for_tests(Some(false));
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        for command in ["/theme nord", "/theme", "/set theme nord"] {
            // Pretend every rendered-line cache is fresh under the old
            // palette: the render guard current, the live lane warm. A
            // switch that only cleared the bake would leave the merged
            // cache serving the old colors and the live lane waiting
            // for its next delta — the generation bump and the stamp
            // reset are what make the very next frame re-render both.
            app.bake.conversation_cache_generation = app.bake.conversation_generation;
            app.freeze.live_stamp = 7;
            app.bake.conversation_baked_through = 3;
            app.pacing.full_repaint_due = None;
            let generation = app.bake.conversation_generation;
            command_output(&mut app, command);
            assert_eq!(
                app.bake.conversation_baked_through, 0,
                "{command} invalidates the conversation bake"
            );
            assert!(
                app.bake.conversation_generation > generation,
                "{command} pierces the render guard's generation"
            );
            assert_eq!(
                app.freeze.live_stamp, 0,
                "{command} forces the live lane to re-render"
            );
            assert!(
                app.pacing.full_repaint_due.is_none(),
                "{command} clears no screen — the diff repaints without a blank frame"
            );
            assert_eq!(
                app.theme.ui.foreground,
                ratatui::style::Color::Reset,
                "{command} defers the palette with no reader to park for the ask"
            );
        }
    }
}
