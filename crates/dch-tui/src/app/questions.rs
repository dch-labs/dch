//! The question sheet: the pending prompts' answer state, the
//! sheet's row composition, and the key handling that walks, marks,
//! and answers them.

use super::TuiApp;
use super::composer::COMPOSE_PREFIX;
use super::composer::paste_for_one_line;
use super::keys::key_survives_the_sheet;
use super::notices::wrap_prompt_rows;
use super::permissions::MAX_OVERLAY_ROWS;
use crate::cells::{for_display, str_width};
use crate::question::QuestionPrompt;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Line;

impl TuiApp {
    /// Install the channel question prompts arrive on.
    ///
    /// The receiver half of the bridge the host builds around the
    /// runner's question channel; once set, pending questions render
    /// as an overlay and the run loop claims the channel into its
    /// select.
    pub fn set_question_requests(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>,
    ) {
        self.questions.question_rx = Some(rx);
    }

    /// Land one delivered question and any others already waiting
    /// behind it.
    ///
    /// The prompt enters the queue and the receiver drains, so a
    /// multi-question batch lands together and the first frame's
    /// overlay reports the true waiting count. The integration suites
    /// stage their batches through here; the run loop lands its own
    /// the same way, through the question state and the receiver it
    /// keeps in a local.
    pub fn land_question_requests(
        &mut self,
        first: QuestionPrompt,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>,
    ) {
        self.questions.land_question_requests(first, rx);
    }

    /// Refresh the question queue without the run loop.
    ///
    /// The test seam the integration suites drive to stage questions:
    /// what arrived lands and what nobody asks any longer drops, both
    /// in the question state's own pass
    /// (`QuestionState::poll_question_requests`). The run loop lands
    /// its batches through the question state instead, from the
    /// receiver it keeps in a local — once `run` has claimed the
    /// channel, only the withdrawal half has anything left to say.
    pub fn poll_question_requests(&mut self) {
        self.questions.poll_question_requests();
    }

    /// Apply a key press to the question on screen.
    ///
    /// `Some(redraw)` when a question is pending — the overlay owns the
    /// keyboard, and which half of it answers depends on whether the
    /// draft is being edited. Over the rows, Up and Down move the cursor,
    /// Left and Right walk the queued questions (the walk moves the view
    /// and not the queue, so the question on screen is the one Enter
    /// answers and Esc dismisses the one question the walk left it on —
    /// never the batch behind it), Space marks
    /// the row it stands on — the compose row included — without ever
    /// answering, and Enter confirms: the marked rows, or the cursor row
    /// where nothing is marked. On the compose row Enter opens the draft,
    /// and a marked compose row still empty opens it too rather than
    /// answering without its text — as does typing a letter anywhere in
    /// the
    /// sheet, which opens the draft with that letter in it. Inside the
    /// draft the arrows move its caret and Home and
    /// End jump its ends — those keys belong to the text being typed, not
    /// to the queue — printable keys build it, Enter sends it, and Esc
    /// steps back to the rows with the text kept. Every other key is
    /// swallowed; `None` for the two `c` chords and the keys
    /// [`key_survives_the_sheet`] names — the machinery's own chords, and
    /// the paging and verbosity that read the conversation the sheet is
    /// parked over.
    pub(crate) fn question_key(&mut self, key: KeyEvent) -> Option<bool> {
        self.questions.shown_question()?;
        if key_survives_the_sheet(key) {
            return None;
        }
        let bare = key.modifiers == KeyModifiers::NONE;
        let redraw = if self
            .questions
            .shown_question()
            .is_some_and(|entry| entry.ui.composing)
        {
            self.question_draft_key(key, bare)
        } else {
            self.question_rows_key(key, bare)
        };
        Some(redraw)
    }

    /// The keys the compose draft answers, on the question on screen.
    ///
    /// The draft owns these while it is open: printable characters land
    /// at its caret, the arrows and Home/End move that caret, Enter sends
    /// the text as the answer the options never offered, and Esc steps
    /// back to the rows keeping the text. Editing follows the composer's
    /// line-editor conventions wherever the surface can honour them:
    /// `Delete` erases to the right, Ctrl with an arrow walks a whole
    /// word, and Ctrl+W and Ctrl+U erase a word and the line's head.
    /// Enter means send whatever modifier it arrives under — the draft
    /// holds one line, so a terminal that spells `Shift+Enter` as its own
    /// event is not asking for a newline the answer cannot carry. A draft
    /// with nothing in it is not an answer and does not go; it says so on
    /// the sheet's own hint row, which is where the key promised
    /// otherwise. A letter counts whether it is shifted, because that is
    /// how a capital arrives; only Ctrl takes a key off its letter.
    /// Everything else is swallowed.
    fn question_draft_key(&mut self, key: KeyEvent, bare: bool) -> bool {
        let mut send = false;
        if let Some(entry) = self.questions.shown_question_mut() {
            let ui = &mut entry.ui;
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let letter = bare || key.modifiers == KeyModifiers::SHIFT;
            match key.code {
                KeyCode::Enter => {
                    send = !ui.draft.trim().is_empty();
                }
                KeyCode::Esc => ui.composing = false,
                KeyCode::Left if ctrl => ui.caret_word_left(),
                KeyCode::Left if bare => ui.caret_left(),
                KeyCode::Right if ctrl => ui.caret_word_right(),
                KeyCode::Right if bare => ui.caret_right(),
                KeyCode::Home if bare => ui.caret_home(),
                KeyCode::End if bare => ui.caret_end(),
                KeyCode::Backspace | KeyCode::Char('w') if ctrl => ui.delete_word_left(),
                KeyCode::Backspace => ui.backspace(),
                KeyCode::Delete => ui.delete_forward(),
                KeyCode::Char('u') if ctrl => ui.delete_to_start(),
                KeyCode::Char(ch) if letter => ui.type_char(ch),
                _ => {}
            }
        }
        if send {
            let answers = self.questions.shown_answer(true);
            self.questions.resolve_pending_question(answers);
        }
        true
    }

