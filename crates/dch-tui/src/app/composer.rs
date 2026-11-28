//! The composer: the input's apply-and-submit path, the caret
//! placement the frame shares with the editor, and the paste
//! normalizations the sheet and the queue agree on.

use super::TuiApp;
use super::caches::InputWrapCache;
use std::sync::Arc;

use crate::commands;
use crate::commands::Parsed;
use crate::input::InputAction;
use crate::input::InputEditor;
use crate::message::TuiMessage;
use crate::queue::Submission;
use dch_config::PathStyle;
use ratatui::layout::Rect;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl TuiApp {
    /// Route an editor action.
    ///
    /// A submit hands the prompt to [`submit_text`](Self::submit_text)
    /// and re-anchors the view to the conversation's newest row; the rest
    /// need nothing beyond the redraw the caller grants.
    pub(crate) fn apply_input_action(&mut self, action: InputAction) -> bool {
        match action {
            InputAction::Submit(text) => {
                self.submit_text(text);
                true
            }
            InputAction::Redraw | InputAction::None => true,
        }
    }

    /// Send one submitted prompt to the agent driver.
    ///
    /// The prompt joins the shared queue before its id is sent, so the
    /// driver always finds the entry behind an id it was handed — and
    /// finds it as it stands then, which is what lets a prompt be fixed
    /// after the fact rather than spent. A send nothing will take puts
    /// the prompt back out of the queue: an entry no driver can claim
    /// would otherwise sit in the panel, offering edits to a prompt that
    /// can never leave.
    ///
    /// The transcript row is not written here. The driver records the
    /// claim with [`push_sent`](crate::observer::TuiObserverState::push_sent)
    /// and the drain turns it into the row, so what the transcript shows
    /// is the text that went out — an edit included, a cancel excluded —
    /// at the place in the transcript its claim time gives it. A bare
    /// app has no driver to claim, so its echo stays here, where it is
    /// the only record the prompt would ever get.
    ///
    /// A send the channel refuses is the session ending around the prompt:
    /// the entry leaves the queue, the notice row says why, and the words
    /// go back into the composer. They were never sent, so they take no
    /// transcript row; and they are the user's, so a driver's exit is not
    /// the thing that throws them away.
    pub(crate) fn submit_text(&mut self, text: String) {
        match commands::parse(&text) {
            Parsed::NotACommand => {}
            parsed => {
                self.handle_command(parsed);
                self.scroll.scroll_to_bottom();
                return;
            }
        }
        if self.host.submit_tx.is_some() {
            let was_empty = self.queued_len() == 0;
            let idle = self.state.live_claim.load(Ordering::SeqCst) == u64::MAX
                && !self.state.agent_running.load(Ordering::SeqCst);
            self.queue.handed = None;
            let id = self.push_queued(text.clone());
            if self.host.send_queued_id(id) {
                if was_empty && idle {
                    self.queue.handed = Some(id);
                }
                self.scroll.scroll_to_bottom();
                return;
            }
            self.remove_queued(id);
            self.post_notice("the driver has gone — nothing more can be sent".to_string());
            self.composer.input.set_text(text);
        } else {
            self.push_message(TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            });
        }
        self.scroll.scroll_to_bottom();
    }

    /// Tell the driver that the prompt `id` was corrected after it went out.
    ///
    /// The gone-arm of a commit: the entry left the queue while its text
    /// stood open in the composer, so the edited wording has to reach the
    /// agent as a turn of its own — and the turn that went out with the
    /// superseded wording has to stop standing in front of it, which only
    /// the driver's loop can do. The cancel and its flag are raised only
    /// when the correction names the turn in flight — the driver mirrors
    /// its live claim into shared state, and a correction committing
    /// while some other run is going must not cancel it: that run is an
    /// unrelated conversation the fix was never aimed at. The cut itself
    /// follows the most recent claim however it settled — a turn that
    /// already landed and still stands as the newest claim is taken out
    /// and the fix runs in its place, no cancel needed.
    /// With no driver there is no turn to cut and nothing to run it, so
    /// the corrected wording goes into the transcript the way any
    /// prompt does when this app is standing alone; with a driver whose
    /// channel has gone silent the send is refused, and the words come
    /// back the way submit's own refusal returns them — a notice row
    /// and the composer holding them again. The words are a saved
    /// prompt's body, so the no-driver fallback delivers them
    /// literally — the command they happen to spell is the user's
    /// words, never a dispatch. The named turn in flight is
    /// stopped, not waited out: the correction is sent first and the
    /// runner's cancel raised beside it, so a superseded turn stops
    /// spending its run on wording the user has already replaced — the
    /// driver clears the signal before the corrected turn starts.
    pub(crate) fn correct_claimed(&mut self, id: u64, text: String) {
        let taken = self.host.submit_tx.as_ref().is_some_and(|tx| {
            tx.send(Submission::Correct {
                id,
                text: text.clone(),
            })
            .is_ok()
        });
        if taken {
            if self.state.live_claim.load(Ordering::SeqCst) == id {
                self.state.correction_cancel.store(id, Ordering::SeqCst);
                self.fire_cancel();
                if self.state.live_claim.load(Ordering::SeqCst) != id {
                    self.state
                        .correction_cancel
                        .store(u64::MAX, Ordering::SeqCst);
                }
            }
        } else {
            self.submit_literal(text);
        }
    }

    /// The composer's height for the rows its buffer wraps to.
    ///
    /// Text rows clamped to the input cap, plus the vertical padding
    /// and the rule row — the one number the welcome block and the
    /// frame layout both budget the composer by, computed identically
    /// on both paths.
    pub(crate) fn composer_height_for(rows: &[String]) -> u16 {
        let cap = usize::from(INPUT_TEXT_ROWS);
        let text_rows = rows.len().clamp(1, cap);
        u16::try_from(text_rows)
            .unwrap_or(INPUT_TEXT_ROWS)
            .saturating_add(INPUT_VERTICAL_PADDING.saturating_mul(2))
            .saturating_add(1)
    }

    /// Whether the composer's caret paints this frame.
    ///
    /// False while the blink phase has it off — unless the config
    /// spells the display `static`, in which case the caret stands solid
    /// through every phase — and for the whole time a permission ask
    /// is pending: the sheet owns the composer's rows and its keys,
    /// and a caret there would point at nothing the user can edit.
    /// The waiting section's focus and its ask hide it for the same
    /// reason: the keys are landing on the queue, and a caret in a
    /// composer that cannot be typed into would point at a lie. The
    /// caret is the app's own painted cell — the terminal's own
    /// cursor stays hidden for the composer — so this is the whole
    /// show-or-hide decision.
    #[must_use]
    pub fn composer_caret_visible(&self) -> bool {
        (matches!(
            self.config.display.caret_display,
            dch_config::CaretDisplay::Static
        ) || self.composer.caret_blink.on)
            && !self.queue.section_holds_keys()
            && self.permission.pending_permissions.front().is_none()
            && self.questions.pending_questions.is_empty()
            && self.pickers.session_picker.is_none()
            && self.pickers.model_picker.is_none()
    }

    /// The place row's text: the working directory spelled per the
    /// config's [`PathStyle`] — the full path, or the `~/`-relative
    /// shape — with the git branch in parentheses when the host named
    /// one. Where the session stands, named above the box the user
    /// types into; the footer keeps what the session stands in
    /// briefly, its context pair and its id.
    pub(crate) fn place_text(&self) -> String {
        match (&self.host.working_dir, &self.host.git_branch) {
            (Some(dir), Some(branch)) => {
                format!(
                    "{} ({branch})",
                    display_path(dir, self.config.display.path_style)
                )
            }
            (Some(dir), None) => display_path(dir, self.config.display.path_style),
            (None, _) => String::new(),
        }
    }

    /// The composer's identity row: the model with its thinking
    /// effort and the permission mode, one dim line closing the box
    /// over its final row of air, the composer's state tag at the
    /// row's very right.
    ///
    /// These are the facts about the *next* thing the user types —
    /// which model answers and what it may do without asking — so they
    /// ride inside the composer's box, one row of context under its
    /// text, rather than on the footer below where they read as
    /// facts about the session's tail.
    pub(crate) fn composer_identity_text(&self) -> String {
        let effort = match self.config.api.reasoning_effort.as_deref() {
            Some(level) => capitalize_first(level),
            None => "Auto".to_string(),
        };
        format!(
            " {} ({effort})  ·  mode: {}",
            self.config.api.model,
            self.config.runner.permission_mode.as_str()
        )
    }

    /// The composer's state as its identity-row tag, empty when the
    /// buffer fits the window.
    ///
    /// Reports which line the caret is on over the buffer's total
    /// while the draft outgrows the composer's window — the one fact
    /// about the draft the box itself cannot show. Submissions queued
    /// behind the driver count themselves in the queue's own section,
    /// and the footer's right edge carries no text in any state.
    /// `rows` and `caret` arrive from the frame's single wrap of the
    /// buffer, so asking never re-wraps the editor.
    pub(crate) fn input_state_tag(rows: &[String], caret: (u16, u16)) -> String {
        if rows.len() > usize::from(INPUT_TEXT_ROWS) {
            let (caret_row, _) = caret;
            format!(
                "{}/{}",
                usize::from(caret_row).saturating_add(1).min(rows.len()),
                rows.len()
            )
        } else {
            String::new()
        }
    }
}

