//! The pinned plan section and the expandable rows it drives:
//! the chords and clicks that toggle sections and rows, the
//! section's scroll geometry, and the tool-detail expansion state.

use super::TuiApp;
use super::selection::Stick;
use std::collections::HashMap;

use crate::todo_view;
use ratatui::layout::Rect;

impl TuiApp {
    /// Stamp the plan generation a todo graduation enqueues under.
    ///
    /// The observer's own tool-result path takes the stamp as each
    /// accepted `TodoWrite` result enters the graduation buffer; a
    /// graduation enqueued through [`graduations`](Self::graduations)
    /// directly takes it here, so the rows it lands carry the same age
    /// the retirement filter compares against.
    pub fn note_todo_graduated(&self) {
        self.state.note_todo_plan_graduated();
    }

    /// Toggle the tool expansion at screen column/row, if any.
    ///
    /// A click — press and release without travel — on a tool
    /// summary row or a running tool row opens or closes that
    /// call's expansion. Toggling a completed block changes the
    /// rendered line count below it, so it bumps the conversation
    /// generation and forfeits the selection, the same way any
    /// renumbering mutation does; a running call's rows are
    /// rebuilt every frame and need no bump. Returns whether a
    /// tool row was hit at all.
    pub(crate) fn toggle_tool_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.pointer.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        let baked = self.bake.tool_summary_lines.get(&line).cloned();
        let call_id = baked
            .clone()
            .or_else(|| self.tools.tool_running_lines.get(&line).cloned())
            .unwrap_or_default();
        if call_id.is_empty() {
            return false;
        }
        // A baked block whose detail the cap retired still registers its
        // plain summary rows — the settle measures the block by them — but
        // no press resolves there: nothing backs a toggle. A running call
        // toggles before any detail is retained, so the check is the baked
        // rows' alone.
        if baked.is_some() && !self.tools.tool_details.contains_key(&call_id) {
            return false;
        }
        let expanding = !self.expansion.expanded_tools.remove(&call_id);
        if expanding {
            self.expansion.expanded_tools.insert(call_id.clone());
        }
        if self.tools.tool_details.contains_key(&call_id) {
            if expanding {
                self.keep_the_seen_layout();
                self.scroll.stick = Stick::Hold;
            }
            self.note_call_reshape(&call_id);
            self.bake.conversation_generation = self.bake.conversation_generation.saturating_add(1);
            self.bake.rebake_whole();
            self.pointer.selection = None;
            self.pointer.drag_position = None;
        }
        true
    }

    /// Fold or lift the plan section a press landed on.
    ///
    /// A click anywhere on the pinned section — open rows or the
    /// folded marker row alike — walks the same posture cycle
    /// [`cycle_plan_view`](Self::cycle_plan_view) walks, so hiding
    /// the plan and bringing it back costs the mouse alone. Returns
    /// whether the press belonged to the section at all.
    pub(crate) fn toggle_plan_section_at(&mut self, column: u16, row: u16) -> bool {
        let rect = self.plan.todo_section_rect;
        if self.plan.latest_todos.is_empty()
            || rect.height == 0
            || column < rect.x
            || column >= rect.right()
            || row < rect.y
            || row >= rect.bottom()
        {
            return false;
        }
        self.cycle_plan_view();
        true
    }

    /// Toggle the thinking section a press landed on.
    ///
    /// Every row of the live block — marker or expanded body — drives
    /// the session's Follow/Suppressed mode, as tool blocks do; a
    /// completed section's rows toggle that section's display alone.
    /// Returns whether the press belonged to a thinking section at
    /// all, so callers can fall through to the tool rows.
    pub(crate) fn toggle_thinking_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.pointer.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        if self
            .thinking_live_rows
            .as_ref()
            .is_some_and(|rows| rows.contains(&line))
        {
            self.expansion.thinking_mode = match self.expansion.thinking_mode {
                ThinkingMode::Follow => ThinkingMode::Suppressed,
                ThinkingMode::Suppressed => ThinkingMode::Follow,
            };
            self.pointer.selection = None;
            self.pointer.drag_position = None;
            return true;
        }
        let Some(key) = self.bake.thinking_summary_lines.get(&line).cloned() else {
            return false;
        };
        let expanding = !self.expansion.expanded_thinking.remove(&key);
        if expanding {
            self.expansion.expanded_thinking.insert(key.clone());
            self.keep_the_seen_layout();
            self.scroll.stick = Stick::Hold;
        }
        self.note_block_reshape(&key);
        self.bake.conversation_generation = self.bake.conversation_generation.saturating_add(1);
        self.bake.rebake_whole();
        self.pointer.selection = None;
        self.pointer.drag_position = None;
        true
    }

    /// Walk the plan section's posture one step, measuring the
    /// capped window at the press.
    ///
    /// The frame's cut flag records what the last layout drew, which
    /// under rapid presses is the previous posture — a Full frame
    /// leaves it false, and a burst through Hidden would fold the
    /// list again instead of opening it. The press measures the
    /// capped window itself against the section's recorded width, so
    /// every transition reads the list as it stands.
    pub(crate) fn cycle_plan_view(&mut self) {
        let capped_cuts = self.capped_plan_cuts();
        self.plan.cycle_plan_view(capped_cuts);
    }

    /// Whether the capped posture would cut an entry, measured now.
    ///
    /// The same render the capped section draws — the same colors,
    /// inset, and body cap at the section's recorded width — read
    /// only for its last row's ellipsis, the mark the cap leaves
    /// when the list outgrows it. Measured at the press so a stale
    /// frame flag never decides the step.
    fn capped_plan_cuts(&self) -> bool {
        if self.plan.latest_todos.is_empty() {
            return false;
        }
        let width = if self.plan.todo_section_rect.width > 0 {
            self.plan.todo_section_rect.width
        } else {
            80
        };
        let colors = todo_view::TodoColors {
            plain: self.theme.ui.foreground,
            dim: self.theme.ui.dim,
            accent: self.theme.ui.primary,
            moved: self.theme.ui.secondary,
            badge_fill: self.theme.ui.primary,
            badge_letters: self.theme.ui.background,
        };
        let inner = width
            .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
            .max(1);
        todo_view::section_lines(
            &self.plan.latest_todos,
            &self.plan.todo_section_changes,
            &colors,
            inner,
            TODO_SECTION_BODY_ROWS,
            self.plan.clamped_todo_skip(),
            (self.plan_chord_hint(), self.plan.plan_cut_way()),
        )
        .last()
        .is_some_and(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.content.starts_with('\u{2026}'))
        })
    }

    /// Toggle the compaction notice a press landed on.
    ///
    /// Mirrors the completed thinking sections: every row of the
    /// notice toggles its own display between the collapsed summary
    /// and the pass's details. Returns whether the press belonged to
    /// a notice at all, so callers can fall through to the tool rows.
    pub(crate) fn toggle_compaction_at(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.pointer.last_view else {
            return false;
        };
        if column < view.area.x
            || column >= view.area.right()
            || row < view.area.y
            || row >= view.area.bottom()
        {
            return false;
        }
        let line = view
            .skip
            .saturating_add(usize::from(row.saturating_sub(view.area.y)));
        let Some(key) = self.bake.compaction_summary_lines.get(&line).cloned() else {
            return false;
        };
        let expanding = !self.expansion.expanded_compactions.remove(&key);
        if expanding {
            self.expansion.expanded_compactions.insert(key.clone());
            self.keep_the_seen_layout();
            self.scroll.stick = Stick::Hold;
        }
        self.note_block_reshape(&key);
        self.bake.conversation_generation = self.bake.conversation_generation.saturating_add(1);
        self.bake.rebake_whole();
        self.pointer.selection = None;
        self.pointer.drag_position = None;
        true
    }

    /// Fold the auto-expanded todo block when the next tool is
    /// running.
    ///
    /// The graduation fold covers the conversation's next completed
    /// step; this covers its next *started* one — a tool in flight is
    /// the conversation moving on even before its result graduates.
    /// Manual expansions are untouched: only the tracked block folds.
    /// A fold that clears a flag rebakes the whole bake, the way an
    /// eviction does: the block sits mid-conversation, past the tail
    /// the incremental bake re-renders, so its expanded rows would
    /// otherwise keep painting a detail no toggle backs any more.
    pub(crate) fn settle_todo_expansion(&mut self) {
        let Some(id) = self.expansion.auto_expanded_todo.as_ref() else {
            return;
        };
        let running = self
            .state
            .active_tools
            .lock()
            .is_ok_and(|tools| !tools.is_empty());
        if running {
            let id = id.clone();
            let folded = self.expansion.expanded_tools.remove(&id);
            self.expansion.auto_expanded_todo = None;
            if folded {
                self.note_call_reshape(&id);
                self.bake.conversation_generation =
                    self.bake.conversation_generation.saturating_add(1);
                self.bake.rebake_whole();
            }
        }
    }

    /// The rows the pinned plan section holds at this width.
    ///
    /// Counted off the same [`todo_view::section_lines`] the section
    /// renders, so the rows the layout reserves are the rows the
    /// frame sees — no blank beneath the plan, no row it cannot pay
    /// for. The folded posture is one row: the marker
    /// [`todo_view::folded_hint`] leaves, cheaper than the list and
    /// louder than nothing.
    pub(crate) fn todo_section_height(&self, width: u16, screen: u16) -> u16 {
        if self.plan.plan_view == PlanView::Hidden {
            // The air above the badge row.
            return 2;
        }
        let colors = todo_view::TodoColors {
            plain: self.theme.ui.foreground,
            dim: self.theme.ui.dim,
            accent: self.theme.ui.primary,
            moved: self.theme.ui.secondary,
            badge_fill: self.theme.ui.primary,
            badge_letters: self.theme.ui.background,
        };
        let inner = width
            .saturating_sub(TODO_SECTION_INSET.saturating_mul(2))
            .max(1);
        let body_cap = match self.plan.plan_view {
            PlanView::Full => usize::from(screen.saturating_sub(12)).max(TODO_SECTION_BODY_ROWS),
            _ => TODO_SECTION_BODY_ROWS,
        };
        self.full_body_cap.set(body_cap);
        let chord_hint = self.plan_chord_hint();
        let cut_way = self.plan.plan_cut_way();
        let lines = todo_view::section_lines(
            &self.plan.latest_todos,
            &self.plan.todo_section_changes,
            &colors,
            inner,
            body_cap,
            self.plan.clamped_todo_skip(),
            (chord_hint, cut_way),
        );
        let cut = body_cap != usize::MAX
            && matches!(lines.last(), Some(line)
                if line.spans.first().is_some_and(|span| span.content.starts_with('\u{2026}')));
        self.plan.plan_cut.set(cut);
        u16::try_from(lines.len()).unwrap_or(0)
    }

    /// Move a scrolled window's cut out of the interior of a todo block.
    ///
    /// The plan badge keeps the list's fraction on screen at every
    /// scroll, so a window that severs an expanded todo block mid-list
    /// shows a count and a set of rows that disagree — the badge says
    /// twelve, the frame carries eleven, and nothing marks the cut.
    /// While the view is scrolled (the bottom pin keeps its
    /// newest-line guarantee untouched), a cut landing inside a block
    /// that fits the viewport moves up to the block's own first row:
    /// the block renders whole or not at all, and the fraction and the
    /// rows can never disagree. A block taller than the viewport stays
    /// scrollable — there is no whole to keep.
    pub(crate) fn snap_skip_to_whole_todo_blocks(&self, skip: usize, viewport: usize) -> usize {
        if self.scroll.scroll_offset == 0 {
            return skip;
        }
        self.bake
            .todo_block_ranges
            .iter()
            .find(|block| skip > block.start && skip < block.end && block.len() <= viewport)
            .map_or(skip, |block| block.start)
    }

    /// Retain a graduated call's full detail, evicting at the cap.
    ///
    /// The one insertion path for the detail store: a graduation
    /// with captured content, and the resume seeding of blocks the
    /// file carried. A retried call graduates once per attempt
    /// under the same id, so the queue is requeued rather than
    /// duplicated and the detail ages from its newest attempt; the
    /// oldest entry retires past the cap with its toggle state — and
    /// a retirement that cleared an expanded flag rebakes at once,
    /// so baked rows cannot keep showing a shape no toggle backs.
    pub(crate) fn retain_tool_detail(&mut self, call_id: &str, input_json: &str, output: &str) {
        if call_id.is_empty() || input_json.is_empty() {
            return;
        }
        self.tools.tool_detail_order.retain(|id| id != call_id);
        self.tools.tool_detail_order.push_back(call_id.to_string());
        if self.tools.tool_detail_order.len() > TOOL_DETAIL_CAP
            && let Some(retired) = self.tools.tool_detail_order.pop_front()
        {
            self.tools.tool_details.remove(&retired);
            if self.expansion.expanded_tools.remove(&retired) {
                self.note_call_reshape(&retired);
                self.bake.conversation_generation =
                    self.bake.conversation_generation.saturating_add(1);
                self.bake.rebake_whole();
            }
        }
        self.tools.tool_details.insert(
            call_id.to_string(),
            ToolDetail {
                input_json: input_json.to_string(),
                output: output.to_string(),
            },
        );
    }

    /// Graduate one accepted `TodoWrite` result into the standing
    /// plan.
    ///
    /// The enforced render supplies the rows and where each entry
    /// stands; the submission supplies each row's whole wording, which
    /// a rendered page keeps only as its first line. A result that
    /// parses through neither half graduates nothing and returns
    /// `None`; one that lands returns its call id — the call the frame
    /// auto-expands — beside the rows, status deltas, and plan
    /// generation it recorded.
    pub(crate) fn graduate_todo_plan(
        &mut self,
        result: &crate::observer::ToolResultDisplay,
    ) -> Option<String> {
        let rows = crate::todo_view::parse_rendered(&result.full_output)
            .map(|rows| crate::todo_view::rows_with_submitted_subjects(rows, &result.full_input))
            .or_else(|| crate::todo_view::parse_submission(&result.full_input))?;
        let changes = crate::todo_view::status_changes(&rows, &self.plan.latest_todos);
        self.plan
            .todo_changes
            .insert(result.call_id.clone(), changes.clone());
        self.plan.todo_section_changes = changes;
        self.plan.latest_todos = rows;
        self.plan.todo_generation = self
            .state
            .todo_graduation_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        self.expansion.expanded_tools.insert(result.call_id.clone());
        Some(result.call_id.clone())
    }
}