    /// The keys the option rows answer, on the question on screen.
    ///
    /// The rows' half of the overlay: navigation, marking, walking the
    /// queue, confirming, dismissing. Marking is Space's whole job, on the
    /// compose row as on an option — it adds or removes the row it stands
    /// on for a multi-select, and moves the one mark there for a
    /// single-select — and it never answers, so a stray press cannot send
    /// a reply. Enter answers with the marked rows (the marked row, or the
    /// cursor row where nothing is marked), a marked draft answering with
    /// its own text, and the compose row answering with the words it
    /// holds. A letter typed here starts a draft, shifted or not, since a
    /// capital is the letter with Shift held.
    fn question_rows_key(&mut self, key: KeyEvent, bare: bool) -> bool {
        let Some(entry) = self.questions.shown_question() else {
            return true;
        };
        let free_row = entry.prompt.options.len();
        let multi = entry.prompt.multi_select;
        let cursor = entry.ui.cursor;
        match key.code {
            KeyCode::Up if bare => {
                if let Some(entry) = self.questions.shown_question_mut() {
                    entry.ui.cursor = entry.ui.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down if bare => {
                if let Some(entry) = self.questions.shown_question_mut() {
                    entry.ui.cursor = (entry.ui.cursor.saturating_add(1)).min(free_row);
                }
            }
            KeyCode::Right if bare => self.questions.step_question_view(true),
            KeyCode::Left if bare => self.questions.step_question_view(false),
            KeyCode::Char(' ') if bare => {
                if let Some(entry) = self.questions.shown_question_mut() {
                    let ui = &mut entry.ui;
                    let marked = ui.toggled.contains(&cursor);
                    if multi {
                        if marked {
                            ui.toggled.remove(&cursor);
                        } else {
                            ui.toggled.insert(cursor);
                        }
                    } else {
                        ui.toggled.clear();
                        if !marked {
                            ui.toggled.insert(cursor);
                        }
                    }
                }
            }
            KeyCode::Enter if bare => {
                if self.questions.needs_the_draft(cursor == free_row) {
                    if let Some(entry) = self.questions.shown_question_mut() {
                        entry.ui.open_composing();
                    }
                    return true;
                }
                let answers = self.questions.shown_answer(false);
                self.questions.resolve_pending_question(answers);
            }
            KeyCode::Esc if bare => {
                self.questions.resolve_pending_question(Vec::new());
            }
            KeyCode::Char(ch) if bare || key.modifiers == KeyModifiers::SHIFT => {
                if let Some(entry) = self.questions.shown_question_mut() {
                    entry.ui.open_composing();
                    entry.ui.type_char(ch);
                }
            }
            _ => return false,
        }
        true
    }

    /// The question sheet's rows and the caret riding them, laid out for
    /// the width the box has and the height the terminal leaves it.
    ///
    /// Every consumer of the sheet's shape — the render and the height
    /// the frame reserves for it — builds through this one walk, so the
    /// box the user sees and the rows the layout holds aside can never
    /// drift apart.
    ///
    /// `budget` is the rows the sheet may occupy inside its borders, and
    /// both consumers hand it the same number. When the terminal cannot
    /// hold everything, the rows go where the answer is made: the draft
    /// row keeps its own row whatever happens, because a sheet that cut
    /// the row being typed would leave the user writing blind. The
    /// option list gives way first — windowed around the row the cursor
    /// stands on, so the walk stays on screen and the arrows walk the
    /// window alongside it — then a draft that outgrows the rows left
    /// for it takes its own window around the caret, and only a sheet too
    /// short to hold both the draft and the key hints gives the hints'
    /// rows up to the caret: the tail is the last to go. See
    /// [`fit_question_sheet`].
    pub(crate) fn question_sheet(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
        more: usize,
        budget: usize,
    ) -> QuestionSheet {
        let head = self.question_sheet_head(entry, wrap_at);
        let options = self.question_sheet_options(entry, wrap_at);
        let (compose, caret_in_compose) = self.question_sheet_compose(entry, wrap_at);
        let tail = self.question_sheet_tail(entry, wrap_at, more);
        let cursor_option = entry.ui.cursor.min(entry.prompt.options.len());
        let (lines, compose_start, caret_in_window) = fit_question_sheet(
            head,
            &options,
            compose,
            tail,
            cursor_option,
            budget,
            caret_in_compose,
        );
        let caret = caret_in_window
            .map(|(row, column)| (compose_start.saturating_add(row), column))
            .filter(|(row, _)| *row < lines.len());
        QuestionSheet { lines, caret }
    }

    /// The sheet's opening block: the header chip, the question's own
    /// text, and the row of air over the options.
    ///
    /// The question wraps at the box's inner width and clamps to
    /// [`MAX_OVERLAY_ROWS`] rows, the last ellipsized, so a paragraph of a
    /// question cannot push the options off a short terminal on its own.
    /// It is the one text block the sheet never re-wraps: what was asked
    /// is what the user has to read to answer.
    fn question_sheet_head(&self, entry: &PendingQuestion, wrap_at: usize) -> Vec<Line<'static>> {
        let question = &entry.prompt;
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(header) = &question.header {
            lines.push(Line::styled(
                format!("[{}]", for_display(header)),
                Style::default().fg(self.theme.ui.secondary),
            ));
        }
        lines.extend(
            wrap_prompt_rows(&for_display(&question.question), wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.foreground))),
        );
        lines.push(Line::default());
        lines
    }

    /// The sheet's option block: one entry per option, holding its label
    /// row and, where it has one, its description row beneath it.
    ///
    /// Grouped rather than flattened because a short terminal drops
    /// options whole: a description left standing apart from the label it
    /// describes reads as another option's. The grouping is also what lets
    /// the sheet keep the cursor's own option visible — the walk that
    /// chooses which options survive is anchored on it.
    ///
    /// Labels and descriptions clamp to one row each, ellipsized, so the
    /// pickable surface stays scannable whatever the model asked. The
    /// indent is applied *outside* `wrap_prompt_rows`: that wrapper splits
    /// on spaces, so a leading run of them is dropped as empty words and
    /// the row lands flush-left instead.
    fn question_sheet_options(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
    ) -> Vec<(usize, Vec<Line<'static>>)> {
        let question = &entry.prompt;
        let ui = &entry.ui;
        let draft_supersedes =
            ui.composing && !question.multi_select && !ui.draft.trim().is_empty();
        question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let marker = if index == ui.cursor && !ui.composing {
                    "\u{203a} "
                } else {
                    "  "
                };
                let checked = if ui.toggled.contains(&index) && !draft_supersedes {
                    "\u{2022} "
                } else {
                    "  "
                };
                let label = wrap_prompt_rows(
                    &for_display(&format!("{marker}{checked}{}", option.label)),
                    wrap_at,
                    1,
                )
                .first()
                .cloned()
                .unwrap_or_default();
                let style = if index == ui.cursor {
                    Style::default().fg(self.theme.ui.primary)
                } else {
                    Style::default().fg(self.theme.ui.foreground)
                };
                let mut rows = vec![Line::styled(label, style)];
                if let Some(description) = &option.description {
                    let detail = wrap_prompt_rows(
                        &for_display(description),
                        wrap_at.saturating_sub(str_width(DETAIL_INDENT)),
                        1,
                    )
                    .first()
                    .cloned()
                    .unwrap_or_default();
                    rows.push(Line::styled(
                        format!("{DETAIL_INDENT}{detail}"),
                        Style::default().fg(self.theme.ui.secondary),
                    ));
                }
                (index, rows)
            })
            .collect()
    }

    /// The sheet's compose block: the row the answer is written on, with
    /// the draft laid across as many rows as the box's width asks for, and
    /// the caret's place in that layout while the draft owns the keys.
    ///
    /// The draft wraps with the composer's own wrap rather than a second
    /// one of the sheet's devising, so the two editable surfaces in the
    /// app break text at the same points and an answer longer than the box
    /// — a path, a UUID, a sentence — continues on a row the user can see
    /// instead of running past the border into nothing. The caret's row
    /// comes out of that wrap too, which is the only place that knows
    /// which row the caret's character landed on.
    ///
    /// The draft renders exactly as typed, the display policy aside:
    /// its rows and the caret's column come from one wrap, and
    /// projecting the text through the policy would move painted
    /// cells off the math the caret rides. The block repaints on
    /// every keystroke, so a cluster the terminal sizes differently
    /// cannot outlive the next press.
    ///
    /// Outside the draft the block is the row at rest plus the subtext
    /// saying what it is for: the invitation, or the words already typed,
    /// which Esc leaves in place precisely because stepping out of the
    /// mode is not a request to lose them.
    fn question_sheet_compose(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let ui = &entry.ui;
        let mut lines: Vec<Line<'static>> = vec![Line::default()];
        if ui.composing {
            let draft_width =
                u16::try_from(wrap_at.saturating_sub(str_width(COMPOSE_PREFIX)).max(1))
                    .unwrap_or(u16::MAX);
            let caret_byte = byte_index(&ui.draft, ui.caret);
            let (rows, caret) =
                crate::input::wrap_rows_and_caret(&ui.draft, caret_byte, draft_width);
            let first = lines.len();
            let continuation = " ".repeat(str_width(COMPOSE_PREFIX));
            for (index, row) in rows.iter().enumerate() {
                let prefix = if index == 0 {
                    COMPOSE_PREFIX
                } else {
                    continuation.as_str()
                };
                lines.push(Line::styled(
                    format!("{prefix}{row}"),
                    Style::default().fg(self.theme.ui.primary),
                ));
            }
            let (caret_row, caret_column) = caret.unwrap_or((0, 0));
            return (
                lines,
                Some((
                    first.saturating_add(usize::from(caret_row)),
                    usize::from(caret_column),
                )),
            );
        }
        let free_row = entry.prompt.options.len();
        let marker = if ui.cursor == free_row {
            "\u{203a} "
        } else {
            "  "
        };
        let checked = if ui.toggled.contains(&free_row) {
            "\u{2022} "
        } else {
            "  "
        };
        let has_draft = !ui.draft.trim().is_empty();
        let body = if has_draft {
            ui.draft.as_str()
        } else {
            "Type your answer"
        };
        lines.push(Line::styled(
            format!("{marker}{checked}\u{270e} {body}"),
            if ui.cursor == free_row {
                Style::default().fg(self.theme.ui.primary)
            } else {
                Style::default().fg(self.theme.ui.foreground)
            },
        ));
        let subtext = if has_draft {
            "enter sends it \u{b7} typing reopens the draft"
        } else {
            "a reply the options did not offer"
        };
        lines.push(Line::styled(
            format!("{DETAIL_INDENT}{subtext}"),
            Style::default().fg(self.theme.ui.secondary),
        ));
        (lines, None)
    }

    /// The sheet's closing block: the row of air under the compose row and
    /// the keys explained.
    ///
    /// The hints never give up their rows to the terminal, which is the
    /// other half of why they lead with the keys that end the exchange:
    /// what a short terminal cuts is the tail of the list, not the list.
    fn question_sheet_tail(
        &self,
        entry: &PendingQuestion,
        wrap_at: usize,
        more: usize,
    ) -> Vec<Line<'static>> {
        let ui = &entry.ui;
        let mut lines: Vec<Line<'static>> = vec![Line::default()];
        let toggle = if entry.prompt.multi_select {
            "[space] toggle"
        } else {
            "[space] pick"
        };
        let parts: Vec<&str> = if ui.composing {
            let enter = if ui.draft.trim().is_empty() {
                "[enter] needs a written answer"
            } else {
                "[enter] send"
            };
            vec![
                "[esc] back",
                enter,
                "[home/end] ends",
                "[\u{2190}\u{2192}] caret",
            ]
        } else if more == 0 {
            vec![
                "[esc] dismiss",
                "[enter] confirm",
                "[\u{2191}\u{2193}] move",
                toggle,
            ]
        } else {
            vec![
                "[esc] dismiss one",
                "[enter] confirm",
                "[\u{2191}\u{2193}] move",
                "[\u{2190}\u{2192}] question",
                toggle,
            ]
        };
        let hints = parts.join(" \u{b7} ");
        lines.extend(
            wrap_prompt_rows(&hints, wrap_at, MAX_OVERLAY_ROWS)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(self.theme.ui.secondary))),
        );
        lines
    }

    /// The rows the pending question's sheet occupies, borders included.
    ///
    /// The frame reserves this many rows above its bottom furniture —
    /// whatever of the sheet the furniture does not already cover —
    /// so the conversation's rows end above the box instead of
    /// rendering behind it. A terminal shorter than the sheet fits the
    /// sheet to it, rows chosen as [`fit_question_sheet`] describes,
    /// rather than hanging rows the frame has nowhere to draw.
    ///
    /// `more` is the count the sheet itself renders beside the hints.
    /// Passing anything else desynchronizes the two by exactly the rows
    /// the queue segment adds once the hints wrap — the box grows past
    /// the rows held aside and covers the conversation's tail it was
    /// reserved to uncover.
    #[must_use]
    pub(crate) fn question_sheet_height(
        &self,
        width: u16,
        sheet_area_height: u16,
        more: usize,
    ) -> u16 {
        let Some(entry) = self.questions.shown_question() else {
            return 0;
        };
        let wrap_at = usize::from(width.max(4).saturating_sub(4));
        let budget = usize::from(sheet_area_height.saturating_sub(2));
        let sheet = self.question_sheet(entry, wrap_at, more, budget);
        u16::try_from(sheet.lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(sheet_area_height)
    }
}