/// The composer's state: the editor, the wrap geometry it renders
/// against, and the cached wrap itself.
///
/// Everything the input surface keeps between frames — the editor
/// with its buffer and history, the width the last frame wrapped
/// at, the pane geometry a press maps through, the caret's blink
/// phase, and the wrap cache that keeps an untouched frame from
/// paying the re-wrap.
pub(crate) struct ComposerState {
    /// The input editor: multi-line buffer, cursor, and history.
    ///
    /// Every non-app key routes here. Submits surface as
    /// [`InputAction::Submit`], which joins the prompt to the shared
    /// queue and hands its id down the submit channel; the composer's
    /// own text is empty from that moment, and what a prompt enters the
    /// conversation with is its claim, not this hand-off.
    pub(crate) input: InputEditor,

    /// The column count the composer last rendered at.
    ///
    /// The editor's vertical keys navigate the wrap grid this width
    /// defines; the event loop runs between frames, so the last
    /// rendered width is the geometry the user is looking at when a
    /// key lands.
    pub(crate) input_wrap_width: u16,

    /// What the last frame's composer pane looked like.
    ///
    /// A press inside the pane places the composer's caret where it
    /// landed — the pane rect to claim the press, the interior cell
    /// where the wrap grid begins, and the window of wrap rows the
    /// pane was showing when the user was looking at it.
    pub(crate) input_view: Option<InputView>,