/// The pinned plan section's state: the standing todo list, its
/// deltas and generation, and the section's own posture and window.
///
/// Everything the plan panel keeps between frames, in one place —
/// the accepted `TodoWrite` list with the per-update accent deltas,
/// the interruption flag a failed run leaves, and the view's
/// posture, cut marker, scroll window, and screen rect. The
/// graduation that replaces the list stays on [`TuiApp`]; this is
/// what it writes into.
pub(crate) struct PlanState {
    /// The session's standing todo list, for the plan section.
    ///
    /// Updated at every accepted `TodoWrite` graduation — the enforced
    /// render first, the submission as fallback — so the pinned section
    /// shows the plan the run actually holds, however many updates the
    /// transcript holds. A rejected update is not state: the tool
    /// refused the submission and the last accepted list stands. The
    /// plan belongs to the session, not the run: it stays up after its
    /// run lands and stands across claims while work remains — carried
    /// exactly as the runner carries its own store — and a claim
    /// retires it only when every entry stands completed, the same
    /// edge the runner clears its store at. Steering delivers mid-run
    /// and never claims, so a plan keeps updating through its own run.
    pub(crate) latest_todos: Vec<todo_view::TodoRow>,

    /// The plan generation `latest_todos` graduated under.
    ///
    /// Snapshotted from the shared state's
    /// [`plan_generation`](crate::observer::TuiObserverState::plan_generation)
    /// at every `TodoWrite` graduation that replaces the rows, so the
    /// retirement filter can tell a retirement newer than the rows —
    /// which may take a pending row — from a stale one a reborn entry
    /// outlived, whatever order the frame drains the two halves in.
    pub(crate) todo_generation: u64,