/// The question sheet's state: the prompts waiting, the one on
/// screen, and the channel they arrive on.
///
/// The batch the asking tool sent, the walk's index into it, and the
/// receiver the run loop claims — everything the sheet keeps between
/// frames. Each queued question carries its own answer-in-progress,
/// so the batch behaves like a form; see [`PendingQuestion`].
#[derive(Default)]
pub(crate) struct QuestionState {
    /// Questions from the asking tool waiting for the user's answer,
    /// oldest first.
    ///
    /// The host converts the tool's batched requests into prompts and
    /// lands them here; the front of the queue is what the overlay
    /// shows and what the keys answer. Answering sends on the
    /// prompt's one-shot and advances to the next question of the
    /// batch. A finished or cancelled run empties the queue — the
    /// dispatches are gone, and dropping the prompts closes their
    /// reply channels, which the asking tools read as dismissals.
    ///
    /// Each entry carries the answer being composed for its own
    /// question, so walking the batch with Left and Right changes only
    /// which question is shown — nothing already marked or typed is
    /// left behind on the way.
    pub(crate) pending_questions: std::collections::VecDeque<PendingQuestion>,

    /// Which queued question is on screen: an index into
    /// [`pending_questions`](Self::pending_questions).
    ///
    /// The walk moves this and leaves the queue's arrival order alone, so
    /// a question keeps its number in the sheet's title while the user
    /// walks away from it and back, and the count names the batch the
    /// asking tool sent. Always a live index: landing only lengthens the
    /// queue, and answering lifts the shown entry out with the next one
    /// sliding into its place.
    pub(crate) question_view: usize,

    /// The channel question prompts arrive on, until `run` claims
    /// it.
    ///
    /// Installed by the host's bridge before the run loop starts; the
    /// loop's select arm moves prompts into the queue, mirroring the
    /// permission channel's shape.
    pub(crate) question_rx: Option<tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>>,
}