    /// The composer caret's blink state.
    ///
    /// The app owns the caret's blink — the terminal's own
    /// blinking-cursor request is too often ignored or
    /// preference-gated to rely on — and this is the phase the frame
    /// renders from: the caret parks on its cell while visible and
    /// nowhere while dark. [`CaretBlink`] carries the cadence and the
    /// input-reset contract.
    pub(crate) caret_blink: CaretBlink,

    /// The composer's cached wrap, if it is still fresh.
    ///
    /// Valid for one buffer mutation at one width — see
    /// [`composer_wrap`](Self::composer_wrap).
    pub(crate) input_wrap_cache: Option<InputWrapCache>,
}

impl ComposerState {
    /// Build the composer with a fresh editor and no geometry.
    ///
    /// The editor starts empty with no history; the wrap width
    /// starts at a neutral standing default the first frame replaces
    /// with the measured one, the pane view stays unset until a frame
    /// records it, and no wrap is cached before one exists.
    pub(crate) fn new() -> Self {
        Self {
            input: InputEditor::new(),
            input_wrap_width: 80,
            input_view: None,
            caret_blink: CaretBlink::default(),
            input_wrap_cache: None,
        }
    }

    /// The frame's single wrap of the composer, reused across
    /// frames that do not touch it.
    ///
    /// A full-buffer wrap costs tens of milliseconds at the paste
    /// cap, and idle frames — the caret's blink, the tool spinner,
    /// a streaming delta — would otherwise pay it every time. The
    /// cache keys on the editor's mutation stamp and the wrap
    /// width, so only edits and resizes re-wrap; everything else
    /// clones the cached grid.
    pub(crate) fn composer_wrap(&mut self, width: u16) -> (Arc<Vec<String>>, (u16, u16)) {
        if let Some(cache) = &self.input_wrap_cache
            && cache.stamp == self.input.stamp()
            && cache.width == width
        {
            return (Arc::clone(&cache.rows), cache.caret);
        }
        let (rows, caret) = self.input.display_rows_and_caret(width);
        let caret = caret.unwrap_or((0, 0));
        let rows = Arc::new(rows);
        self.input_wrap_cache = Some(InputWrapCache {
            stamp: self.input.stamp(),
            width,
            rows: Arc::clone(&rows),
            caret,
        });
        (rows, caret)
    }