    /// The rows each todo update changed, by the update's call id.
    ///
    /// The conversation's per-update block accents exactly these
    /// rows: the ids whose status moved in that call, against the
    /// snapshot the previous call left. Bounded with the transcript
    /// itself; the map rides in memory only and dies with the rows it
    /// keyed — a fresh or cleared session carries no deltas, and the
    /// resume path re-derives its accents from the seeding walk, which
    /// records each restored update's deltas against the snapshot
    /// before it.
    pub(crate) todo_changes: HashMap<String, HashMap<String, todo_view::TodoStatus>>,

    /// The rows the newest todo update changed, for the plan section.
    ///
    /// The pinned section accents what the last update moved — the
    /// same deltas the newest conversation block carries, kept
    /// beside the list they name so the section can say "this is
    /// what just moved" without knowing which call id was newest.
    pub(crate) todo_section_changes: HashMap<String, todo_view::TodoStatus>,

    /// Whether the run that last landed was interrupted.
    ///
    /// An interrupted run's plan is owed work — the session resumes
    /// into it — so the next claim retires nothing: the plan stands
    /// until the resumed run files its own. Set by the [`RunEnded`](crate::observer::Graduation::RunEnded) arm
    /// of the drain when `success` is false, consumed and cleared by
    /// the Sent arm; a run that files no plan leaves the flag
    /// irrelevant either way.
    pub(crate) plan_interrupted: bool,