impl QuestionState {
    /// The question on screen: the one the sheet renders and the keys
    /// answer.
    ///
    /// Read through the view index rather than the queue's front, which
    /// is what lets a batch hold its arrival order — the count the title
    /// prints — while the user walks it: the third question of four is
    /// what this returns after two presses of Right, not the one that
    /// arrived third. `None` is a closed sheet, an empty queue, which
    /// every question path faces the moment the last answer is sent or
    /// the last prompt withdrawn.
    pub(crate) fn shown_question(&self) -> Option<&PendingQuestion> {
        self.pending_questions.get(self.question_view)
    }

    /// The question on screen, for the keys that move its cursor, mark a
    /// row, or compose a draft on it.
    ///
    /// The same index the render reads, so a key can only ever reach the
    /// question the user is looking at, and walking the queue leaves every
    /// other answer-in-progress exactly as it was. `None` means no sheet
    /// is open: a key that arrives in that moment has nothing to act on
    /// and does nothing, rather than borrowing the next question to
    /// arrive.
    fn shown_question_mut(&mut self) -> Option<&mut PendingQuestion> {
        self.pending_questions.get_mut(self.question_view)
    }

    /// Walk the shown question one step, wrapping at both ends.
    ///
    /// The walk moves the view and leaves the queue's arrival order
    /// alone, because that order is what the sheet counts: walking to the
    /// third question of four says `3 of 4`, and says it again after a
    /// walk away and back. It is a circle in both directions, so the
    /// question answered last sits one press from the one answered first.
    ///
    /// A batch of one has nowhere to go, and the step declines rather
    /// than reports: the key that asked for it is still swallowed, which
    /// keeps an arrow from falling through to the transcript behind the
    /// sheet.
    fn step_question_view(&mut self, forward: bool) {
        let count = self.pending_questions.len();
        if count < 2 {
            return;
        }
        self.question_view = if forward {
            let next = self.question_view.saturating_add(1);
            if next >= count { 0 } else { next }
        } else if self.question_view == 0 {
            count.saturating_sub(1)
        } else {
            self.question_view.saturating_sub(1)
        };
    }

    /// Keep [`question_view`](Self::question_view) a live index into the
    /// queue.
    ///
    /// Answering or withdrawing a question can shrink the queue under a
    /// user standing at its end, and an index past the last row is a
    /// sheet showing nothing rather than a sheet showing the next
    /// question. So the view is pulled back to the last question left; an
    /// emptied queue takes it home to zero, which is also where the next
    /// batch is met.
    pub(crate) fn clamp_question_view(&mut self) {
        self.question_view = self
            .question_view
            .min(self.pending_questions.len().saturating_sub(1));
    }

    /// Take off the queue every question the asking side has stopped
    /// waiting for.
    ///
    /// A prompt's one-shot is the asker's stake in its question: the
    /// receiver lives exactly as long as the wait does, and ends with it.
    /// A batch's wait ends early on its first dismissal — one question
    /// coming back unanswered is the whole ask's answer to the model —
    /// and the questions behind it drop their receivers unheard, as they
    /// do when a dispatch is cancelled or the asking channel closes
    /// mid-wait. Nothing else says so: without this the user keeps
    /// looking at a question nobody will ever read, over the reply that
    /// question never waited for. A closed reply channel is the whole
    /// notice, and this is where it is read.
    ///
    /// This is the running half of a question's retirement: the tick
    /// already drops the whole queue when the run itself is gone, while
    /// this catches the single ask abandoned *inside* a live run — the
    /// model goes on asking and answering other things, and only these
    /// few questions stop being anyone's.
    ///
    /// The view follows the questions rather than their indices: it steps
    /// back by the ones withdrawn ahead of it, so the question the walk
    /// arrived at stays on screen for as long as it survives.
    pub(crate) fn retire_withdrawn_questions(&mut self) -> bool {
        let view = self.question_view;
        let before = self.pending_questions.len();
        let mut withdrawn_ahead = 0usize;
        let mut scanned = 0usize;
        self.pending_questions.retain(|entry| {
            let closed = entry.prompt.reply.is_closed();
            if closed && scanned < view {
                withdrawn_ahead = withdrawn_ahead.saturating_add(1);
            }
            scanned = scanned.saturating_add(1);
            !closed
        });
        if before == self.pending_questions.len() {
            return false;
        }
        self.question_view = view.saturating_sub(withdrawn_ahead);
        self.clamp_question_view();
        true
    }

    /// Answer the question on screen, wherever the walk left it.
    ///
    /// Sends `answers` on its one-shot and lifts it out of the queue —
    /// its cursor, marks, and draft going with it, so the question
    /// behind it is still clean rather than wearing the last one's
    /// answer. A send that finds no receiver (a dispatch already
    /// cancelled away) is silently fine — the asking tool read its
    /// dismissal when its future dropped.
    fn resolve_pending_question(&mut self, answers: Vec<String>) {
        let shown = self.question_view;
        if let Some(entry) = self.pending_questions.remove(shown)
            && entry.prompt.reply.send(answers).is_err()
        {
            tracing::trace!("question reply channel already closed");
        }
        self.clamp_question_view();
    }

    /// What the shown question's marks and its draft add up to, as the
    /// answer Enter sends.
    ///
    /// Marked option rows answer in the order the sheet shows them and a
    /// draft answers last, from the row below every option. A single
    /// select answers one thing: the marked row — or the draft, where the
    /// draft is what the user chose to write — and with nothing marked at
    /// all, the row the cursor stands on. A multi-select asks the same
    /// question of its rows and, when none of them is marked, takes the
    /// cursor's row for its own: an empty reply is exactly what a
    /// dismissal sends, so Enter on a row the user walked onto cannot
    /// answer with nothing. Whichever row the cursor stands on is the row
    /// Enter confirms, the compose row included — there its words are its
    /// content, and a draft kept out of compose answers from no other row
    /// but that one.
    ///
    /// `draft_counts` is whether the typed text belongs to the answer on
    /// its own rather than only where its row is marked: the draft's own
    /// Enter says yes, because writing a reply *is* choosing it, while
    /// Enter over the rows asks the mark. Both paths come here, so a
    /// marked draft answers the same however it was reached and composing
    /// never silently drops the rows marked beside it.
    fn shown_answer(&self, draft_counts: bool) -> Vec<String> {
        let Some(entry) = self.shown_question() else {
            return Vec::new();
        };
        let ui = &entry.ui;
        let free_row = entry.prompt.options.len();
        let draft = ui.draft.trim().to_string();
        let draft_wanted = !draft.is_empty()
            && (draft_counts || ui.toggled.contains(&free_row) || ui.cursor == free_row);
        if entry.prompt.multi_select {
            let mut answers = entry
                .prompt
                .options
                .iter()
                .enumerate()
                .filter(|(index, _)| *index < free_row && ui.toggled.contains(index))
                .map(|(_, option)| option.label.clone())
                .collect::<Vec<String>>();
            if draft_wanted {
                answers.push(draft);
            }
            if answers.is_empty() {
                answers.extend(
                    entry
                        .prompt
                        .options
                        .get(ui.cursor)
                        .map(|option| option.label.clone()),
                );
            }
            answers
        } else if draft_wanted {
            vec![draft]
        } else {
            ui.toggled
                .iter()
                .copied()
                .find(|index| *index < free_row)
                .and_then(|index| entry.prompt.options.get(index))
                .or_else(|| entry.prompt.options.get(ui.cursor))
                .map_or_default(|option| vec![option.label.clone()])
        }
    }

