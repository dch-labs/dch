//! The selection: the drag and keyboard walkers over painted
//! cells, the projection a copy reads, and the scroll-stick the
//! pane's tail obeys.
use ratatui::text::Line;

use super::SelectableShape;
use super::TuiApp;
use super::clipboard::NO_LINES;
use super::paint::conversation::visible_window;
use crate::cells::first_retained_offset;
use crate::cells::{display_width, painted_steps};
use crossterm::event::KeyCode;
use dch_config::DchConfig;
use ratatui::layout::Rect;
use std::sync::{Mutex, PoisonError};

impl TuiApp {
    /// The base the next selection layers onto, materializing it when no
    /// host installed one.
    ///
    /// A host that boot-selected before handing the config over installs
    /// the pre-selection base at wiring; a panel constructed straight
    /// from a file config — tests, embedded hosts — holds no selection
    /// yet, so the live config at the first verdict *is* the base and
    /// capturing it there is exact.
    pub(crate) fn base_for_selection(&mut self) -> DchConfig {
        self.config_writes
            .config_base
            .get_or_insert_with(|| self.config.clone())
            .clone()
    }

    /// One wheel notch: up pages into the conversation, down toward the
    /// newest line.
    ///
    /// The plain transcript and the question sheet take their
    /// scrolling from here, so a notch means the same distance and
    /// rearms auto-scroll on the same terms on either surface; the
    /// permission ask parks scrolling wholesale, and the plan
    /// section's wheel keeps its own one-entry step.
    pub(crate) fn scroll_the_wheel(&mut self, up: bool) -> bool {
        if up {
            self.keep_the_seen_layout();
            self.scroll.scroll_offset =
                self.scroll.scroll_offset.saturating_add(WHEEL_SCROLL_LINES);
            self.scroll.stick = Stick::Hold;
        } else {
            self.scroll.scroll_offset =
                self.scroll.scroll_offset.saturating_sub(WHEEL_SCROLL_LINES);
            self.scroll.rearm_if_near_bottom();
        }
        true
    }

    /// Scroll one step while a drag sits at the pane's top or bottom
    /// row, extending the selection head to the edge's line after
    /// the step.
    ///
    /// Editors scroll the document under a selection dragged past
    /// the viewport, in the drag's direction. A terminal reports a
    /// drag only while the pointer moves, so a pointer parked at the
    /// edge goes quiet — the drag arm runs this for an immediate
    /// step per movement and the periodic tick keeps running it once
    /// per beat while the push continues. The scroll stops at the
    /// document's first or last line whatever the drag state says,
    /// and the head clamps into the selectable document, so an edge
    /// push stops at the document's last content line, running
    /// tools included. The drag state itself ends on the release
    /// or, when that arrived outside the terminal's view, on the
    /// first buttonless motion — so a lost release cannot carry the
    /// scroll far. Returns whether the view or the head moved.
    pub(crate) fn drag_autoscroll(&mut self) -> bool {
        let Some((column, row)) = self.pointer.drag_position else {
            return false;
        };
        let Some(view) = self.pointer.last_view.as_ref().copied() else {
            return false;
        };
        let height = usize::from(view.area.height);
        if height == 0 || view.total == 0 {
            return false;
        }
        let selectable_last = view.selectable.saturating_sub(1);
        let col = usize::from(column.saturating_sub(view.area.x))
            .min(usize::from(view.area.width.saturating_sub(1)));
        if row <= view.area.y {
            let head = CellPos {
                line: view.skip.saturating_sub(1).min(selectable_last),
                col,
            };
            let scrolled = view.skip > 0;
            if scrolled {
                self.keep_the_seen_layout();
                self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_add(1);
                self.scroll.stick = Stick::Hold;
            }
            let extended = self.extend_selection_head(head);
            scrolled || extended
        } else if usize::from(row).saturating_add(1) >= usize::from(view.area.bottom()) {
            let last = view
                .skip
                .saturating_add(1)
                .min(view.total.saturating_sub(height))
                .saturating_add(height)
                .saturating_sub(1)
                .min(view.total.saturating_sub(1))
                .min(selectable_last);
            let head = CellPos { line: last, col };
            let scrolled = self.scroll.scroll_offset > 0;
            self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_sub(1);
            self.scroll.rearm_if_near_bottom();
            let extended = self.extend_selection_head(head);
            scrolled || extended
        } else {
            false
        }
    }

    /// Point the selection head at `head`, reporting whether it moved.
    ///
    /// Exactly one selection is in play during a drag; with none —
    /// the button already released, say — there is nothing to extend.
    fn extend_selection_head(&mut self, head: CellPos) -> bool {
        match self.pointer.selection.as_mut() {
            Some(selection) => {
                let moved = selection.head != head;
                selection.head = head;
                moved
            }
            None => false,
        }
    }