    /// The plan section's posture, as Ctrl+T or a click leaves it.
    ///
    /// Capped by default — four task rows and the ellipsis — with one
    /// press, the chord or a click on the section, opening the list up
    /// to the screen's height and the next folding the section away. A
    /// view preference, not a mode: no keys are owned, the plan keeps
    /// updating underneath, and the claims leave the posture standing
    /// — whatever the last run left is what the next run's plan opens
    /// in. The cycle skips the
    /// open posture when the capped one already shows every entry —
    /// opening a list that stands whole changes nothing, so the press
    /// spends itself on the fold.
    pub(crate) plan_view: PlanView,

    /// Whether the capped posture's last render cut an entry.
    ///
    /// The layout computes the section's rows from the same render the
    /// frame draws, so it is the one place that already knows whether
    /// the cap ate anything; the cell carries that answer to the chord
    /// hint the identity row names — whether to offer the grow chord —
    /// while the posture cycle measures fresh at its own press. Fresh
    /// until the first frame renders — `false` reads as "the cap cut
    /// nothing" until one proves otherwise.
    pub(crate) plan_cut: std::cell::Cell<bool>,

    /// The pinned section's scroll: how many entries the window skips.
    ///
    /// The wheel over the section steps this window one entry at a
    /// time — a long plan stays readable without a scrollbar and
    /// without the section's height ever growing past the screen.
    /// Zero shows the plan's head; the window always keeps at least
    /// one entry visible, and Ctrl+T's posture change resets it.
    pub(crate) todo_scroll: usize,