    /// Place the composer's caret where a press landed.
    ///
    /// Presses inside the composer pane claim the press — padding
    /// rows included, click-clamped the way editors clamp: the row
    /// clamps into the text window, and the column travels raw so
    /// the caret search clamps it to that row's own width. Returns
    /// whether the press belonged to the composer at all.
    pub(crate) fn place_input_caret(&mut self, column: u16, row: u16) -> bool {
        let Some(view) = self.input_view else {
            return false;
        };
        if column < view.pane.x
            || column >= view.pane.right()
            || row < view.pane.y
            || row >= view.pane.bottom()
        {
            return false;
        }
        let grid_row = view.start.saturating_add(
            usize::from(row.saturating_sub(view.origin.1)).min(view.visible.saturating_sub(1)),
        );
        let grid_column = usize::from(column.saturating_sub(view.origin.0));
        self.input
            .move_caret_to_cell(self.input_wrap_width, grid_row, grid_column);
        true
    }
}

/// Project a pasted block onto the one row a question draft holds.
///
/// A draft answers one question in one line, so a paste's line
/// breaks — whatever spelling the terminal delivered them in —
/// become the spaces between its parts, and every other control
/// character joins them for the same reason: the terminal has no
/// cell for a tab or a stray escape, and the spaces leave the text
/// readable and re-editable. Nothing is discarded; the wording
/// survives whole, only its breaks and invisible controls are
/// spelled as separators.
pub(crate) fn paste_for_one_line(text: &str) -> String {
    normalize_pasted_newlines(text)
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Spell a paste's line breaks the editor's way.
///
/// Terminals differ in how they relay newlines inside a bracketed
/// paste — some send line feeds, some carriage returns, some both.
/// The editor's line model is the line feed, and a carriage return
/// that survives as content welds the whole paste into one logical
/// line (the composer's window then shows only its tail) and later
/// reaches the terminal as a cell the renderer never meant to
/// draw. Both spellings fold here; every other character passes
/// through untouched.
pub(crate) fn normalize_pasted_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn a_pasted_block_becomes_one_line() {
        // An answer is one line, so the breaks and control characters a
        // pasted block carries arrive as the spaces between its parts.
        assert_eq!(paste_for_one_line("two\r\nlines"), "two lines");
        assert_eq!(paste_for_one_line("two\nlines"), "two lines");
        assert_eq!(paste_for_one_line("tab\there"), "tab here");
        assert_eq!(paste_for_one_line("plain"), "plain");
    }

    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::commands::Command;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn the_draft_wraps_a_written_answer_with_the_caret_in_it() {
        // The rows a written answer is drawn across, and the caret's place
        // among them, come from the composer's own wrap — the sheet asks the
        // same question the composer asks and gets the same answer.
        let (rows, caret) = crate::input::wrap_rows_and_caret("keep the reply short", 4, 10);
        assert_eq!(
            rows,
            vec!["keep the", "reply", "short"],
            "the draft breaks where the composer breaks the same text"
        );
        assert_eq!(
            caret,
            Some((0, 4)),
            "a caret after a word stands in that word's row, past it"
        );

        let (_rows, caret) = crate::input::wrap_rows_and_caret("keep the reply short", 12, 10);
        assert_eq!(
            caret,
            Some((1, 3)),
            "a caret inside a wrapped row counts cells from that row's own start"
        );
    }

    #[test]
    fn a_caret_after_the_last_character_opens_the_row_it_writes_on() {
        // A caret past the last character takes the next free cell, which is
        // a row of its own when the row it follows has no room left — the
        // cell a user is about to type into is always on screen.
        let (rows, caret) = crate::input::wrap_rows_and_caret("short", 5, 5);
        assert_eq!(rows, vec!["short", ""], "a full row hands the caret a row");
        assert_eq!(caret, Some((1, 0)));

        let (rows, caret) = crate::input::wrap_rows_and_caret("hi", 2, 10);
        assert_eq!(rows, vec!["hi"], "room left on the row means no extra row");
        assert_eq!(caret, Some((0, 2)));
    }

    #[test]
    fn arrows_over_the_command_list_write_the_composer() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.input.set_text("/".to_string());
        let (_, candidates, _) = app.fresh_completion("/");
        assert!(
            candidates.len() >= 3,
            "the command list offers several rows, got {candidates:?}"
        );
        let first = candidates.first().cloned().unwrap_or_default();
        let second = candidates.get(1).cloned().unwrap_or_default();
        let last = candidates.last().cloned().unwrap_or_default();
        assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert_eq!(
            app.composer.input.text(),
            format!("/{second}"),
            "Down applies the row it lands on — the composer follows the highlight"
        );
        assert!(
            app.completion_popup_live(),
            "the popup stands on the whole list while the arrows walk it"
        );
        assert!(app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(
            app.composer.input.text(),
            format!("/{first}"),
            "Up steps back and applies the row above"
        );
        assert!(app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(
            app.composer.input.text(),
            format!("/{last}"),
            "Up from the top wraps to the last row and applies it"
        );
    }

    #[test]
    fn the_caret_blinks_on_the_clock_not_the_tick_count() {
        let base = Instant::now();
        let mut blink = CaretBlink {
            anchor: base,
            on: true,
        };
        assert!(
            !blink.refresh(base + Duration::from_millis(100)),
            "inside the first interval the caret stays solid"
        );
        assert!(
            !blink.refresh(base + Duration::from_millis(529)),
            "a tick near the boundary still finds it on"
        );
        assert!(
            blink.refresh(base + Duration::from_millis(530)),
            "the interval boundary flips the caret off"
        );
        assert!(!blink.on, "the off phase holds between flips");
        assert!(
            blink.refresh(base + Duration::from_millis(1060)),
            "the second interval flips it back on"
        );
        // A long doze lands on the phase the clock says, not the one
        // the tick count would: 1600 ms is three intervals — odd, off.
        let mut dozed = CaretBlink {
            anchor: base,
            on: true,
        };
        assert!(
            dozed.refresh(base + Duration::from_millis(1600)),
            "three intervals in, the caret is dark whatever woke it"
        );
    }

    #[test]
    fn the_caret_inverts_the_glyph_under_it_while_visible() {
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.composer.input.set_text("zq".to_string());
        // Park the caret over the `q` so a glyph is under it.
        subject
            .composer
            .input
            .handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), 80);
        subject.composer.caret_blink.on = true;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("draw with the caret on");
        let inverted = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "q" && cell.bg == subject.theme.ui.primary)
            .expect("the glyph under the caret carries the accent");
        assert_eq!(
            inverted.fg, subject.theme.ui.background,
            "the glyph reads in the canvas color: inverted under the caret"
        );

        subject.composer.caret_blink.on = false;
        terminal
            .draw(|frame| subject.render(frame))
            .expect("draw with the caret off");
        let plain = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "q")
            .expect("the glyph is still on screen");
        assert_ne!(
            plain.bg, subject.theme.ui.primary,
            "the blink's dark phase restores the plain cell"
        );
    }

    #[test]
    fn a_static_caret_display_paints_through_the_dark_phase() {
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.config.display.caret_display = dch_config::CaretDisplay::Static;
        subject.composer.input.set_text("zq".to_string());
        subject.composer.caret_blink.on = false;
        assert!(
            subject.composer_caret_visible(),
            "a static caret ignores the beat's phase"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| subject.render(frame))
            .expect("draw in the dark phase");
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.bg == subject.theme.ui.primary),
            "the caret cell paints through the dark phase"
        );

        // The blinking default keeps the phase: the same dark tick
        // hides the caret.
        let mut beating = TuiApp::new(dch_config::DchConfig::default());
        beating.composer.caret_blink.on = false;
        assert!(
            !beating.composer_caret_visible(),
            "the blinking caret still hides in its dark phase"
        );
    }

    #[test]
    fn typing_resolidifies_a_dark_caret_and_mouse_traffic_does_not() {
        // The painted caret's resolidify contract, pinned at the seam
        // where the phase can be forced dark: a keystroke through the
        // input path brings the caret back at once — not at the next
        // phase boundary — and mouse traffic, which reads rather than
        // types, leaves the dark phase dark.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.caret_blink.on = false;
        assert!(
            app.handle_event(&crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE
            ))),
            "the editor takes the keystroke"
        );
        assert!(
            app.composer_caret_visible(),
            "keyboard input resolidifies the caret from its dark phase"
        );
        app.composer.caret_blink.on = false;
        let reading = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        let _ = app.handle_event(&reading);
        assert!(
            !app.composer_caret_visible(),
            "mouse traffic leaves the dark phase alone"
        );
    }

    #[test]
    fn path_style_re_spells_the_place_row_live() {
        // The place row reads the config's own spelling every frame,
        // so the promotion is classification alone — the row answers
        // a /set path_style the moment the config changes.
        let Some(home) = std::env::var("HOME").ok().filter(|home| !home.is_empty()) else {
            return;
        };
        let mut app = app();
        app.set_working_dir(std::path::Path::new(&home));
        assert_eq!(app.place_text(), home, "the default spells it in full");
        app.run_command(Command::Set {
            key: "path_style".to_string(),
            value: "relative".to_string(),
        });
        assert_eq!(
            app.place_text(),
            "~",
            "home itself re-spells as the bare tilde, live"
        );
    }
}
/// One caret blink phase.
///
/// Half a second on, half a second off — the cadence editors settle
/// on when they own the blink themselves. The terminal's own
/// blinking-cursor request may be ignored or preference-gated, so
/// the app owns the phase: what the user sees depends on neither the
/// terminal's cooperation nor the rate anything redraws — a stream
/// painting every frame and a fully idle lane alike flip at the
/// clock's cadence.
pub(crate) const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// The columns the compose row spends before its draft text: the cursor
/// marker and the pencil, one display column each.
///
/// The caret's cell is the row's start, plus this, plus the display width
/// of the text left of the caret — so the row and the caret share one
/// spelling of the prefix.
pub(crate) const COMPOSE_PREFIX: &str = "\u{203a} \u{270e} ";