    /// Move the selection head one step for Shift and an arrow key.
    ///
    /// The head moves and the anchor stays, so stepping away from the
    /// anchor extends the selection and stepping back over it shrinks
    /// it — the same select-by-keyboard every editor ships. Left and
    /// Right step one cell and wrap at line ends; Up and Down step
    /// one rendered line and keep the column. The viewport follows
    /// the head, and the adjusted text replaces the clipboard copy
    /// silently, exactly as a release does.
    pub(crate) fn nudge_selection(&mut self, direction: KeyCode) {
        let Some(selection) = self.pointer.selection.as_ref() else {
            return;
        };
        let head = selection.head;
        let up_one = head.line.saturating_sub(1);
        let down_one = head.line.saturating_add(1);
        let nudged = match direction {
            KeyCode::Left => {
                if head.col > 0 {
                    CellPos {
                        line: head.line,
                        col: head.col.saturating_sub(1),
                    }
                } else if head.line > 0 {
                    let width = self.line_width(up_one).unwrap_or(0);
                    CellPos {
                        line: up_one,
                        col: width.saturating_sub(1),
                    }
                } else {
                    head
                }
            }
            KeyCode::Right => {
                let width = self.line_width(head.line).unwrap_or(0);
                if width > 0 && head.col.saturating_add(1) < width {
                    CellPos {
                        line: head.line,
                        col: head.col.saturating_add(1),
                    }
                } else if self.line_width(down_one).is_some() {
                    CellPos {
                        line: down_one,
                        col: 0,
                    }
                } else {
                    head
                }
            }
            KeyCode::Up => {
                if head.line > 0 {
                    CellPos {
                        line: up_one,
                        col: head.col,
                    }
                } else {
                    head
                }
            }
            KeyCode::Down => {
                if self.line_width(down_one).is_some() {
                    CellPos {
                        line: down_one,
                        col: head.col,
                    }
                } else {
                    head
                }
            }
            _ => head,
        };
        if nudged == head {
            return;
        }
        if let Some(selection) = self.pointer.selection.as_mut() {
            selection.head = nudged;
        }
        self.follow_head(nudged.line);
        let covered = self
            .pointer
            .selection
            .as_ref()
            .filter(|selection| selection.anchor != selection.head)
            .map(|selection| self.selection_text(selection));
        if let Some((text, covered_any)) = covered
            && covered_any
        {
            self.copy_selection_and_notice(&text);
        }
    }

    /// The rendered width, in cells, of one selectable conversation
    /// line.
    ///
    /// `None` past the end of the document the selection walks — the
    /// same line segments [`Self::selectable_window`] windows over,
    /// running tools included, with the pending-steer and run-timer
    /// rows below left out. Left and Right use the width as their
    /// wrap boundary and Down uses existence as its floor. The
    /// measure is the line's flattened projection — the same single
    /// policy walk the flip, the copy, and the screen's strip run —
    /// so the keyboard's cell space and the selection's are one
    /// space, seam clusters included.
    pub(crate) fn line_width(&self, index: usize) -> Option<usize> {
        self.selectable_window(index, 1)
            .into_iter()
            .next()
            .map(|line| {
                let mut flat = String::new();
                for span in &line.spans {
                    flat.push_str(span.content.as_ref());
                }
                display_width(&flat)
            })
    }