    /// Whether Enter here means go and write a reply rather than answer.
    ///
    /// An empty compose row cannot be sent, so Enter opens the draft to
    /// fill it — whenever the row is the answer the user is pointing at
    /// (`on_compose_row`, where an unmarked compose row is exactly that
    /// invitation) or when it is the only mark on the sheet. Marks on real
    /// options beside it settle the matter the other way: the answer is
    /// chosen, and the empty row contributes nothing to it.
    fn needs_the_draft(&self, on_compose_row: bool) -> bool {
        self.shown_question().is_some_and(|entry| {
            let free_row = entry.prompt.options.len();
            let marks = entry.ui.toggled.len();
            let options_marked = entry
                .ui
                .toggled
                .iter()
                .filter(|index| **index < free_row)
                .count();
            entry.ui.draft.trim().is_empty()
                && if on_compose_row || entry.ui.toggled.contains(&free_row) {
                    options_marked == 0 && (on_compose_row || marks == 1)
                } else {
                    false
                }
        })
    }

    /// Land a paste on the question on screen: into the open draft at its
    /// caret, or into a draft opened for it.
    ///
    /// A paste is a burst of keystrokes, and the overlay answers
    /// keystrokes — a letter typed at the rows opens the draft with that
    /// letter in it, so a paste arrives the same way rather than vanishing
    /// without a word. Dropping it would lose the likeliest thing a user
    /// has to say that the options never offered: a path, a hash, the line
    /// copied out of the conversation above the box.
    ///
    /// The text lands as one line whatever it held. A draft answers one
    /// question in one row, so the breaks a multi-line paste carries
    /// become the spaces between its parts; nothing is discarded.
    pub(crate) fn paste_into_the_question(&mut self, text: &str) -> bool {
        let Some(entry) = self.shown_question_mut() else {
            return true;
        };
        if !entry.ui.composing {
            entry.ui.open_composing();
        }
        entry.ui.insert_str(&paste_for_one_line(text));
        true
    }

    /// Refresh the queue: land what arrived, drop what nobody asks.
    ///
    /// Landing and withdrawal are one pass because they are the same
    /// question asked twice — is this prompt still live? — and the run
    /// loop does both on its own beat. The test seam for staging
    /// questions without the loop: production keeps its receiver in a
    /// local and lands batches through
    /// [`land_question_requests`](Self::land_question_requests) instead —
    /// once `run` claims the channel, this slot is empty and only the
    /// withdrawal half has anything left to say.
    pub(crate) fn poll_question_requests(&mut self) {
        while let Some(prompt) = self.question_rx.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.pending_questions
                .push_back(PendingQuestion::new(prompt));
        }
        self.retire_withdrawn_questions();
    }

    /// Land one delivered question and any others already waiting
    /// behind it.
    ///
    /// The run loop's select arm hands the prompt its wakeup
    /// delivered plus the receiver it owns: the prompt enters the
    /// queue and the receiver drains, so a multi-question batch lands
    /// together and the first frame's overlay reports the true
    /// waiting count.
    pub(crate) fn land_question_requests(
        &mut self,
        first: QuestionPrompt,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<QuestionPrompt>,
    ) {
        self.pending_questions
            .push_back(PendingQuestion::new(first));
        while let Ok(prompt) = rx.try_recv() {
            self.pending_questions
                .push_back(PendingQuestion::new(prompt));
        }
    }
}

/// One question's answer-in-progress: its cursor, its marks, and its
/// draft.
///
/// Everything about answering a question that is not the question
/// itself. It lives on the queued question — [`PendingQuestion`] —
/// rather than on the app, so walking a batch with Left and Right sets
/// an unfinished answer aside and finds it waiting on the return visit;
/// a question answers or retires, and its state leaves with it.
///
/// The answer has two modes and the keys split between them. Over the
/// rows, Space marks the row under the cursor and Enter confirms; in the
/// draft, the arrows and Home and End belong to the text and Enter sends
/// it. [`composing`](Self::composing) is which mode the question stands
/// in, and flipping it changes no answer itself — the marks stay, the
/// words stay, and only the keys' meaning moves. That is why the draft
/// can be stepped out of and back into without losing anything: the mode
/// is a layer over the answer, not the answer.
#[derive(Debug, Default)]
struct QuestionUi {
    /// The cursor row over the option rows plus the compose row.
    ///
    /// Indexes into the question's options, with one past the last
    /// naming the compose row. Enter confirms this row when nothing is
    /// marked.
    cursor: usize,

    /// The rows marked so far, by option index.
    ///
    /// A multi-select marks any number and Enter confirms the set. A
    /// single-select marks at most one — Space moves the mark, and
    /// Space on the marked row clears it — so Space always does
    /// something and never answers; Enter is the only send.
    toggled: std::collections::HashSet<usize>,

    /// Whether the compose row is open and the draft owns keys.
    ///
    /// While set, printable keys build the draft and the arrows move
    /// its caret instead of walking the queue; Enter sends the draft
    /// and Esc steps back to the rows with the text kept.
    composing: bool,

    /// The free answer being composed, if any.
    ///
    /// Survives the step back to the options — a half-typed reply is
    /// the user's text, not the overlay's to discard — and clears when
    /// the question resolves.
    draft: String,

    /// The draft's caret, counted in characters from its start.
    ///
    /// A character count, not a byte offset: the draft carries accents
    /// and double-width glyphs, and the caret moves by characters. The
    /// byte offsets an edit needs derive from this, and so does the
    /// screen column it draws at.
    caret: usize,
}

impl QuestionUi {
    /// The draft's length in characters — the unit every caret position
    /// in this type is counted in.
    ///
    /// `String::len` would answer in bytes and put the caret a keystroke
    /// inside an accent, so every bound the caret is clamped against goes
    /// through here. The byte offsets an edit needs are derived from this
    /// count rather than the other way round, because the user counts
    /// characters and only `String` counts bytes.
    fn draft_len(&self) -> usize {
        self.draft.chars().count()
    }

    /// Open the draft for editing, with the caret at its end.
    ///
    /// This is where a fresh draft and a held one meet, and the rule is
    /// that opening never discards: text left by an earlier edit stays,
    /// because coming back to add a word is not a request to lose the
    /// sentence. The caret therefore goes to the end rather than the
    /// start — carrying on is the ordinary move, and `Home` is one press
    /// away when the intent is the front.
    ///
    /// What opening *costs* is the caller's business, not this type's.
    /// Over the compose row it is free; over an option on a
    /// single-answer question it competes with a marked row, and the
    /// sheet shows that trade by shadowing the mark while the draft is
    /// open.
    fn open_composing(&mut self) {
        self.composing = true;
        self.caret = self.draft_len();
    }