    /// The rect the last frame painted the pinned section into.
    ///
    /// The wheel and the section's click toggle need to know whether
    /// the pointer stands on the section; the render records the rect
    /// it filled — the folded marker row included — so the next mouse
    /// event answers against the same geometry, and the layout clears
    /// it when the section stands empty.
    pub(crate) todo_section_rect: Rect,
}

impl Default for PlanState {
    fn default() -> Self {
        Self {
            latest_todos: Vec::new(),
            todo_generation: 0,
            todo_changes: HashMap::new(),
            todo_section_changes: HashMap::new(),
            plan_interrupted: false,
            plan_view: PlanView::Capped,
            plan_cut: std::cell::Cell::new(false),
            todo_scroll: 0,
            todo_section_rect: Rect::default(),
        }
    }
}

/// Which blocks the transcript renders expanded, and the session's
/// thinking-display preference.
///
/// The expansion toggles a press flips and the auto-expansion a
/// graduation arms, in one place: tool blocks keyed by call id,
/// thinking sections and compaction notices by positional key, and
/// the mode that decides whether a live reasoning stream renders
/// open or folded. The toggles themselves stay on [`TuiApp`] — a
/// press lands on the whole app — but every flag it flips lives
/// here.
pub(crate) struct ExpansionState {
    /// Tool calls whose blocks render expanded.
    ///
    /// Keyed by call id; a click on a summary row toggles membership.
    /// Running calls share the key with their completed block, so an
    /// expansion opened while the tool runs survives its graduation.
    pub(crate) expanded_tools: std::collections::HashSet<String>,

    /// The todo block the last update auto-expanded, folded again
    /// once the conversation moves on.
    ///
    /// A `TodoWrite` result expands so the reader sees the plan as it
    /// stands; the expansion is momentary — the next graduation, or
    /// the next tool starting, folds it back to its summary row (the
    /// standing plan panel carries the list from there). Manual
    /// expansions never ride this: the field tracks only the one
    /// auto-expanded block, and once folded it is done.
    pub(crate) auto_expanded_todo: Option<String>,

    /// The session's thinking-display preference.
    ///
    /// Follow renders live reasoning expanded and auto-collapses at
    /// graduation; Suppressed renders every stream collapsed from its
    /// first delta. Only clicks on the live row change it — completed
    /// sections toggle their own display and leave this untouched.
    /// Session-only: never persisted, every session starts in Follow.
    pub(crate) thinking_mode: ThinkingMode,