    /// A window over the lines a selection can cover.
    ///
    /// The whole rendered document through the running tools — the
    /// settled conversation, the seam and thinking rows, the frozen
    /// and live streaming segments, the compaction row, and the
    /// running tool blocks — shares one line space with the frame's
    /// render order, so what a copy walks, what the highlight spans,
    /// and what the pane shows are the same lines. The pending-steer
    /// and run-timer rows render below the document and stay out of
    /// it: a press on them clamps to the last line above.
    fn selectable_window(&self, skip: usize, height: usize) -> Vec<Line<'_>> {
        visible_window(
            &[
                &self.bake.conversation_cache,
                &self.lane.live_seam_lines,
                &self.lane.live_thinking_cache,
                &self.freeze.frozen_lines,
                &self.freeze.live_lines,
                &self.lane.live_compaction_cache,
                &self.lane.live_tools_cache,
                &NO_LINES,
                &NO_LINES,
            ],
            skip,
            height,
        )
    }

    /// Scroll the view just enough to keep `line` on screen.
    ///
    /// A keyboard-walked selection head moves through the document
    /// under its own power; the viewport follows so the moving end
    /// stays visible — stepping upward detaches the view, and
    /// stepping back within the stick tolerance re-arms it.
    fn follow_head(&mut self, line: usize) {
        let Some(view) = self.pointer.last_view.as_ref().copied() else {
            return;
        };
        let height = usize::from(view.area.height);
        if height == 0 {
            return;
        }
        if line < view.skip {
            self.keep_the_seen_layout();
            self.scroll.scroll_offset = self
                .scroll
                .scroll_offset
                .saturating_add(view.skip.saturating_sub(line));
            self.scroll.stick = Stick::Hold;
        } else if line >= view.skip.saturating_add(height) {
            let excess = line
                .saturating_sub(view.skip.saturating_add(height))
                .saturating_add(1);
            self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_sub(excess);
            self.scroll.rearm_if_near_bottom();
        }
    }

    /// Record the frame's lane shape, dropping a selection it
    /// stranded.
    ///
    /// The live lane rebuilds every frame, so its segment lengths
    /// moving — or the streaming region rewriting itself — can leave
    /// a selection naming lines that no longer sit where it was
    /// drawn. Only a selection reaching past the stable conversation
    /// prefix can be stranded: shallower ones keep their numbering,
    /// and the drag ends with the selection it was steering.
    pub(crate) fn settle_selectable_shape(
        &mut self,
        streaming_changed: bool,
        shape: SelectableShape,
    ) {
        let shape_before = self.pointer.selectable_shape;
        self.pointer.selectable_shape = shape;
        let stranded =
            (streaming_changed || shape != shape_before)
                && self.pointer.selection.as_ref().is_some_and(|selection| {
                    ordered(selection).1.line >= shape.0.min(shape_before.0)
                });
        if stranded {
            self.pointer.selection = None;
            self.pointer.drag_position = None;
        }
    }

    /// The characters the selection covers, as plain text, with
    /// whether any character was covered at all.
    ///
    /// Walks the same rendered lines the highlight walks, so what is
    /// copied is exactly what is shown selected. The flag is false
    /// only when no row covered a single character — a span over
    /// blank cells — so a caller can leave the clipboard alone even
    /// though the separators make the text itself non-empty; an
    /// interior blank line inside a genuine selection is content
    /// and does not clear the flag.
    pub(crate) fn selection_text(&self, selection: &CellSelection) -> (String, bool) {
        let (from, to) = ordered(selection);
        let lines = self.selectable_window(
            from.line,
            to.line.saturating_sub(from.line).saturating_add(1),
        );
        let mut text = String::new();
        let mut covered_any = false;
        for (offset, line) in lines.iter().enumerate() {
            let line_index = from.line.saturating_add(offset);
            let start = if line_index == from.line { from.col } else { 0 };
            let end = if line_index == to.line {
                to.col
            } else {
                usize::MAX
            };
            let flat: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            let covered = covered_chars(&flat, start, end);
            if !covered.is_empty() {
                covered_any = true;
            }
            text.push_str(&covered);
            if line_index < to.line {
                text.push('\n');
            }
        }
        (text, covered_any)
    }
}

/// Where the scroll pin sits in the transcript's own structure: the
/// message whose rows hold it, the rendering block within that message,
/// the paragraph unit within that block (a source segment, a paragraph of
/// a text, thinking, or detail block), and the row's offset inside the
/// unit beside the unit's row count when the identity was read.
///
/// A rewrap at a new width moves every unit's rows but keeps the blocks
/// and their units, so the settle maps the identity back onto the same
/// source line; a row index alone names a different line after every
/// rewrap. The block is named apart from the unit because a fold changes
/// a block's unit count: counted flat across the message, a folded
/// thinking block would take a paragraph from every block after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PinIdentity {
    /// The message whose rows hold the pin, as an index into the bake's
    /// message ranges.
    ///
    /// A rewrap keeps the messages, so the index names the same message
    /// on both sides of it.
    pub(crate) message: usize,

    /// The rendering block within that message holding the pin — the
    /// whole of a user, system, error, or compaction message, or one
    /// content block or the trailing duration row of an assistant
    /// message — as the message's block list numbers them.
    ///
    /// Blocks survive a rewrap and a fold of another block whole; only
    /// their rows and paragraph counts change.
    pub(crate) block: usize,

    /// The paragraph unit within that block holding the pin, as the
    /// block's unit starts list numbers them.
    ///
    /// Units survive a rewrap whole; only their row counts change, which
    /// is why the pin is carried at this grain and not as a row.
    pub(crate) unit: usize,

    /// The pin's row offset inside its unit when the identity was read.
    ///
    /// Scaled by the unit's new row count over its old one across a
    /// rewrap, so under uniform wrapping it lands on the same source
    /// line.
    pub(crate) offset: usize,

    /// The unit's row count when the identity was read.
    ///
    /// The denominator of that scaling; zero would divide nothing, and
    /// a pin is only ever read off a unit that has rows.
    pub(crate) unit_rows: usize,
}