    /// Type `ch` at the caret, pushing what stood there to the right.
    ///
    /// Insertion is the whole of typing here: there is no overtype mode,
    /// and the caret ends up just past the character it placed so the
    /// next keystroke follows it. That is what makes pausing to fix a
    /// word mid-sentence behave — step back, insert, and carry on writing
    /// forward without the caret wandering off.
    ///
    /// The character's byte width and the caret's position are two counts
    /// that meet only in `byte_index`: the string is edited by bytes, the
    /// caret is moved in characters, and neither is allowed to assume the
    /// other for the draft's whole length.
    fn type_char(&mut self, ch: char) {
        let byte = byte_index(&self.draft, self.caret);
        self.draft.insert(byte, ch);
        self.caret = self.caret.saturating_add(1);
    }

    /// Delete the character before the caret, leaving the text right of
    /// it exactly where it stands.
    ///
    /// A caret names a position *between* characters, so deleting takes
    /// from the left, as every editor does: the word ahead is untouched,
    /// which is what lets the cursor be parked in the middle of a
    /// sentence and work backwards through it.
    ///
    /// At the start there is nothing to the left, and nothing goes.
    /// Deleting the character to the right instead would borrow this key
    /// for a different one, and a draft empty in the first place is the
    /// same case by position — both leave the text and the caret alone.
    /// A grapheme cluster is not atomic here either: one backspace peels
    /// one character, so an accent is removed before the letter it sits
    /// on.
    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let Some((byte, ch)) = self.draft.char_indices().nth(self.caret.saturating_sub(1)) else {
            return;
        };
        self.draft
            .replace_range(byte..byte.saturating_add(ch.len_utf8()), "");
        self.caret = self.caret.saturating_sub(1);
    }

    /// Move the caret one character left, stopping at the draft's start.
    ///
    /// The stop is the substance of it. While the draft is open the
    /// arrows are the text's, and an arrow that walked off the end of the
    /// text would be two different keys sharing one press — the same
    /// reason the queue's own walking is unreachable from in here.
    fn caret_left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    /// Move the caret one character right, stopping at the draft's end.
    ///
    /// Clamped to the current length rather than to any past-the-end
    /// position, so the caret always sits on a real boundary between two
    /// characters and a type after the last one appends. A draft emptied
    /// from under the caret clamps it back to the single position the
    /// empty string has.
    fn caret_right(&mut self) {
        self.caret = (self.caret.saturating_add(1)).min(self.draft_len());
    }

    /// Put the caret at the draft's start.
    ///
    /// A jump, not a walk: `Home` and `End` are why the arrow keys can
    /// stop at the edges without stranding anyone at the far end of a
    /// long reply. Both are safe on an empty draft, where the two ends
    /// are the same single position.
    fn caret_home(&mut self) {
        self.caret = 0;
    }

    /// Put the caret at the draft's end — the position `open_composing`
    /// already puts it in, kept for the key that means the end from
    /// wherever the caret stands.
    ///
    /// Read off the current length rather than remembered, so text
    /// inserted or deleted elsewhere cannot leave it pointing past what
    /// is there.
    fn caret_end(&mut self) {
        self.caret = self.draft_len();
    }

    /// Insert `text` at the caret as one edit, the caret landing past it.
    ///
    /// The entry a paste takes: one gesture, one insertion, rather than a
    /// character at a time. Landing the caret past what arrived is the
    /// point — text dropped into the middle of a sentence leaves the caret
    /// where typing resumes, the way it does after typing that text by
    /// hand.
    fn insert_str(&mut self, text: &str) {
        let byte = byte_index(&self.draft, self.caret);
        self.draft.insert_str(byte, text);
        self.caret = self.caret.saturating_add(text.chars().count());
    }

    /// Delete the character right of the caret, leaving the caret and the
    /// text left of it exactly where they stand.
    ///
    /// `backspace`'s mirror: a caret names a position between characters,
    /// and this one takes from the right. At the draft's end there is
    /// nothing to the right and nothing goes, which is the same reason
    /// backspace does nothing at the start.
    fn delete_forward(&mut self) {
        let Some((byte, ch)) = self.draft.char_indices().nth(self.caret) else {
            return;
        };
        self.draft
            .replace_range(byte..byte.saturating_add(ch.len_utf8()), "");
    }

    /// Move the caret to the start of the word left of it, or of the word
    /// it stands inside.
    ///
    /// A word is a run of non-blank characters, and blanks are crossed
    /// before the word beyond them, so a caret parked in a gap lands at
    /// the previous word's start rather than at the gap's end — the
    /// backward-word spot the composer's own editor walks to. The step
    /// counts characters here because that is the unit the draft's caret
    /// is held in.
    fn caret_word_left(&mut self) {
        self.caret = self.word_boundary_left(self.caret.min(self.draft_len()));
    }

    /// Move the caret past the end of the word right of it.
    ///
    /// From inside a word the jump is to that word's end; from a gap it
    /// crosses the gap and the word after it, matching the forward-word
    /// convention the composer follows. The caret stops at the draft's end,
    /// where the arrows already stop.
    fn caret_word_right(&mut self) {
        let chars: Vec<char> = self.draft.chars().collect();
        let last = chars.len();
        let mut index = self.caret.min(last);
        while chars.get(index).copied().is_some_and(char::is_whitespace) {
            index = index.saturating_add(1);
        }
        while chars.get(index).is_some_and(|ch| !ch.is_whitespace()) {
            index = index.saturating_add(1);
        }
        self.caret = index;
    }

    /// Delete the word left of the caret, taking the caret with it.
    ///
    /// The text right of the caret keeps its place, so the word reads as
    /// lifted out rather than the tail shifted back. With nothing left of
    /// the caret the boundary is already where the caret stands and
    /// nothing goes.
    fn delete_word_left(&mut self) {
        let caret = self.caret.min(self.draft_len());
        let from = self.word_boundary_left(caret);
        let start = byte_index(&self.draft, from);
        let end = byte_index(&self.draft, caret);
        self.draft.replace_range(start..end, "");
        self.caret = from;
    }

    /// Delete everything between the caret and the draft's start.
    ///
    /// The line-head kill the composer's editor answers with the same
    /// chord: what was typed before the point the user meant to start at
    /// goes in one press, and the caret lands on the start it cleared.
    fn delete_to_start(&mut self) {
        let end = byte_index(&self.draft, self.caret.min(self.draft_len()));
        self.draft.replace_range(0..end, "");
        self.caret = 0;
    }

    /// The draft's character index at the start of the word standing left
    /// of `index`, crossing any blanks between the two.
    fn word_boundary_left(&self, index: usize) -> usize {
        let chars: Vec<char> = self.draft.chars().collect();
        let mut at = index.min(chars.len());
        // The `at > 0` guard is what ends these walks, and the only thing
        // that ends them: `0usize.saturating_sub(1)` is still 0, so the
        // saturated offsets the borrow checker asks for keep reading the
        // draft's first character, and a walk without the guard never
        // reaches the start it is walking to.
        while at > 0
            && chars
                .get(at.saturating_sub(1))
                .copied()
                .is_some_and(char::is_whitespace)
        {
            at = at.saturating_sub(1);
        }
        while at > 0
            && chars
                .get(at.saturating_sub(1))
                .is_some_and(|ch| !ch.is_whitespace())
        {
            at = at.saturating_sub(1);
        }
        at
    }
}