    /// Which completed thinking sections render expanded, by block key.
    ///
    /// Keys are message-and-block positions, stable for the session
    /// because the conversation only ever appends. Empty means every
    /// section renders collapsed — the default both after graduation
    /// and after loading a session.
    pub(crate) expanded_thinking: std::collections::HashSet<String>,

    /// Which compaction notices render expanded, by message key.
    ///
    /// Same positional-key scheme as the thinking sections, so the
    /// expand gesture covers the notices with one vocabulary.
    pub(crate) expanded_compactions: std::collections::HashSet<String>,
}

impl Default for ExpansionState {
    fn default() -> Self {
        Self {
            expanded_tools: std::collections::HashSet::new(),
            auto_expanded_todo: None,
            thinking_mode: ThinkingMode::Follow,
            expanded_thinking: std::collections::HashSet::new(),
            expanded_compactions: std::collections::HashSet::new(),
        }
    }
}

impl PlanState {
    /// Retire the plan section when its work is finished.
    ///
    /// The plan belongs to the session, not the run: a claim retires it
    /// only when every entry stands completed — the same edge the runner
    /// clears its own carried store at — while an unfinished plan keeps
    /// standing across the claim exactly as the store carries it, so the
    /// section never desyncs from the plan the model and the belts are
    /// working against. An empty plan retires trivially; an interrupted
    /// run's caller never reaches here at all.
    pub(crate) fn retire_finished_plan(&mut self) {
        if self
            .latest_todos
            .iter()
            .all(|row| row.status == todo_view::TodoStatus::Completed)
        {
            self.latest_todos.clear();
            self.todo_section_changes.clear();
        }
    }

    /// Walk the plan section's posture one step.
    ///
    /// Capped opens to the whole list when the capped window is
    /// actually cutting entries — the caller measures it at the
    /// press, because the frame's own flag may describe a previous
    /// posture — otherwise the next step folds the section away, and
    /// a fold steps back to the capped posture. The chord and a
    /// click on the section both spend one press on one step, and
    /// every step resets the section's scroll window.
    pub(crate) fn cycle_plan_view(&mut self, capped_cuts: bool) {
        self.plan_view = match self.plan_view {
            PlanView::Capped if capped_cuts => PlanView::Full,
            PlanView::Capped | PlanView::Full => PlanView::Hidden,
            PlanView::Hidden => PlanView::Capped,
        };
        self.todo_scroll = 0;
    }

    /// The section's window offset, never past the last entry.
    ///
    /// The wheel moves one entry at a time and the window always
    /// keeps at least one entry in it, so the offset clamps against
    /// the plan's own length — a plan that shrank under a standing
    /// scroll heals on the next frame without a reset of its own.
    pub(crate) fn clamped_todo_skip(&self) -> usize {
        if self.latest_todos.is_empty() {
            0
        } else {
            self.todo_scroll
                .min(self.latest_todos.len().saturating_sub(1))
        }
    }

    /// Step the section's window one entry under the wheel.
    ///
    /// Down moves deeper into the plan, up back toward its head; the
    /// clamp keeps at least one entry on screen either way, so the
    /// window never scrolls past the plan's end.
    /// The gesture that reaches the cut rows, for the ellipsis.
    ///
    /// The posture decides it: while the cap holds the window the
    /// chord opens the list, and once the list is open the wheel walks
    /// it — the chord there folds the section, so naming it would
    /// promise the opposite of the next press. Open follows the
    /// posture, never the window's size: a screen too short to widen
    /// the window still leaves the wheel as the way in.
    pub(super) fn plan_cut_way(&self) -> &'static str {
        match self.plan_view {
            PlanView::Full => "wheel to walk",
            PlanView::Capped | PlanView::Hidden => "ctrl+t to open",
        }
    }

    pub(crate) fn scroll_todo_section(&mut self, up: bool) {
        if self.latest_todos.is_empty() {
            return;
        }
        let deepest = self.latest_todos.len().saturating_sub(1);
        self.todo_scroll = if up {
            self.todo_scroll.saturating_sub(1)
        } else {
            self.todo_scroll.saturating_add(1).min(deepest)
        };
    }

    /// Whether the wheel's row stands inside the painted section.
    ///
    /// The rect the last frame recorded is the geometry the pointer
    /// event was reported against; a hidden or empty section never
    /// claims the wheel.
    pub(crate) fn wheel_over_todo_section(&self, row: u16) -> bool {
        self.todo_section_rect.height > 0
            && self.plan_view != PlanView::Hidden
            && !self.latest_todos.is_empty()
            && row >= self.todo_section_rect.y
            && row
                < self
                    .todo_section_rect
                    .y
                    .saturating_add(self.todo_section_rect.height)
    }
}