/// The merged indexes of one layout: each message's row span and, per
/// message and rendering block, the row each of its paragraph units opens
/// at — the figures [`PinIdentity`] is read against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BakeLayout {
    /// Each message's row span in the merged cache of that layout.
    ///
    /// The first figure the pin's identity is read against: the message
    /// a pinned row sits in, found by the row's place in these spans.
    pub(crate) ranges: Vec<std::ops::Range<usize>>,

    /// Per message, per rendering block, the row each of the block's
    /// paragraph units opens at.
    ///
    /// The second figure: the block and the unit within it, and — beside
    /// the message's span — the row's offset inside the unit and the
    /// unit's row count.
    pub(crate) units: Vec<Vec<Vec<usize>>>,
}

/// One block's shape as it stood when a fold or expansion was noted,
/// ahead of the rebake that redraws the row maps.
///
/// The settle re-reads the block after the rebake and moves the pin by
/// the difference; every figure is in the last settle's coordinates,
/// the pin included, so the two measurements agree wherever the reader
/// has since moved the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReshapeNote {
    /// The bake key the block is registered under.
    ///
    /// What the settle re-reads the block by once the rebake has
    /// redrawn the row maps, and what makes a later click on the same
    /// block find its standing note.
    pub(crate) key: String,

    /// The message the noted occurrence sits in, when the key is a call
    /// id a retry gave two blocks: the note's rows are read within that
    /// message alone, so each occurrence moves the pin by its own change.
    ///
    /// `None` for a key that already names one block — a thinking
    /// section's, or a notice's.
    pub(crate) message: Option<usize>,

    /// The rows the block held above `pin` when the note was taken.
    ///
    /// The before of the row-by-row count a block the pin sat inside is
    /// measured by; the after is read at the settle against the same pin.
    pub(crate) rows_above: usize,

    /// The row the note's figures were counted against: the view's top
    /// row as the reader had it when the block changed.
    pub(crate) pin: usize,

    /// The block's row count when the note was taken.
    ///
    /// The before of the whole-length count a block wholly above the pin
    /// is measured by: every row it gained or lost lands above the pin.
    pub(crate) len: usize,

    /// The block's last row when the note was taken.
    ///
    /// Before `pin`, the block stood wholly above it and the whole-length
    /// rule applies; at or after it, the pin sat inside the block and the
    /// rows are counted one by one.
    pub(crate) last_row: usize,
}

/// The conversation view's scrolling posture: the offset above the
/// newest line and whether the view follows content or holds.
///
/// The two facts every scroll action reads and writes, in one place
/// — the offset the wheel and jumps move, and the stickiness that
/// decides whether growth below the viewport drags the view along.
/// [`Stick`] spells the two postures.
#[derive(Default)]
pub(crate) struct ScrollPosture {
    /// Lines scrolled up from the bottom of the view.
    ///
    /// Zero means pinned to the newest line; submitting resets it.
    pub(crate) scroll_offset: usize,

    /// The view's stickiness posture.
    ///
    /// Pinned views re-anchor to the bottom on every layout; any
    /// scroll-up detaches, and scrolling back within
    /// [`STICK_TOLERANCE`] lines of the bottom — or pressing End or
    /// submitting — re-arms.
    pub(crate) stick: Stick,

    /// Blocks that changed shape since the last settle while the view
    /// was detached, one [`ReshapeNote`] per block. Every fold and
    /// expansion site notes its block ahead of its `rebake_whole()`,
    /// and the first note for a block stands until the settle however
    /// many times it is clicked before a draw — every click reads the
    /// same painted map, so the settle must not apply one fold once per
    /// click. The next settle, running after the rebake has redrawn the
    /// row maps, re-reads each block against its note and spares the
    /// offset exactly the rows that moved above the pin, so a block
    /// folding or opening above the pin leaves the pinned row in place
    /// instead of dragging the view by the block's whole change.
    pub(crate) reshaped_blocks: Vec<ReshapeNote>,

    /// The row the detached view held at its top when the settle last
    /// ran, in that frame's coordinates; `None` while pinned. The settle
    /// re-derives the offset from it every frame, so a viewport that
    /// changes under the view — a pane tmux resized while it was hidden —
    /// and the clamp that follows cannot lose the reader's line: the pin
    /// is remembered even while the offset cannot reach it.
    pub(crate) anchored_row: Option<usize>,

    /// The offset the last settle left. The next settle reads any change
    /// to the offset against it — a wheel notch, a prompt jump, a drag's
    /// autoscroll — as the reader moving the pin, so every writer of the
    /// offset moves the pin through the same door.
    pub(crate) settled_offset: usize,

