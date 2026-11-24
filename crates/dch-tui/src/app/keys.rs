//! Key and mouse dispatch: the composer's editing keys, the command
//! prefix, the selection drags and presses — each routed to its
//! handler with the raw event the terminal reported.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};

use super::TuiApp;
use super::composer::normalize_pasted_newlines;
use super::queue::QueueMode;
use super::selection::{CellSelection, Stick};
use crossterm::event::MouseEventKind;
use std::sync::atomic::Ordering;
use std::time::Instant;

impl TuiApp {
    /// Apply one terminal event to the app state.
    ///
    /// Returns whether the event requires a redraw. Key releases and
    /// repeats are ignored so a held key fires once per press. While a
    /// permission ask is pending, the overlay owns the keyboard —
    /// keystrokes and pastes alike.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) => {
                self.composer.caret_blink.resolidify();
                if key.kind != KeyEventKind::Press {
                    return false;
                }
                self.handle_key(*key)
            }
            Event::Mouse(mouse) => self.handle_mouse(*mouse),
            Event::Paste(text) => {
                if self.permission.pending_permissions.front().is_some() {
                    return true;
                }
                self.composer.caret_blink.resolidify();
                if !self.questions.pending_questions.is_empty() {
                    return self.paste_into_the_question(text);
                }
                if self.queue.queue_mode == QueueMode::Focused {
                    return true;
                }
                self.composer
                    .input
                    .insert_str(&normalize_pasted_newlines(text));
                self.retire_theme_browse_if_gone();
                true
            }
            Event::Resize(_, _) => true,
            Event::FocusGained => {
                self.pacing.full_repaint_due = Some(Instant::now());
                true
            }
            Event::FocusLost => false,
        }
    }

    /// Apply one key press: the quit chord, copy chord, and
    /// page-scroll keys stay app-level, everything else belongs to the
    /// input editor — unless a permission ask is pending, in which
    /// case the overlay owns the keyboard first (`y` allows,
    /// `n`/Esc deny, the rest swallowed) and only the two `c` chords and
    /// the keys [`key_survives_the_sheet`] names pass through to the
    /// machinery below.
    ///
    /// Under a running agent, Ctrl+C Ctrl+C is one action in the
    /// Esc-Esc shape: with an empty composer, the first press only
    /// arms — nothing stops, nothing leaves — and the second press
    /// fires the cancel exactly as Esc-Esc fires it (the same
    /// canceller, the same record) with the program's termination
    /// riding the same press. Text in the composer clears first,
    /// run or not — clearing is not consenting, and the fire needs
    /// its presses behind an empty buffer like everywhere else. A
    /// stray key between the presses stands the whole action down.
    /// At rest the chord is the exit alone: a non-empty buffer clears
    /// first, arming nothing; on an empty one the first press arms
    /// and the second quits; any other key disarms, so the chord
    /// never fires from stale intent. Each rung is named by a notice,
    /// so the ladder reads from the frame instead of from memory.
    /// Ctrl+Shift+C copies the live selection where the terminal
    /// reports the shift; on terminals that collapse it to a plain
    /// Ctrl+C it simply joins the chord's clearing behavior. Up,
    /// Down, Home, and End fall back to the transcript while the
    /// input sits empty — Home jumps to the oldest line, End snaps
    /// to the newest, and the arrows scroll by one once nothing is
    /// recallable. The arrows join the editor as soon as anything
    /// is typed or recallable; Home and End follow the input's
    /// emptiness alone, so with an empty draft they keep scrolling
    /// the transcript even while history is recallable. Shift and
    /// an arrow, with a transcript selection up, moves the
    /// selection's head instead: the editor's select-by-keyboard.
    pub(in crate::app) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let handled = self.handle_key_inner(key);
        self.retire_theme_browse_if_gone();
        handled
    }

    /// The key dispatch itself, the core [`Self::handle_key`] wraps.
    ///
    /// Order is the contract. The chord ladders are disarmed first —
    /// any key but their own stands them down, so stale intent never
    /// fires. Then the overlays get their look in turn
    /// ([`Self::sheet_key`], then [`Self::completion_key`]): whoever
    /// claims the key owns it, and nothing beneath runs. Only a key
    /// no overlay wanted reaches the editor-and-chrome match below —
    /// chords, scrolling, the transcript's fallbacks, selection
    /// motion. Returns whether the key was consumed; the wrapper
    /// runs its post-key retirement on every path this can take.
    pub(in crate::app) fn handle_key_inner(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        ) {
            self.quit.disarm();
        }
        if !matches!(
            (key.code, key.modifiers),
            (KeyCode::Esc, KeyModifiers::NONE)
        ) {
            self.exit.cancel_arm = false;
        }
        if let Some(handled) = self.sheet_key(key) {
            return handled;
        }
        if let Some(handled) = self.completion_key(key) {
            return handled;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => self.esc_under_run_or_idle(),
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => self.steer_now(),
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                self.cycle_plan_view();
                true
            }
            (KeyCode::Char('c'), mods) if mods == KeyModifiers::CONTROL | KeyModifiers::SHIFT => {
                self.copy_selection_now();
                true
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if !self.composer.input.is_empty() {
                    self.composer.input.clear();
                } else if self
                    .state
                    .agent_running
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    if self.quit.armed {
                        self.exit.shutdown_flag.store(true, Ordering::SeqCst);
                        self.snapshot_exit_state_before_cancel();
                        self.fire_cancel();
                        self.post_notice("Agent cancelled".to_string());
                        self.quit.requested = true;
                    } else {
                        self.post_notice("press ctrl+c again to cancel and exit".to_string());
                        self.quit.arm();
                    }
                } else if self.quit.armed {
                    self.exit.shutdown_flag.store(true, Ordering::SeqCst);
                    self.quit.requested = true;
                } else {
                    self.quit.arm();
                }
                true
            }
            (KeyCode::F(2), _) => {
                self.cycle_verbosity();
                true
            }
            (KeyCode::PageUp, _) => {
                self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_add(10);
                self.scroll.stick = Stick::Hold;
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_sub(10);
                self.scroll.rearm_if_near_bottom();
                true
            }
            (KeyCode::Up, KeyModifiers::ALT) => {
                self.jump_user_message(true);
                true
            }
            (KeyCode::Down, KeyModifiers::ALT) => {
                self.jump_user_message(false);
                true
            }
            (KeyCode::Up, KeyModifiers::NONE)
                if self.composer.input.is_empty() && !self.composer.input.has_history() =>
            {
                self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_add(1);
                self.scroll.stick = Stick::Hold;
                true
            }
            (KeyCode::Down, KeyModifiers::NONE)
                if self.composer.input.is_empty() && !self.composer.input.has_history() =>
            {
                self.scroll.scroll_offset = self.scroll.scroll_offset.saturating_sub(1);
                self.scroll.rearm_if_near_bottom();
                true
            }
            (KeyCode::Home, _) if self.composer.input.is_empty() => {
                self.scroll.scroll_to_top();
                true
            }
            (KeyCode::End, _) if self.composer.input.is_empty() => {
                self.scroll.scroll_to_bottom();
                true
            }
            (KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right, KeyModifiers::SHIFT)
                if self.pointer.selection.is_some() =>
            {
                self.nudge_selection(key.code);
                true
            }
            (KeyCode::Tab | KeyCode::BackTab, KeyModifiers::NONE | KeyModifiers::SHIFT)
                if self.composer.input.text().starts_with('/') =>
            {
                self.tab_completion(key)
            }
            _ => {
                self.drop_applied_row_for_typing(key);
                let action = self
                    .composer
                    .input
                    .handle_key(key, self.composer.input_wrap_width);
                self.apply_input_action(action)
            }
        }
    }

    /// Apply one mouse event.
    ///
    /// The wheel scrolls by [`WHEEL_SCROLL_LINES`]; a press-drag-
    /// release over the conversation pane selects rendered
    /// characters — inside the app — and the release hands the
    /// covered text to the clipboard, confirmed on the notice row,
    /// while the highlight stays up until the next press. A drag
    /// pushed against the pane's
    /// top or bottom row scrolls the view in the drag's direction,
    /// extending the selection — one line per movement report, and
    /// one per tick while the pointer stays parked there; a press
    /// over the composer pane places its caret where it landed.
    /// Anything else is ignored. While a permission ask is pending,
    /// the overlay owns the pointer as it owns the keyboard: every
    /// report is swallowed and any parked drag is dropped. A question
    /// sheet holds the presses for the same reason but not the wheel —
    /// reading the conversation above the box is how a question gets
    /// answered, and the wheel is how that reading happens.
    pub(in crate::app) fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.permission.pending_permissions.front().is_some() {
            self.pointer.drag_position = None;
            return true;
        }
        if !self.questions.pending_questions.is_empty() {
            return match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_the_wheel(true),
                MouseEventKind::ScrollDown => self.scroll_the_wheel(false),
                _ => {
                    self.pointer.drag_position = None;
                    true
                }
            };
        }
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.quit.disarm();
        }
        match mouse.kind {
            MouseEventKind::ScrollUp if self.plan.wheel_over_todo_section(mouse.row) => {
                self.plan.scroll_todo_section(true);
                true
            }
            MouseEventKind::ScrollDown if self.plan.wheel_over_todo_section(mouse.row) => {
                self.plan.scroll_todo_section(false);
                true
            }
            MouseEventKind::ScrollUp => self.scroll_the_wheel(true),
            MouseEventKind::ScrollDown => self.scroll_the_wheel(false),
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if let Some(cell) = self.pointer.cell_at(mouse.column, mouse.row) {
                    self.pointer.selection = Some(CellSelection {
                        anchor: cell,
                        head: cell,
                    });
                    self.pointer.drag_position = Some((mouse.column, mouse.row));
                    true
                } else {
                    let retired = self.pointer.selection.take().is_some();
                    self.pointer.drag_position = None;
                    if self.toggle_plan_section_at(mouse.column, mouse.row)
                        || self.toggle_queue_section_at(mouse.column, mouse.row)
                    {
                        return true;
                    }
                    self.composer.place_input_caret(mouse.column, mouse.row) || retired
                }
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if self.pointer.selection.is_none() {
                    return false;
                }
                self.pointer.drag_position = Some((mouse.column, mouse.row));
                let mut handled = false;
                if let Some(cell) = self.pointer.cell_at(mouse.column, mouse.row)
                    && let Some(selection) = self.pointer.selection.as_mut()
                {
                    selection.head = cell;
                    handled = true;
                }
                self.drag_autoscroll() || handled
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.pointer.drag_position = None;
                if let Some(selection) = self.pointer.selection.take() {
                    if selection.anchor == selection.head {
                        self.toggle_thinking_at(mouse.column, mouse.row);
                        self.toggle_compaction_at(mouse.column, mouse.row);
                        self.toggle_tool_at(mouse.column, mouse.row);
                        self.copy_session_id_at(mouse.column, mouse.row);
                    } else {
                        let (text, covered_any) = self.selection_text(&selection);
                        if covered_any {
                            self.copy_selection_and_notice(&text);
                        }
                        self.pointer.selection = Some(selection);
                    }
                    true
                } else {
                    let toggled_thinking = self.toggle_thinking_at(mouse.column, mouse.row);
                    let toggled_compaction = self.toggle_compaction_at(mouse.column, mouse.row);
                    let toggled_tool = self.toggle_tool_at(mouse.column, mouse.row);
                    let copied_id = self.copy_session_id_at(mouse.column, mouse.row);
                    toggled_thinking || toggled_compaction || toggled_tool || copied_id
                }
            }
            MouseEventKind::Moved => {
                self.pointer.drag_position = None;
                false
            }
            _ => false,
        }
    }
}
/// Whether a key belongs to the app while an overlay owns the screen.
///
/// Paging the transcript and cycling the display verbosity read the
/// conversation; neither answers the question or the permission waiting on
/// top of it, and that conversation is exactly what a user needs in order
/// to decide. Both overlays therefore hold these keys off the sheet and
/// hand them to the machinery below — the copy chord on every sheet,
/// while the plain interrupt is the ask sheet's own deny and never
/// rides this pass: swallowing them would leave a user scrolling for
/// context with a keyboard that answers nothing. The steering chord
/// rides the same rule — the hint row offers it the moment a draft
/// meets a running agent, and an overlay swallowing a chord the
/// interface is advertising would be advertising noise.
pub(crate) fn key_survives_the_sheet(key: KeyEvent) -> bool {
    let chord = key.code == KeyCode::Char('c')
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    let steering = key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL;
    chord
        || steering
        || matches!(
            key.code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::F(2)
        )
}