/// The byte offset of `text`'s `index`th character, or the end of the
/// string once the index is past the last character.
///
/// Caret positions count characters while `String` edits take byte
/// offsets, and the two meet here.
fn byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// A queued question and the answer being composed for it.
///
/// The answer's state rides with its question instead of sitting on the
/// app, so a batch behaves like a form: mark three answers, walk back to
/// the first, change it, and everything else is as it was left. A
/// question's state leaves with it when it answers or retires, and the
/// question behind it is still clean.
#[derive(Debug)]
pub(crate) struct PendingQuestion {
    /// What was asked, exactly as delivered.
    ///
    /// The asking side's own wording, with its options in the order the
    /// model wrote them: the sheet renders the question verbatim and the
    /// answer reads its labels off these rows, so the text the user chose
    /// is the text that was asked with. Nothing here is the display's to
    /// reword, reorder, or truncate.
    prompt: QuestionPrompt,

    /// This question's cursor, marks, and draft.
    ///
    /// The answer-in-progress rides with its question rather than on the
    /// app, which is what lets a batch behave like a form: walk away from
    /// a half-answered question and it waits, unwritten, for the return
    /// visit. It arrives clean and retires with the question, so the one
    /// behind an answered question has never worn the last answer.
    ui: QuestionUi,
}

impl PendingQuestion {
    /// Wrap a delivered prompt with a clean answer-in-progress.
    ///
    /// The only way a question enters the queue, which is what makes an
    /// inherited answer impossible: every prompt lands at row zero,
    /// unmarked, with an empty draft, whatever the question that left the
    /// queue just before it had been doing.
    pub(crate) fn new(prompt: QuestionPrompt) -> Self {
        Self {
            prompt,
            ui: QuestionUi::default(),
        }
    }
}

/// The question sheet's rows and the caret riding them.
///
/// Built by [`TuiApp::question_sheet`], the one walk both the render and
/// the height the frame reserves go through, so the box the user sees and
/// the rows held aside for it cannot disagree.
pub(crate) struct QuestionSheet {
    /// Every row from the header chip to the key hints.
    ///
    /// Already wrapped to the width asked for, in the order they draw.
    /// Their count *is* the sheet's height, which is why the layout that
    /// holds rows aside for the overlay counts this list instead of
    /// measuring anything on its own: one walk produces both, so the box
    /// and the space reserved for it cannot disagree.
    pub(crate) lines: Vec<Line<'static>>,

    /// The row holding the draft, and the caret's display column within
    /// its text — `Some` only while the draft owns the keys, since only
    /// then is anything on the sheet being edited.
    pub(crate) caret: Option<(usize, usize)>,
}

/// The column option descriptions and the compose row's subtext hang at:
/// the two marker columns an option row spends before its label — the
/// cursor's and the toggle's.
///
/// Detail rows share it so a description reads as belonging to the option
/// above it rather than as an unrelated indented row. The indent is
/// applied outside `wrap_prompt_rows`, which splits on spaces and would
/// drop a leading run of them as empty words.
const DETAIL_INDENT: &str = "    ";