/// The composer caret's blink state.
///
/// The terminal's blinking-cursor request is often ignored or
/// preference-gated, so the app owns the blink: half a second
/// visible, half a second dark. Keyboard input resolidifies the
/// caret and restarts the phase, so typing holds it solid and the
/// blink resumes after half a second of stillness; mouse traffic
/// reads, it does not type, and leaves the phase alone.
pub(crate) struct CaretBlink {
    /// Whether the caret is in its visible phase.
    ///
    /// The frame paints the caret cell only while this is true;
    /// the pending-permission and pending-question sheets suppress it.
    pub(crate) on: bool,

    /// When the current phase began — the clock the blink beats on.
    ///
    /// Set by input (the caret resolidifies) and read by
    /// [`refresh`](Self::refresh), which derives the phase from the
    /// time since it rather than from any tick count, so no redraw
    /// rate can speed the blink up or slow it down.
    pub(crate) anchor: Instant,
}

impl Default for CaretBlink {
    fn default() -> Self {
        Self {
            on: true,
            anchor: Instant::now(),
        }
    }
}

impl CaretBlink {
    /// Restart the visible phase — the reaction to any input.
    ///
    /// Keyboard input resolidifies the caret immediately; mouse
    /// traffic never calls this, so reading alone does not hold the
    /// caret solid.
    pub(crate) fn resolidify(&mut self) {
        self.anchor = Instant::now();
        self.on = true;
    }