    /// The pin's place in the transcript's own structure, read off the bake
    /// whenever something other than a rewrap moves the pin. The settle
    /// maps it across a width change, where the pin's row index would
    /// name a different line.
    pub(crate) pin_identity: Option<PinIdentity>,

    /// The merged message ranges and unit starts the last settle's pin
    /// was read against — the previous layout, kept whole.
    ///
    /// A reader who queues a notch against that layout and then draws at
    /// a new width asks for a row of it; the settle reads the row's place
    /// here before mapping the place onto the rebaked rows, so the notch
    /// survives the rewrap instead of being replaced by the old pin.
    pub(crate) settled_layout: Option<Box<BakeLayout>>,

    /// The bake width the last settle saw, the following ones too: a
    /// click that detaches keeps the layout of the frame it was made in,
    /// and the next settle compares against that frame's width, never the
    /// last detached one's.
    ///
    /// A different width this frame means the bake rewrapped and moved
    /// every row, so the settle maps the pin's identity onto the rebaked
    /// rows rather than keep its row — a pin with no identity, the live
    /// region say, keeps its row all the same. A rewrap alone does not
    /// re-read the identity: the unit stays where it was, and keeping
    /// the origin width's offset inside it maps back to the very row on
    /// a round trip.
    pub(crate) settled_width: usize,
}

impl ScrollPosture {
    /// Jump the view back to the bottom and re-arm stickiness.
    ///
    /// Submits call this so freshly appended output is visible and
    /// followed; the End key shares the path.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.stick = Stick::Follow;
    }

    /// Jump the view to the oldest line and drop stickiness.
    ///
    /// The Home key calls this. The offset is saturated because the
    /// scrollable height is layout-time knowledge: the next render's
    /// settle clamps it to the document's top, and the dropped
    /// stickiness keeps growth below the viewport from dragging the
    /// view back down.
    pub(crate) fn scroll_to_top(&mut self) {
        self.scroll_offset = usize::MAX;
        self.stick = Stick::Hold;
    }

    /// Re-arm stickiness when a scroll lands near the bottom.
    ///
    /// Scroll-down actions within [`STICK_TOLERANCE`] lines of the
    /// newest content count as "back at the bottom"; the next layout
    /// snaps fully.
    pub(crate) fn rearm_if_near_bottom(&mut self) {
        if self.scroll_offset <= STICK_TOLERANCE {
            self.stick = Stick::Follow;
        }
    }
}

/// The pointer's state over the transcript: the selection it is
/// dragging, the frame geometry it maps through, and the drag's
/// parked position.
///
/// Mouse events arrive between frames carrying screen cells; the
/// [`last_view`](Self::last_view) geometry is the bridge that maps
/// them onto conversation lines, and the drag position is what the
/// periodic tick reads to keep an edge push scrolling after the
/// pointer goes quiet.
#[derive(Default)]
pub(crate) struct SelectionState {
    /// The mouse selection over the transcript, if any.
    ///
    /// Both ends are display cells — an index into the rendered
    /// document's lines, conversation and live lane alike, plus a
    /// column — so the selection is character-granular and holds
    /// still while the view scrolls. It survives the release — the
    /// copy happens then,
    /// the highlight stays up — until the next press replaces it; a
    /// press-release without travel clears it.
    pub(crate) selection: Option<CellSelection>,

    /// The previous frame's conversation viewport.
    ///
    /// Mouse events arrive between frames carrying screen cells;
    /// this maps them onto conversation lines using the geometry of
    /// the frame the user is looking at.
    pub(crate) last_view: Option<ViewState>,

    /// The screen cell the drag last reported, while the button is
    /// held.
    ///
    /// Set between a press inside the conversation pane and its
    /// release. A terminal reports a drag only while the pointer
    /// moves, so a pointer pushed against the pane's edge goes quiet
    /// — the periodic tick reads this to keep scrolling there, one
    /// line per beat, until the release arrives or the document runs
    /// out. A release delivered outside the terminal's view never
    /// arrives as an event; buttonless motion (only sent with no
    /// button held) ends the drag instead, so a lost release cannot
    /// run the scroll for long.
    pub(crate) drag_position: Option<(u16, u16)>,

    /// The selectable document's segment lengths last frame.
    ///
    /// One number per segment in render order — conversation, seam,
    /// thinking, frozen, live, compaction, tools. Any change
    /// renumbers the lane at or below it, so the frame compares
    /// against this to decide whether a live selection still names
    /// the lines it was drawn over.
    pub(crate) selectable_shape: SelectableShape,
}