/// Assemble the question sheet's blocks into at most `budget` rows.
///
/// The compose block and the hints are the two blocks a user cannot answer
/// without: the first is where a reply the options never offered gets
/// written, and the second is the only place the keys are explained. Both
/// keep their rows whatever the terminal asks, which is what stops a short
/// window from cutting the row the user is typing into and leaving them
/// writing blind. The question text follows them, clamped to the rows that
/// fit, and the option list is what gives way.
///
/// The options give way as a window rather than a truncation, anchored on
/// `cursor_option` and grown outward — see [`option_window`]. A draft that
/// outgrows the rows left for it takes its own window around `caret`, so
/// the row the caret writes on stays on screen with the caret translated
/// into it; a draft that fits shows whole. The assembled rows never exceed
/// `budget` — zero included — so a caret can never land on the border. The
/// key hints keep their rows while any room is left for the draft beside
/// them; a sheet too short for both gives the hints up to the caret — the
/// draft windows over the whole remainder, and the tail is the last to
/// go.
fn fit_question_sheet(
    head: Vec<Line<'static>>,
    options: &[(usize, Vec<Line<'static>>)],
    mut compose: Vec<Line<'static>>,
    tail: Vec<Line<'static>>,
    cursor_option: usize,
    budget: usize,
    mut caret: Option<(usize, usize)>,
) -> (Vec<Line<'static>>, usize, Option<(usize, usize)>) {
    let fixed = compose.len().saturating_add(tail.len());
    let spendable = budget.saturating_sub(fixed);
    let head_rows = head.len().min(spendable);
    let room = spendable.saturating_sub(head_rows);
    let (lo, hi) = option_window(options, cursor_option, room);
    let mut lines = Vec::with_capacity(budget);
    lines.extend(head.into_iter().take(head_rows));
    lines.extend(
        options
            .iter()
            .skip(lo)
            .take(hi.saturating_sub(lo))
            .flat_map(|(_, rows)| rows.clone()),
    );
    let compose_start = lines.len();
    let compose_room = budget
        .saturating_sub(compose_start)
        .saturating_sub(tail.len());
    let compose_room = if compose_room > 0 {
        compose_room
    } else {
        budget.saturating_sub(compose_start)
    };
    if compose.len() > compose_room && compose_room > 0 {
        let caret_row = caret.map_or(0, |(row, _)| row);
        let window_start = caret_row
            .saturating_sub(compose_room.saturating_sub(1))
            .min(compose.len().saturating_sub(compose_room));
        caret = caret.map(|(row, column)| (row.saturating_sub(window_start), column));
        let mut windowed = compose.split_off(window_start);
        windowed.truncate(compose_room);
        compose = windowed;
    }
    lines.extend(compose);
    let tail_room = budget.saturating_sub(lines.len());
    lines.extend(tail.into_iter().take(tail_room));
    lines.truncate(budget);
    (lines, compose_start, caret)
}

/// The option groups that fit in `room` rows, as the `(lo, hi)` index
/// range into `options` that holds `cursor_option`.
///
/// The cursor's own group is taken first, because it is the row the sheet
/// has to show, and the window then grows downward before upward so a list
/// reads in the order it was asked. Walking with the arrows moves the
/// window with the cursor, so an option is never unreachable — only
/// undrawn until it is stood on. A budget that will not hold the cursor's
/// own group leaves the window empty: options have nothing to spare rather
/// than half a row between them.
fn option_window(
    options: &[(usize, Vec<Line<'static>>)],
    cursor_option: usize,
    room: usize,
) -> (usize, usize) {
    let last = options.len();
    if last == 0 || room == 0 {
        return (0, 0);
    }
    let rows_of = |index: usize| {
        options
            .get(index)
            .map_or(usize::MAX, |(_, rows)| rows.len())
    };
    let below = |index: usize| {
        if index < last {
            rows_of(index)
        } else {
            usize::MAX
        }
    };
    let above = |index: usize| {
        if index > 0 {
            rows_of(index.saturating_sub(1))
        } else {
            usize::MAX
        }
    };
    let start = cursor_option.min(last.saturating_sub(1));
    let mut used = rows_of(start);
    if used > room {
        return (0, 0);
    }
    let (mut lo, mut hi) = (start, start.saturating_add(1));
    loop {
        let down = below(hi);
        if used.saturating_add(down) <= room {
            used = used.saturating_add(down);
            hi = hi.saturating_add(1);
            continue;
        }
        let up = above(lo);
        if used.saturating_add(up) <= room {
            used = used.saturating_add(up);
            lo = lo.saturating_sub(1);
            continue;
        }
        break;
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn the_sheet_never_gives_up_the_row_being_written() {
        // The compose block and the key hints are what a user cannot answer
        // without, so they are paid for first and the question's text and the
        // option list are what a short terminal takes from.
        let groups: Vec<(usize, Vec<ratatui::text::Line<'static>>)> =
            (0..2).map(|index| (index, block_rows("o", 1))).collect();
        let (lines, compose_start, _) = fit_question_sheet(
            block_rows("h", 3),
            &groups,
            block_rows("c", 2),
            block_rows("t", 1),
            0,
            3,
            None,
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(compose_start, 0, "the draft starts at the top of the sheet");
        assert_eq!(
            rows,
            vec!["c0", "c1", "t0"],
            "a terminal holding only the untouchable blocks keeps them whole"
        );

        // One more row buys the question's text back before it buys a
        // single option: reading the question outranks reading the list.
        let groups = vec![(0, block_rows("o", 1)), (1, block_rows("o", 1))];
        let (lines, compose_start, _) = fit_question_sheet(
            block_rows("h", 3),
            &groups,
            block_rows("c", 2),
            block_rows("t", 1),
            0,
            6,
            None,
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(compose_start, 3);
        assert_eq!(rows, vec!["h0", "h1", "h2", "c0", "c1", "t0"]);
    }

    #[test]
    fn the_option_window_holds_the_row_the_cursor_stands_on() {
        // The options give way as a window around the row the cursor stands
        // on, never as a truncation that leaves the cursor outside.
        let options: Vec<(usize, Vec<ratatui::text::Line<'static>>)> =
            (0..5).map(|index| (index, block_rows("o", 1))).collect();
        assert_eq!(
            option_window(&options, 4, 1),
            (4, 5),
            "the cursor's own row is the row that survives"
        );
        assert_eq!(
            option_window(&options, 1, 2),
            (1, 3),
            "the window grows below first, so the list reads in the order it was asked"
        );
        assert_eq!(option_window(&options, 2, 0), (0, 0), "nothing to show");
        let tall = vec![(0, block_rows("o", 3)), (1, block_rows("o", 1))];
        assert_eq!(
            option_window(&tall, 0, 2),
            (0, 0),
            "a cursor's group that will not fit leaves the list out entirely"
        );
    }

    #[test]
    fn a_draft_longer_than_the_sheet_windows_around_the_caret() {
        // The compose block is five rows against a budget that, with
        // the hints, holds four of them: the window keeps the rows
        // around the caret — the row it writes on — and the caret
        // lands inside the window, never on the border.
        let (lines, compose_start, caret) = fit_question_sheet(
            block_rows("h", 1),
            &[],
            block_rows("c", 5),
            block_rows("t", 1),
            0,
            5,
            Some((4, 2)),
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(
            compose_start, 0,
            "the untouchable blocks take the budget before the head"
        );
        assert_eq!(
            rows,
            vec!["c1", "c2", "c3", "c4", "t0"],
            "the draft's window holds its last rows — the caret's end"
        );
        assert_eq!(
            caret,
            Some((3, 2)),
            "the caret lands on its own row inside the window"
        );
    }

    #[test]
    fn a_sheet_too_short_for_the_draft_and_the_hints_keeps_the_caret_row() {
        // The zero-room case: a budget of three against a five-row
        // draft and three rows of hints. Reserving the hints first
        // leaves the draft nothing, and the old assembly kept the
        // draft's first rows and dropped the caret's — the user
        // typed blind. The tail is the last to go: the draft windows
        // over the whole budget around the caret, the hints lose
        // their rows, and a one-row budget still keeps the caret's own
        // row.
        let (lines, compose_start, caret) = fit_question_sheet(
            block_rows("h", 1),
            &[],
            block_rows("c", 5),
            block_rows("t", 3),
            0,
            3,
            Some((4, 2)),
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(compose_start, 0, "the head cannot fit and gives way");
        assert_eq!(
            rows,
            vec!["c2", "c3", "c4"],
            "the draft windows around the caret; the hints yield their rows"
        );
        assert_eq!(caret, Some((2, 2)), "the caret lands on its own row");
        let (lines, _, caret) = fit_question_sheet(
            block_rows("h", 1),
            &[],
            block_rows("c", 5),
            block_rows("t", 3),
            0,
            1,
            Some((4, 2)),
        );
        let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(
            rows,
            vec!["c4"],
            "a one-row budget keeps the caret's row alone"
        );
        assert_eq!(caret, Some((0, 2)), "translated into the one-row window");
    }

    #[test]
    fn the_assembled_rows_never_exceed_the_budget() {
        // Four fixed rows against a three-row budget: the sheet keeps
        // exactly three, and a zero budget assembles nothing at all — a
        // caret on the border was the old overflow's symptom.
        let (lines, _, _) = fit_question_sheet(
            block_rows("h", 1),
            &[],
            block_rows("c", 2),
            block_rows("t", 1),
            0,
            3,
            None,
        );
        assert_eq!(lines.len(), 3, "the assembly is capped at the budget");
        let (none, _, _) = fit_question_sheet(
            block_rows("h", 1),
            &[],
            block_rows("c", 2),
            block_rows("t", 1),
            0,
            0,
            None,
        );
        assert!(none.is_empty(), "a zero budget assembles no rows");
    }

    #[test]
    fn the_draft_walks_and_erases_by_words() {
        // Editing a written reply follows the composer's line-editor
        // conventions: words are units, and `Delete` erases to the right.
        let mut ui = QuestionUi::default();
        ui.open_composing();
        for ch in "run the lints".chars() {
            ui.type_char(ch);
        }
        assert_eq!(ui.caret, 13);

        ui.caret_word_left();
        assert_eq!(ui.caret, 8, "the caret lands at the word's start");
        ui.caret_word_right();
        assert_eq!(ui.caret, 13, "and walks back the same distance");

        ui.caret_word_left();
        ui.delete_word_left();
        assert_eq!(
            ui.draft, "run lints",
            "one word goes with the space that held it off, and not the line"
        );
        assert_eq!(ui.caret, 4);

        ui.caret_word_left();
        assert_eq!(ui.caret, 0, "the caret stops at the line's start");
        ui.delete_word_left();
        assert_eq!(ui.draft, "run lints", "nothing lies before the start");

        ui.caret_end();
        ui.delete_to_start();
        assert_eq!(
            ui.draft, "",
            "from the end of the line, one press takes everything written"
        );

        for ch in "draft".chars() {
            ui.type_char(ch);
        }
        ui.caret = 0;
        ui.delete_forward();
        assert_eq!(ui.draft, "raft", "Delete erases right of the caret");
        ui.caret_word_right();
        assert_eq!(ui.caret, 4);
        ui.insert_str("s were ");
        assert_eq!(ui.draft, "rafts were ", "a paste lands at the caret");
    }
}