/// A tool call's retained full data, for the expanded block.
///
/// What the dispatch-side capture recorded, held on the app past
/// graduation so the expansion outlives the shared store's
/// turnover.
pub(crate) struct ToolDetail {
    /// The call's input, pretty-printed JSON.
    ///
    /// What the dispatch-side capture recorded at call time; the
    /// expansion renders it verbatim below the summary row.
    pub(crate) input_json: String,

    /// The call's output, within the retention cap.
    ///
    /// Truncated to the capture store's bound before it lands here,
    /// so a host rendering the expansion never reparses megabytes.
    pub(crate) output: String,
}

/// The thinking-display preference for the current session.
///
/// Follow is the default: live reasoning renders expanded and
/// auto-collapses when the stream completes. Suppressed renders every
/// stream collapsed from its first delta — the remembered answer to a
/// live-stream collapse, held only for the session.
pub(crate) enum ThinkingMode {
    /// Live reasoning renders expanded and auto-collapses at graduation.
    ///
    /// The default every session starts in; clicking the live row
    /// while expanded switches to
    /// [`Suppressed`](ThinkingMode::Suppressed).
    Follow,

    /// Every reasoning stream renders collapsed from its first delta.
    ///
    /// The remembered answer to a live-stream collapse; clicking the
    /// live row while collapsed switches back to
    /// [`Follow`](ThinkingMode::Follow).
    Suppressed,
}
/// The plan section's posture, as Ctrl+T or a click on the section
/// leaves it.
///
/// One chord — or one click on the section — walks the three: capped
/// at its four task rows with the cut named, opened up to the screen's
/// height with the wheel walking a taller plan, or folded away
/// entirely. A posture is a view
/// preference — the plan keeps updating underneath every one of them,
/// and the claims leave it standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanView {
    /// Capped at [`TODO_SECTION_BODY_ROWS`] task rows, the ellipsis
    /// naming the rest and the chord that shows them.
    Capped,

    /// Opened to the whole list up to the screen's height — taller
    /// plans cap with the ellipsis, and the wheel walks the rest —
    /// because opening it is a deliberate act with the cost in plain
    /// sight.
    Full,

    /// Folded away: the conversation takes the rows back, one dim
    /// marker row left standing in the section's place — see
    /// [`folded_hint`](crate::todo_view::folded_hint).
    Hidden,
}

/// How many graduated calls keep their expansion data.
///
/// The most recent calls stay expandable; older ones fold back to
/// plain summary rows once this many newer calls have graduated.
/// The retained set is bounded the way the capture store is, in
/// FIFO order rather than a wholesale drop — the newest tool
/// blocks, the ones a reader is still working through, are the
/// ones that stay openable.
pub(crate) const TOOL_DETAIL_CAP: usize = 256;

/// The most task rows the pinned plan section holds.
///
/// Four: the section is the plan's window, not its archive — the
/// conversation's per-update blocks carry every task in full, so the
/// section spends the conversation's rows on the tasks nearest the
/// plan's top and names the rest with one ellipsis line. The header
/// and that line ride outside the count, so a full section stands at
/// six rows at the very widest plan.
pub(crate) const TODO_SECTION_BODY_ROWS: usize = 4;

/// The plan section's inset from each of the frame's edges.
///
/// The section's own breath from the frame: the plan's rows read as
/// the transcript's register, plain lines rather than a panel, and
/// never flush against the frame — the badge rides flush with the
/// rows inside it.
pub(crate) const TODO_SECTION_INSET: u16 = 2;