    /// Bring the phase up to `now`, reporting whether it flipped —
    /// the only idle activity a blink generates.
    ///
    /// The phase is the clock's, not the tick count's: on through one
    /// [`CARET_BLINK_INTERVAL`] since the anchor, off through the
    /// next, alternating — so a burst of ticks between flips (a
    /// stream painting every frame) and a dozing lane (a fully idle
    /// session) both find the caret at the same steady cadence.
    pub(crate) fn refresh(&mut self, now: Instant) -> bool {
        let intervals = now
            .duration_since(self.anchor)
            .as_millis()
            .div_euclid(CARET_BLINK_INTERVAL.as_millis());
        let on = matches!(intervals.checked_rem(2), Some(0));
        let flipped = on != self.on;
        self.on = on;
        flipped
    }
}

/// What the last frame's composer pane looked like.
///
/// The bridge between a press (screen cells, between frames) and the
/// composer's wrap grid: which pane rect belongs to the composer,
/// which interior cell holds the grid's first row and column, and
/// which slice of the grid the pane's text window was showing.
#[derive(Clone, Copy)]
pub(crate) struct InputView {
    /// The composer pane's rect — presses inside belong to it.
    ///
    /// The press-routing claim: a press inside this rect places the
    /// caret, everything outside it falls to the conversation's
    /// selection logic.
    pub(crate) pane: Rect,