impl SelectionState {
    /// The transcript cell under screen column/row, if the position
    /// lands inside the last frame's conversation pane.
    ///
    /// The line clamps into the selectable document: the steer and
    /// timer rows that render below it never join the selection's
    /// line space, so a press on them — or on the empty pane under
    /// short content — anchors on the last line a copy can walk, the
    /// way an editor clamps a click below a document's end. A pane
    /// with nothing selectable offers no cell at all.
    pub(crate) fn cell_at(&self, column: u16, row: u16) -> Option<CellPos> {
        let view = self.last_view.as_ref()?;
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return None;
        }
        let last = view.selectable.checked_sub(1)?;
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)))
            .min(last);
        let col = usize::from(column.saturating_sub(view.area.x));
        Some(CellPos { line, col })
    }
}

/// Whether the view follows the newest content or holds its ground.
///
/// The stickiness posture the scroll keys drive: pinned views
/// re-anchor to the bottom on every layout; any scroll-up detaches,
/// and scrolling back within [`STICK_TOLERANCE`] lines of the bottom
/// — or pressing End or submitting — re-arms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Stick {
    /// The view follows the newest content.
    ///
    /// The resting posture: every layout re-anchors the view to the
    /// newest line, so streamed growth and new turns arrive into
    /// view. A scroll up leaves it for [`Hold`], and the stick
    /// tolerance — or End, or a submit — returns to it.
    #[default]
    Follow,

    /// The view holds its offset.
    ///
    /// The reading posture: the offset stands still while content
    /// grows beneath it, so the lines being read never move; only a
    /// deliberate act — scrolling back within [`STICK_TOLERANCE`]
    /// lines of the bottom, pressing End, or submitting — re-arms
    /// [`Follow`].
    Hold,
}

/// One display cell of the rendered transcript.
///
/// A conversation line index paired with the column, in cells, of
/// the character within that rendered line — the unit a mouse
/// selection drags over.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellPos {
    /// The conversation line the cell sits on.
    ///
    /// Index into the app's rendered-transcript row space, not the
    /// terminal's — scrolling moves the terminal row but never this
    /// index.
    pub(crate) line: usize,

    /// The cell column within the rendered line.
    ///
    /// Terminal cells, so a wide glyph counts two; the selection's
    /// character granularity comes from clipping to the line's width.
    pub(crate) col: usize,
}

/// A mouse selection between two display cells.
///
/// Order-independent: whichever end the press anchored stays put
/// while the head follows the drag — and the edge-scroll steps that
/// carry it past the viewport — in content coordinates, so scrolling
/// between drags does not move the selection.
pub(crate) struct CellSelection {
    /// The cell the press landed on.
    ///
    /// Stays fixed for the selection's lifetime — the anchor is where
    /// the press began, whichever end of the range it reads as.
    pub(crate) anchor: CellPos,

    /// The cell the drag currently reaches.
    ///
    /// Moves with the drag and the edge-scroll steps that carry it
    /// past the viewport; the copy takes the range it spans.
    pub(crate) head: CellPos,
}

/// Whether a character's display cells meet the selection's span.
///
/// A character counts as covered when any of its cells does: a
/// selection landing on the second cell of a wide character still
/// picks it up, the way a terminal's native selection treats
/// partially covered ink. The character occupies `cell..cell +
/// width`; `start..=end` are the span's cell bounds.
pub(crate) fn cells_overlap(cell: usize, width: usize, start: usize, end: usize) -> bool {
    cell <= end && cell.saturating_add(width).saturating_sub(1) >= start
}

/// The graphemes of `flat` whose painted cells meet `start..=end`.
///
/// Walks the display projection — the same cells the pane painted
/// for the reader to sweep — and takes every painted grapheme that
/// overlaps the span, copying each through its raw source range so
/// the clipboard reproduces the author's bytes (a covered selector
/// cluster copies whole, a covered family copies its members and
/// joiners). Clusters, not characters: a combining mark adds no cell
/// of its own, and a selected base carries its marks.
fn covered_chars(flat: &str, start: usize, end_inclusive: usize) -> String {
    let mut out = String::new();
    let mut cell = 0;
    for (range, width) in painted_steps(flat) {
        if cells_overlap(cell, width, start, end_inclusive)
            && let Some(cluster) = flat.get(range)
        {
            out.push_str(cluster);
        }
        cell = cell.saturating_add(width);
        if cell > end_inclusive {
            break;
        }
    }
    out
}