#[cfg(test)]
mod tests {
    use super::PlanView;
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn a_clear_retires_the_plan_with_the_conversation() {
        // The plan belongs to the conversation: /clear starts a fresh
        // session, and the retired plan must not linger in the section
        // the cleared transcript left behind.
        let mut subject = app();
        subject.plan.latest_todos = vec![crate::todo_view::TodoRow {
            id: "1".to_string(),
            subject: "Ship the release".to_string(),
            status: crate::todo_view::TodoStatus::Pending,
        }];
        assert!(
            subject
                .plan
                .latest_todos
                .iter()
                .any(|row| row.subject.contains("Ship the release")),
            "the plan stands before the clear"
        );
        subject.clear_command();
        assert!(
            subject.plan.latest_todos.is_empty(),
            "the cleared conversation retires its plan"
        );
    }

    #[test]
    fn rapid_ctrl_t_presses_measure_the_capped_window_at_the_press() {
        // A tall frame drew the Full posture, whose layout cuts
        // nothing: presses in the burst that follows must measure the
        // capped window themselves — the frame's stale flag, read
        // verbatim, folded the list back where the press should have
        // opened it.
        let mut subject = app();
        subject.plan.latest_todos = (0..6)
            .map(|index| crate::todo_view::TodoRow {
                id: format!("t{index}"),
                subject: format!("task {index} stands"),
                status: crate::todo_view::TodoStatus::InProgress,
            })
            .collect();
        let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        let draw = |subject: &mut TuiApp| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
            terminal.draw(|frame| subject.render(frame)).unwrap();
        };
        draw(&mut subject);
        assert!(subject.handle_key(ctrl_t), "the chord is taken");
        assert_eq!(
            subject.plan.plan_view,
            PlanView::Full,
            "a cutting cap opens"
        );
        draw(&mut subject);
        assert!(subject.handle_key(ctrl_t), "the chord is taken");
        assert_eq!(
            subject.plan.plan_view,
            PlanView::Hidden,
            "the full list folds"
        );
        assert!(subject.handle_key(ctrl_t), "the chord is taken");
        assert_eq!(
            subject.plan.plan_view,
            PlanView::Capped,
            "the fold steps back to the capped posture"
        );
        assert!(subject.handle_key(ctrl_t), "the chord is taken");
        assert_eq!(
            subject.plan.plan_view,
            PlanView::Full,
            "the press measures the capped window — the stale Full frame's \
             no-cut flag does not fold the list again"
        );
    }

    #[test]
    fn a_space_retires_the_command_cycle_and_tab_completes_values() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.input.set_text("/".to_string());
        let (_, commands, _) = app.fresh_completion("/");
        let steps = commands
            .iter()
            .position(|name| name == "theme")
            .expect("the command list carries /theme");
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        for _ in 0..steps {
            assert!(app.handle_key(down), "the arrows walk the command menu");
        }
        assert_eq!(
            app.composer.input.text(),
            "/theme",
            "the walk landed on the /theme row"
        );
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            "the space is the editor's, taken as typed"
        );
        let (_, themes, _) = app.fresh_completion("/theme ");
        let first = themes.first().cloned().unwrap_or_default();
        assert!(app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(
            app.composer.input.text(),
            format!("/theme {first}"),
            "Tab at the value position completes a theme — the command list is a word behind"
        );
        assert!(
            app.completion_popup_live(),
            "the popup stands on the theme list"
        );
    }

    #[test]
    fn keys_after_a_retired_browse_do_not_flip_the_theme() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.input.set_text("/theme ".to_string());
        let original = app.theme.name;
        // Preview a different theme.
        assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert!(
            app.theme.name != original,
            "the arrows previewed away from the original"
        );
        // Backspace out of the applied row and past the space — the
        // browse ends on the press that removes the space, and the
        // theme restores.
        while app.composer.input.text().starts_with("/theme ") {
            assert!(
                app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
                "the editor takes the backspace"
            );
        }
        assert_eq!(
            app.theme.name, original,
            "the retire restored the session's theme"
        );
        // Now type ordinary text: the theme must NOT change on any
        // keypress. This is the exact flip-on-every-key bug.
        for glyph in "hello world".chars() {
            assert!(app.handle_key(KeyEvent::new(KeyCode::Char(glyph), KeyModifiers::NONE)));
            assert_eq!(
                app.theme.name, original,
                "typing '{glyph}' after a retired browse leaves the theme alone"
            );
        }
        // Backspaces too.
        for _ in 0..5 {
            assert!(app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
            assert_eq!(
                app.theme.name, original,
                "backspacing after a retired browse leaves the theme alone"
            );
        }
    }
}