    /// The interior cell where the wrap grid's row 0, column 0 sits.
    ///
    /// Press coordinates become grid coordinates by subtracting
    /// this, net of the pane's own side inset.
    pub(crate) origin: (u16, u16),

    /// The wrap-grid row the text window's top row shows.
    ///
    /// The window follows the caret's line; this records which
    /// slice was on screen when the press landed.
    pub(crate) start: usize,

    /// How many wrap rows the text window shows.
    ///
    /// A press below the window's last row clamps into it — the
    /// way editors clamp a click below a document's end.
    pub(crate) visible: usize,
}

/// Columns of tinted breathing room at each end of the input field's
/// text, inside the fill's edge.
pub(crate) const INPUT_SIDE_INSET: u16 = 2;

/// Rows of text the input field shows at its largest.
///
/// The field starts as a single line and grows one row per line of
/// buffer — typed or wrapped — up to this cap; past it the text
/// window scrolls with the caret and the conversation pane above
/// holds still again.
pub(crate) const INPUT_TEXT_ROWS: u16 = 3;

/// Blank tinted rows of breathing room above and below the field's
/// text rows — the vertical half of the composer's inner padding.
pub(crate) const INPUT_VERTICAL_PADDING: u16 = 1;

/// Spell the working directory per the configured style: the full
/// path as the host named it, or the `~/`-relative shape when the
/// directory sits under the user's home — falling back to the full
/// path outside home or when the environment names no home, so the
/// row never shows a bare unanchored fragment.
pub(crate) fn display_path(dir: &str, style: PathStyle) -> String {
    match style {
        PathStyle::Full => dir.to_string(),
        PathStyle::Relative => std::env::var("HOME")
            .ok()
            .filter(|home| !home.is_empty())
            .and_then(|home| relativize(Path::new(dir), Path::new(&home)))
            .unwrap_or_else(|| dir.to_string()),
    }
}

/// The `~/`-spelling of `dir` under `home`, or `None` when the
/// directory sits outside home.
///
/// The home directory itself is the bare `~`.
pub(crate) fn relativize(dir: &Path, home: &Path) -> Option<String> {
    if dir == home {
        Some("~".to_string())
    } else {
        dir.strip_prefix(home)
            .ok()
            .map(|rest| format!("~/{}", rest.display()))
    }
}

/// Copy `word` with its first character uppercased.
///
/// The composer identity row's effort label shows the configured
/// level's own spelling with a leading capital — `low` renders
/// `(Low)` — whatever case the config file carried; the rest of the
/// word is untouched.
pub(crate) fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}