/// Take a shared buffer's contents, leaving it empty.
///
/// Recovers from poisoning — the same policy the observer writes
/// with — so finalized data still graduates after another thread's
/// panic left the lock poisoned.
pub(crate) fn take_locked<T: Default>(mutex: &Mutex<T>) -> T {
    std::mem::take(&mut *mutex.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Order a selection's ends by line then column.
///
/// The anchor stays wherever the press landed and the head follows
/// the drag, so either end can sit above the other; walking the
/// covered span and highlighting it both need the ends sorted.
/// Returns the lower end first, compared as `(line, column)` —
/// the order the rendered line space reads.
pub(crate) fn ordered(selection: &CellSelection) -> (&CellPos, &CellPos) {
    let (a, b) = (&selection.anchor, &selection.head);
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use ratatui::text::{Line, Span};

    #[test]
    fn a_drag_across_a_selector_cluster_copies_exactly_the_swept_cells() {
        // On "done ✔️ shipped" the check paints one cell at column 5, so
        // a sweep of columns 5..=8 covers "✔ sh" — and the copy is
        // exactly those characters, the cluster's raw bytes whole.
        // The raw walk this replaces scored the cluster two cells and
        // copied one character further ("✔️ s").
        let flat = "done \u{2714}\u{fe0f} shipped";
        assert_eq!(covered_chars(flat, 5, 8), "\u{2714}\u{fe0f} sh");
    }

    #[test]
    fn a_drag_past_a_family_copies_the_swept_characters_not_four_further() {
        // A family paints six cells (three members at two each); the
        // raw walk scored it two, so every sweep right of it copied
        // four characters further than the ink the reader covered.
        let flat = "xx \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} yy";
        assert_eq!(covered_chars(flat, 9, 11), " yy");
        assert_eq!(
            covered_chars(flat, 3, 8),
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "a sweep over the family copies its members and joiners whole"
        );
    }

    #[test]
    fn a_cluster_split_across_spans_counts_the_same_cells_flipped_and_copied() {
        // The copy walks the flattened line, so an emoji ending one
        // span and its modifier opening the next strips together and
        // paint four cells fewer than a per-span highlight walk that
        // forgets the emoji at the span boundary. Both walks run on
        // the one flattened projection now: the flipped ink and the
        // clipboard text spend the same cells for the same sweep.
        // A partial sweep — cells 0..=3, which the flattened walk
        // reads as `a`, the emoji, and the space — makes the
        // divergence visible in the ink itself: a per-span walk
        // forgets the emoji at the span boundary, keeps the modifier
        // as its own two cells, and flips it instead of the space.
        let flat = "a\u{1f44d}\u{1f3fd} b";
        let mut lines = vec![Line::from(vec![
            Span::raw("a\u{1f44d}"),
            Span::raw("\u{1f3fd} b"),
        ])];
        reverse_selection(
            &mut lines,
            0,
            &CellSelection {
                anchor: CellPos { line: 0, col: 0 },
                head: CellPos { line: 0, col: 3 },
            },
        );
        let line = lines.first().expect("the fixture line survives the flip");
        let ink: String = line
            .spans
            .iter()
            .filter(|span| {
                span.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .map(|span| span.content.to_string())
            .collect();
        let copied = covered_chars(flat, 0, 3);
        assert_eq!(
            ink, copied,
            "the flip and the copy take the same characters for the same sweep"
        );
        assert_eq!(
            display_width(&ink),
            4,
            "and both spend the four painted cells the sweep covers"
        );
    }

    use crate::app::TuiApp;
    use crate::app::test_support::*;
    use crate::cells::display_width;

    use crate::message::TuiMessage;

    #[test]
    fn the_scrollbar_stays_one_column_wide_at_any_document_length() {
        fn seed_turns(app: &mut TuiApp, count: usize) {
            for _ in 0..count {
                app.push_message(TuiMessage::User {
                    text: "a turn of a few wrapped lines".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                });
            }
        }

        fn thumb_columns(app: &mut TuiApp) -> Vec<u16> {
            let thumb = app.theme.ui.scrollbar_thumb;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
                .expect("terminal");
            terminal.draw(|frame| app.render(frame)).expect("draw");
            let buffer = terminal.backend().buffer();
            let mut columns: Vec<u16> = Vec::new();
            for y in 0..24u16 {
                for x in 0..80u16 {
                    if buffer[(x, y)].style().bg == Some(thumb) && !columns.contains(&x) {
                        columns.push(x);
                    }
                }
            }
            columns
        }

        let mut short = app();
        seed_turns(&mut short, 1);
        assert!(
            thumb_columns(&mut short).is_empty(),
            "a conversation that fits draws no scrollbar at all"
        );

        let mut long = app();
        seed_turns(&mut long, 60);
        assert_eq!(
            thumb_columns(&mut long),
            vec![79],
            "a long conversation's scrollbar is exactly the one reserved gutter column"
        );
    }
}
/// How close to the newest line a scroll action must land to count
/// as "back at the bottom".
///
/// The streaming region grows between a scroll and the next layout,
/// so an exact offset-zero check would detach a view that only fell
/// behind by growth.
pub(crate) const STICK_TOLERANCE: usize = 2;

/// Lines one wheel or touchpad scroll event moves.
///
/// One line per wheel event: a burst of events still accumulates
/// into fast travel, while a single notch lands the smallest useful
/// step.
pub(crate) const WHEEL_SCROLL_LINES: usize = 1;

/// What the last frame's conversation pane looked like.
///
/// The bridge between mouse events (screen cells, between frames)
/// and the conversation's line indices: the pane's rect and the skip
/// the viewport rendered with. Stale by at most one event while
/// frames are in flight — imperceptible for a drag.
#[derive(Clone, Copy)]
pub(crate) struct ViewState {
    /// The conversation pane's rect, gutter excluded.
    ///
    /// The screen-cell bounds a mouse event is tested against; the
    /// scrollbar's gutter column stays outside so a press there
    /// belongs to nothing.
    pub(crate) area: Rect,

    /// The first conversation line the pane showed.
    ///
    /// The additive base that turns a screen row into a
    /// conversation line index — clicks, drags, and the
    /// edge-scroll all map through it.
    pub(crate) skip: usize,

    /// The conversation's total line count that frame.
    ///
    /// The scroll space's size: every rendered row counts, the
    /// pending-steer and run-timer chrome below the document
    /// included, because the viewport scrolls over them all.
    pub(crate) total: usize,

    /// The line count a selection can cover.
    ///
    /// The rendered document through the running tools; the steer
    /// and timer rows below it stay chrome, so a press on them
    /// clamps to the last line above.
    pub(crate) selectable: usize,
}

/// Reverse-video the selected cells across the visible lines.
///
/// Splits the affected lines' spans at the selection's cell
/// boundaries and flips exactly the covered characters, so the
/// highlight is character-granular whatever the underlying styles.
/// The walk runs on the flattened line's display projection — the
/// same walk and the same cells the copy takes — with each painted
/// grapheme's raw range mapped back to the span that owns its first
/// retained character for styling. Flattening is what keeps a cluster split across
/// spans whole: an emoji ending one span and its modifier opening the
/// next strip together exactly as they paint, so the reversed ink
/// lands on the cells the reader swept and spends the same cells the
/// clipboard copies for the same sweep.
pub(crate) fn reverse_selection(lines: &mut [Line<'_>], skip: usize, selection: &CellSelection) {
    let (from, to) = ordered(selection);
    for (offset, line) in lines.iter_mut().enumerate() {
        let line_index = skip.saturating_add(offset);
        if line_index < from.line || line_index > to.line {
            continue;
        }
        let start = if line_index == from.line { from.col } else { 0 };
        let end = if line_index == to.line {
            to.col
        } else {
            usize::MAX
        };
        let mut flat = String::new();
        let mut owners: Vec<(usize, ratatui::style::Style)> = Vec::new();
        for span in line.spans.drain(..) {
            owners.push((flat.len(), span.style));
            flat.push_str(&span.content);
        }
        let mut cell = 0;
        let mut spans = Vec::with_capacity(owners.len().saturating_add(2));
        let mut chunk = String::new();
        let mut chunk_style = ratatui::style::Style::default();
        let head_kept = first_retained_offset(&flat);
        for (range, width) in painted_steps(&flat) {
            let Some(cluster) = flat.get(range.clone()) else {
                continue;
            };
            let style_at = if range.start == 0 {
                head_kept
            } else {
                range.start
            };
            let style = owners
                .iter()
                .rev()
                .find(|(at, _)| *at <= style_at)
                .map_or(chunk_style, |(_, style)| *style);
            let selected = cells_overlap(cell, width, start, end);
            if selected {
                if !chunk.is_empty() {
                    spans.push(ratatui::text::Span::styled(
                        std::mem::take(&mut chunk),
                        chunk_style,
                    ));
                }
                spans.push(ratatui::text::Span::styled(
                    cluster.to_string(),
                    style.add_modifier(ratatui::style::Modifier::REVERSED),
                ));
            } else {
                if !chunk.is_empty() && style != chunk_style {
                    spans.push(ratatui::text::Span::styled(
                        std::mem::take(&mut chunk),
                        chunk_style,
                    ));
                }
                chunk.push_str(cluster);
                chunk_style = style;
            }
            cell = cell.saturating_add(width);
        }
        if !chunk.is_empty() {
            spans.push(ratatui::text::Span::styled(chunk, chunk_style));
        }
        line.spans = spans;
    }
}
