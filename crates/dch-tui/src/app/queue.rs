//! The waiting-prompt section's interaction: the queue's keyboard
//! modes, the edit and confirmation flows, and the entries they
//! steer, edit, and drop.

use super::TuiApp;
use super::keys::key_survives_the_sheet;
use super::notices::{plain_wrapped_lines, wrap_prompt_rows};
use crate::cells::{for_display, str_width};
use crate::queue::Submission;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;

impl TuiApp {
    /// Fold or lift the waiting section a press landed on.
    ///
    /// A click on the section flips it between its standing and
    /// folded postures — the flip
    /// [`toggle_queue_posture`](Self::toggle_queue_posture) makes —
    /// badge and body rows alike. The plain mode alone answers: the
    /// fold is a display posture the section's own modes refuse, so a
    /// press while the queue holds the keyboard changes nothing.
    /// Returns whether the press belonged to the section.
    pub(crate) fn toggle_queue_section_at(&mut self, column: u16, row: u16) -> bool {
        if self.queue.queue_mode != QueueMode::Plain || self.queued_display_len() == 0 {
            return false;
        }
        let rect = self.queue.queue_section_rect;
        if rect.height == 0
            || column < rect.x
            || column >= rect.right()
            || row < rect.y
            || row >= rect.bottom()
        {
            return false;
        }
        self.queue.toggle_queue_posture();
        true
    }

    /// Hand `id` to the driver, reporting whether the channel took it.
    ///
    /// The send is a [`Submission::Run`]: the driver resolves the id
    /// against the shared queue when the turn comes, so this never has to
    /// take anything back.
    pub(crate) fn send_queued_id(&self, id: u64) -> bool {
        self.host
            .submit_tx
            .as_ref()
            .is_some_and(|tx| tx.send(Submission::Run { id }).is_ok())
    }

    /// Add `text` to the shared queue and name it.
    ///
    /// The id this returns is the prompt's only handle from here on: the
    /// panel addresses its row through it and the channel carries nothing
    /// else, which is why an id is issued before the send rather than
    /// taken from whatever position the prompt happens to land at.
    pub(crate) fn push_queued(&self, text: String) -> u64 {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text)
    }

    /// Take the prompt `id` out of the shared queue.
    ///
    /// The user's drop, the rollback of a send nobody took, and the
    /// cancel's clearing all need the same act: the prompt is gone and
    /// can never run.
    pub(crate) fn remove_queued(&self, id: u64) -> Option<String> {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
    }

    /// Rewrite the prompt `id` holds, reporting whether it was still there.
    ///
    /// In place and by id: the saved text takes the same slot the prompt
    /// was submitted into, and a prompt the driver claimed mid-edit
    /// answers `false` rather than letting the text land on its neighbour.
    fn replace_queued(&self, id: u64, text: String) -> bool {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(id, text)
    }

    /// The queue's display length: every waiting prompt, minus the one
    /// whose claim is a channel hop away.
    ///
    /// The handoff hide is display-only — the shared queue still holds
    /// the prompt for the driver to claim, and the exit and resume
    /// paths read the queue directly, unaffected. The hidden row is
    /// the queue's last (its window only opens on a queue that was
    /// empty), so earlier rows keep their display positions.
    pub(crate) fn queued_display_len(&self) -> usize {
        let total = self.queued_len();
        if self
            .queue
            .handed
            .is_some_and(|id| self.queued_index_of(id).is_some())
        {
            total.saturating_sub(1)
        } else {
            total
        }
    }

    /// How many prompts wait.
    ///
    /// Two callers depend on this count agreeing with what the panel shows:
    /// the status tag, which names a number of prompts the user can go on to
    /// walk, and the layout, which holds rows for the section only while it
    /// is non-zero — a session with nothing waiting keeps its exact rows.
    /// A poisoned lock still answers from the queue it holds: a panic
    /// elsewhere in the process says nothing about what the user queued.
    pub(crate) fn queued_len(&self) -> usize {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The prompt at display position `index`, as `(id, text)`.
    ///
    /// The one conversion from a row on screen to an address, read fresh
    /// every frame: a prompt claimed or dropped since the last frame
    /// moves the rows underneath the cursor, and a remembered position
    /// would point at whoever slid into its place.
    pub(crate) fn queued_at(&self, index: usize) -> Option<(u64, String)> {
        let queue = self
            .state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        queue.get(index).map(|entry| (entry.id, entry.text.clone()))
    }

    /// Where the prompt `id` sits now, if it still waits.
    ///
    /// Asked after the fact rather than remembered: a save lands by id,
    /// and this only answers where to draw the row it landed in.
    pub(crate) fn queued_index_of(&self, id: u64) -> Option<usize> {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .index_of(id)
    }

    /// Apply a key press to the queue section while it holds the keys.
    ///
    /// `Some(redraw)` when the section owns the press — as the focused
    /// list, or as the prompt standing open in the composer. `None` while
    /// the queue holds neither, and for the keys
    /// [`key_survives_the_sheet`] names: the run's cancel, copy, and
    /// steering chords and the transcript's paging stay live over the
    /// section, the same courtesy the ask sheet extends, because a user
    /// checking what a waiting prompt refers to has to be able to read
    /// the conversation it came out of. The chords are asked here rather
    /// than below for the moments they own: `Ctrl+W` walks the waiting
    /// prompts — from the composer it raises the section's focus,
    /// standing where the view last stood, and once the section holds
    /// the keys the chord steps the walk forward; over a folded section
    /// or an empty queue the chord is inert or the composer's — the
    /// fold opens only on `Ctrl+Q` or a click on the section, and a
    /// queue with nothing waiting hands the chord back so the composer
    /// keeps its delete-a-word.
    /// `Ctrl+Q` folds and
    /// unfolds the section from the composer, commits an edit from
    /// inside it, and leaves a focused section. Leaving a
    /// focused section is `Esc`'s and the chord's both — the focused arm
    /// below takes the key and hands the keys back, so one press means
    /// enter and the next means leave.
    pub(crate) fn queue_key(&mut self, key: KeyEvent) -> Option<bool> {
        if matches!(self.queue.queue_mode, QueueMode::Editing { .. }) {
            if let (KeyCode::Char('q'), KeyModifiers::CONTROL) = (key.code, key.modifiers) {
                self.commit_queue_edit();
                return Some(true);
            }
            return self.queue_edit_key(key);
        }
        if key_survives_the_sheet(key) {
            return None;
        }
        if let QueueMode::Confirming { act, id } = self.queue.queue_mode {
            return Some(self.queue_confirm_key(key, act, id));
        }
        if self.queue.queue_mode == QueueMode::Focused {
            return Some(self.queue_focused_key(key));
        }
        if let (KeyCode::Char('w'), KeyModifiers::CONTROL) = (key.code, key.modifiers) {
            if self.queued_display_len() == 0 {
                return None;
            }
            if self.queue.queue_posture == QueuePosture::Folded {
                return Some(true);
            }
            self.focus_queue();
            return Some(true);
        }
        if let (KeyCode::Char('q'), KeyModifiers::CONTROL) = (key.code, key.modifiers)
            && self.queued_display_len() > 0
        {
            self.queue.toggle_queue_posture();
            return Some(true);
        }
        None
    }

    /// The keys the section takes back while a prompt is out in the
    /// composer.
    ///
    /// Typing, the caret, and pasting stay the composer's: the draft is
    /// fixed with the same keys a new prompt is written with, which is the
    /// point of lifting it there rather than into a box with its own
    /// editor. Three chords come back here — Enter saves into the prompt
    /// the text came from, and Esc and Ctrl+C both put the draft away.
    /// Ctrl+C is the one whose meaning moves: with a prompt out, the press
    /// lands on the text in front of the user rather than reaching past it
    /// to a run they have stopped looking at, and the run's cancel is one
    /// Esc away. Neither abort drops anything — a key taken as a way out
    /// of the mode leaves the waiting prompt exactly as it was found.
    /// Everything else is the composer's, and falls through.
    fn queue_edit_key(&mut self, key: KeyEvent) -> Option<bool> {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) | (KeyCode::Char('m'), KeyModifiers::CONTROL) => {
                self.commit_queue_edit();
                Some(true)
            }
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.abort_queue_edit();
                Some(true)
            }
            _ => None,
        }
    }

    /// The keys the focused list answers.
    ///
    /// Left and Right walk the waiting prompts and `Ctrl+W` steps the
    /// same walk forward, `e` lifts the shown one
    /// into the composer, `s` sends it to the turn in flight as guidance,
    /// Delete or `d` takes it out of the queue for good, and Esc hands the
    /// keyboard back. Whatever else arrives is
    /// swallowed here rather than handed down: a letter typed while the
    /// user reads the backlog has nowhere it should be going, and letting
    /// it reach the composer would build a draft behind their back.
    pub(crate) fn queue_focused_key(&mut self, key: KeyEvent) -> bool {
        let bare = key.modifiers == KeyModifiers::NONE;
        match key.code {
            KeyCode::Left if bare => self.step_queue_view(false),
            KeyCode::Right if bare => self.step_queue_view(true),
            KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {
                self.step_queue_view(true);
            }
            KeyCode::Esc => self.queue.queue_mode = QueueMode::Plain,
            KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
                self.queue.queue_mode = QueueMode::Plain;
            }
            KeyCode::Char('e') if bare => self.open_queue_edit(),
            KeyCode::Delete | KeyCode::Char('d') if bare => {
                let Some((id, _)) = self.queued_at(self.queue.queue_view) else {
                    self.clamp_queue_view();
                    return true;
                };
                self.queue.queue_mode = QueueMode::Confirming {
                    act: QueueAct::Drop,
                    id,
                };
            }
            KeyCode::Char('s') if bare => {
                let Some((id, _)) = self.queued_at(self.queue.queue_view) else {
                    self.clamp_queue_view();
                    return true;
                };
                self.queue.queue_mode = QueueMode::Confirming {
                    act: QueueAct::Steer,
                    id,
                };
            }
            _ => {}
        }
        true
    }

    /// The keys the confirmation answers.
    ///
    /// `y` performs the act the ask holds — after the mode returns to
    /// `Focused`, so the act's own emptying of the queue still releases
    /// the focus the way the immediate keypress once did. The act
    /// resolves against the prompt whose id the ask recorded: if a
    /// claim took it in the window between the arming press and `y`,
    /// whatever slid into its display position is a stranger the
    /// confirm refuses to spend. Everything else stands the ask down:
    /// `n` and Esc plainly, and any stray key too, because a key that
    /// lands by accident must never be the one that spends the prompt.
    pub(crate) fn queue_confirm_key(&mut self, key: KeyEvent, act: QueueAct, id: u64) -> bool {
        if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Char('y') {
            self.queue.queue_mode = QueueMode::Focused;
            if self.queued_index_of(id).is_none() {
                self.post_notice(
                    "the prompt left the queue — the confirmation stood down".to_string(),
                );
                self.clamp_queue_view();
                return true;
            }
            match act {
                QueueAct::Drop => self.drop_queue_entry(id),
                QueueAct::Steer => self.steer_queued(id),
            }
        } else {
            self.queue.queue_mode = QueueMode::Focused;
        }
        true
    }

    /// Put the keyboard in the queue section's hands.
    ///
    /// `false` when nothing waits: the chord asks for a panel, and a
    /// session with nothing queued has none, so taking the keys then
    /// would leave a mode with nothing on screen to act on. A view left
    /// from an earlier walk is clamped back onto the queue rather than
    /// reset, so stepping out and in again returns to where the walk was;
    /// a first entry meets the queue at the front, the prompt that runs
    /// next.
    fn focus_queue(&mut self) -> bool {
        if self.queued_display_len() == 0 {
            return false;
        }
        self.queue.queue_mode = QueueMode::Focused;
        self.clamp_queue_view();
        true
    }

    /// Lift the shown prompt into the composer for editing.
    ///
    /// What is already in the composer is not collateral: with a draft
    /// standing there the prompt stays in the queue and the reason goes on
    /// the notice row. The two texts have nowhere to meet — overwriting
    /// the user's unsent words to edit somebody else's prompt would trade
    /// one lost draft for two.
    ///
    /// The focus is released for the duration, which is the mechanism and
    /// not a courtesy: the list's keys and the composer's keys are
    /// different keys, and typing has to reach the text on screen.
    fn open_queue_edit(&mut self) {
        let Some((id, text)) = self.queued_at(self.queue.queue_view) else {
            self.clamp_queue_view();
            return;
        };
        if !self.composer.input.is_empty() {
            self.post_notice(
                "the composer holds a draft — empty it to edit a waiting prompt".to_string(),
            );
            return;
        }
        self.composer.input.set_text(text);
        self.queue.queue_mode = QueueMode::Editing { id };
    }

    /// Save the composer's text into the prompt it was opened from.
    ///
    /// A save is not a submission: nothing goes out, nothing enters the
    /// input history — the recall the Up key walks is the list of prompts
    /// actually sent, and an edit was never one — and the run in flight is
    /// untouched. What changes is the body the claim will read, in the
    /// slot the prompt was submitted into, so the order the prompts were
    /// typed in stays the order their turns run in.
    ///
    /// Two edges, and both say so on the notice row. A blank body is
    /// refused rather than saved: an entry emptied into a prompt of nothing
    /// is worse to find on the row than the wrong word that was there, and
    /// the draft stays out for another try. A prompt the run claimed while
    /// its text was out cannot be edited retroactively — the words it went
    /// with already reached the model — so the fix goes out as a prompt of
    /// its own instead of being quietly thrown away.
    fn commit_queue_edit(&mut self) {
        let QueueMode::Editing { id } = self.queue.queue_mode else {
            return;
        };
        let text = self.composer.input.text().to_string();
        if text.trim().is_empty() {
            self.post_notice(
                "the prompt keeps its words — a save needs something to send".to_string(),
            );
            return;
        }
        self.composer.input.clear();
        if self.replace_queued(id, text.clone()) {
            if let Some(index) = self.queued_index_of(id) {
                self.queue.queue_view = index;
            }
        } else {
            self.correct_claimed(id, text);
        }
        self.leave_queue_edit();
        self.clamp_queue_view();
    }

    /// Put the draft away, leaving its prompt exactly as it was.
    ///
    /// The way out of the mode, not out of the queue: the prompt keeps the
    /// wording it had before the composer took it and waits where it
    /// waited, and the composer's buffer — which held somebody else's
    /// prompt on loan, never the user's own draft — goes with it.
    fn abort_queue_edit(&mut self) {
        if !matches!(self.queue.queue_mode, QueueMode::Editing { .. }) {
            return;
        }
        self.composer.input.clear();
        self.clamp_queue_view();
        self.leave_queue_edit();
    }

    /// Take the named prompt out of the queue for good.
    ///
    /// A waiting prompt is the only thing this touches: the run in flight
    /// keeps running, the prompts behind it keep their order, and the
    /// prompt itself simply never runs. The removal keys on the id the
    /// caller armed — the confirmation's ask, or the focused press — so
    /// whatever slid into a stale view position is never the prompt
    /// spent; a miss (the id left between the ask and the act) stands
    /// down with the same notice the confirm's own gate gives. There is
    /// no confirmation here because there is nothing to lose that
    /// cannot be retyped — the entry is on screen at the press, and it
    /// is the only way to unload an entry a blank save refuses to empty.
    fn drop_queue_entry(&mut self, id: u64) {
        if self.remove_queued(id).is_none() {
            self.post_notice("the prompt left the queue — the confirmation stood down".to_string());
        }
        self.clamp_queue_view();
        self.release_queue_if_empty();
    }

    /// Come back out of an edit, into the mode the section stands in now.
    ///
    /// The keys go to the section, because that is where they were the
    /// moment before `e` lifted a prompt out: an edit is a detour off the
    /// walk, not a new departure, and a user who saved one prompt's fix
    /// expects to step to the next with an arrow rather than a chord. With
    /// nothing left to stand on there is no section to hold the keys, and
    /// the composer takes them back.
    pub(crate) fn leave_queue_edit(&mut self) {
        self.queue.queue_mode = if self.queued_len() == 0 {
            QueueMode::Plain
        } else {
            QueueMode::Focused
        };
    }

    /// Stand the section's focus down when the section is gone.
    ///
    /// The focus is the only mode released. It holds keys for a panel that
    /// is no longer drawn, and keeping it over an empty queue would swallow
    /// every keystroke for nothing. An open edit is kept: it holds no keys
    /// of the section's — typing, the caret, and pasting stay the composer's
    /// in that mode whatever the queue looks like — and what stands in the
    /// composer is the user's own text mid-edit, not something the section
    /// can release on the queue's behalf.
    ///
    /// Which is the moment the edit matters most. A run claiming the last
    /// waiting prompt is exactly when the prompt open in the composer has
    /// already gone out with the older wording, and the frame that draws the
    /// claim lands before the Enter that fixes it. Releasing the mode there
    /// would turn the fix into an ordinary second turn and quietly drop the
    /// cut the section promises; keeping it lets
    /// [`commit_queue_edit`](Self::commit_queue_edit) find the entry gone and
    /// send the wording as a correction instead.
    pub(crate) fn release_queue_if_empty(&mut self) {
        if self.queued_len() == 0
            && matches!(
                self.queue.queue_mode,
                QueueMode::Focused | QueueMode::Confirming { .. }
            )
        {
            self.queue.queue_mode = QueueMode::Plain;
        }
    }

    /// Walk the shown prompt one step, wrapping at both ends.
    ///
    /// The walk moves the view and leaves the queue's order alone, for the
    /// reason the ask sheet's walk gives: the count the section prints is
    /// the shown prompt's place in the order the prompts were submitted,
    /// and it says the same thing after a walk away and back. It is a
    /// circle, so the prompt at the tail is one press from the one at the
    /// front rather than a walk through everything between. A queue of one
    /// has nowhere to go and the step declines — the key is still
    /// swallowed, so an arrow cannot fall through to the transcript.
    fn step_queue_view(&mut self, forward: bool) {
        let count = self.queued_len();
        if count < 2 {
            return;
        }
        self.queue.queue_view = if forward {
            let next = self.queue.queue_view.saturating_add(1);
            if next >= count { 0 } else { next }
        } else if self.queue.queue_view == 0 {
            count.saturating_sub(1)
        } else {
            self.queue.queue_view.saturating_sub(1)
        };
    }

    /// Keep [`queue_view`](Self::queue_view) a live position in the queue.
    ///
    /// A claim at the front and a drop anywhere pull the rows behind them
    /// up a place, and a view left past the end would show nothing rather
    /// than the prompt that slid into the row. An emptied queue takes the
    /// view home to zero, which is also where the next prompt is met.
    pub(crate) fn clamp_queue_view(&mut self) {
        self.queue.queue_view = self
            .queue
            .queue_view
            .min(self.queued_display_len().saturating_sub(1));
    }

    /// Reconcile the section's view and mode with what the queue holds.
    ///
    /// Once a frame, because the queue changes underneath the keyboard
    /// with no key pressed: a run's claim takes the front prompt the
    /// instant its turn starts — one claim per turn, so the backlog
    /// thins under the walk whenever a turn lands. Nothing here is a
    /// policy the keys do not already
    /// apply — it is the same two rules, applied by the frame rather than
    /// waiting for a press that may never come.
    ///
    /// The mode half of the release is the frame's most consequential
    /// contribution, because the claim that empties the queue is the event
    /// that makes an open edit a correction rather than a save, and it
    /// always lands a frame before the Enter that follows it. [`release_queue_if_empty`]
    /// is why the frame stands down a focus and leaves an edit alone.
    pub(crate) fn sync_queue_view(&mut self) {
        if self.queued_len() == 0 {
            self.queue.queue_view = 0;
            self.release_queue_if_empty();
            return;
        }
        self.clamp_queue_view();
    }

    /// The rows the waiting section needs for the prompt it shows.
    ///
    /// The badge, the air above it and below it, and the rows the
    /// shown prompt wraps into at this width — the section reserves
    /// what it paints and nothing beneath it: a one-line wording ends
    /// the section at its own last row, so the air under the queue is
    /// the frame's furniture (the notice row, the spacer), never the
    /// section's own blank margin. A wrapping wording grows the
    /// section only as far as the two prompt rows the cap allows, so
    /// the conversation pays for words, not for air.
    pub(crate) fn queue_section_height(&self, width: u16) -> u16 {
        let total = self.queued_display_len();
        if total == 0 {
            return 0;
        }
        if self.queue.queue_posture == QueuePosture::Folded
            && self.queue.queue_mode == QueueMode::Plain
        {
            return 2;
        }
        let shown = self.queue.queue_view.min(total.saturating_sub(1));
        let Some((_, text)) = self.queued_at(shown) else {
            return 0;
        };
        let prefix = queue_prompt_prefix(total, shown);
        let inner = usize::from(
            width
                .saturating_sub(super::todos::TODO_SECTION_INSET.saturating_mul(2))
                .max(1),
        );
        let limit = inner.saturating_sub(str_width(&prefix)).max(1);
        let cap = usize::from(QUEUE_ROWS).saturating_sub(3);
        let body = wrap_prompt_rows(&for_display(&text), limit, cap).len();
        u16::try_from(body.saturating_add(3)).unwrap_or(QUEUE_ROWS)
    }

    /// The chords the queue section names beside its badge.
    ///
    /// They are the mode's, not the section's: while the composer holds the
    /// keys the section cannot act on a letter typed into it, so the badge
    /// offers the way in rather than keys that would be swallowed a moment
    /// later. Once the section has them, it says what it does with them,
    /// including the way back out — the mode that swallows everything else
    /// owes the user the key that ends it.
    pub(crate) fn queue_hint(&self) -> &'static str {
        if matches!(self.queue.queue_mode, QueueMode::Editing { .. }) {
            "enter save  esc discard"
        } else if let QueueMode::Confirming { act, .. } = self.queue.queue_mode {
            match act {
                QueueAct::Steer => "guide the running turn? y/n",
                QueueAct::Drop => "drop this prompt? y/n",
            }
        } else if self.queue.queue_mode == QueueMode::Focused {
            "\u{2190}/\u{2192} walk  ctrl+w next  e edit  s guide  d drop  esc back"
        } else {
            "ctrl+w walk  ctrl+q hide"
        }
    }

    /// Send the composer straight to the model, mid-turn: `Ctrl+S`.
    ///
    /// Enter is patient and Ctrl+S is not. With a run in flight and the
    /// seam installed the text goes to the steering slot and reaches the
    /// model at the run's next turn boundary, joining the conversation the
    /// model continues rather than waiting behind the queue. With no run to
    /// steer — or with a run that ended in the instant between the keypress
    /// and the offer — `steer` refuses and the text takes the ordinary
    /// path, so the chord is never a key that eats what was typed.
    ///
    /// A prompt open in the composer for editing belongs to the waiting
    /// section no longer: its slot is retired first, or the same words
    /// would reach the model now *and* run again from the queue later.
    ///
    /// A draft of nothing but blanks is refused, exactly as Enter refuses it:
    /// the chord sends what Enter would send, and Enter sends nothing here.
    /// From an edit the stakes are higher than a wasted row — the slot is
    /// retired before delivery — so a whitespace-only draft would drop a real
    /// waiting prompt to deliver text nobody typed.
    ///
    /// The row is owed from here rather than at a claim: unlike a
    /// queued prompt, this one is sent by the hand that writes it.
    /// The transcript itself records it only when the model reads it
    /// — the row waits pinned just above the timer until the boundary
    /// that drains it — so the send and the record are two moments,
    /// and both are the sender's own.
    pub(crate) fn steer_now(&mut self) -> bool {
        if self.composer.input.text().trim().is_empty() {
            return true;
        }
        let text = self.composer.input.text().to_string();
        if let QueueMode::Editing { id } = self.queue.queue_mode {
            self.remove_queued(id);
            self.leave_queue_edit();
            self.clamp_queue_view();
            self.release_queue_if_empty();
        }
        self.composer.input.clear();
        self.composer.input.remember(text.clone());
        self.deliver_guidance(text);
        true
    }

    /// Hand `text` to the run in flight, or send it as a turn.
    ///
    /// The one delivery path behind both entrances to steering — the
    /// composer's `Ctrl+S` and the waiting section's `s` — so the two can
    /// never mean different things. Accepted, it is the loop's to read
    /// at its next boundary; its transcript row is not written at the
    /// send, though — the model has not read the words yet — but pinned
    /// just above the running turn's timer as a user row, in the spot
    /// the boundary that reads it will land it in, after the work that
    /// was running. A run that ends before any boundary reads it
    /// hands the text to the waiting section instead. Refused, it
    /// takes the ordinary path.
    ///
    /// Recording on recall stays with the callers: a prompt that arrives
    /// out of the queue was already recorded when it was submitted, and
    /// saying it twice would list it twice.
    fn deliver_guidance(&mut self, text: String) {
        let steered = self
            .state
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|guidance| guidance.steer(text.clone()))
            || self
                .host
                .guidance
                .as_ref()
                .is_some_and(|guidance| guidance.steer(text.clone()));
        if steered {
            self.state.push_steered(text);
            self.scroll.scroll_to_bottom();
            return;
        }
        self.submit_text(text);
    }

    /// Guide the named prompt into the run on screen: `s`'s act.
    ///
    /// The hole `Ctrl+S` cannot reach: a prompt typed a minute ago, queued
    /// patiently, that the running turn has since made urgent. It leaves
    /// the queue and joins the turn in flight, which is also why it leaves
    /// first — the same words must not both reach the model now and run
    /// again from the queue later. The removal keys on the id the caller
    /// armed and the text delivered is the one that removal returned: if a
    /// claim took the prompt in the window between the arming press and
    /// the act, whatever slid into its display position is a stranger the
    /// guide refuses to spend, and the prompt already on its way to the
    /// model is never duplicated.
    ///
    /// Refused while nothing is running. Guidance is advice for a turn, and
    /// a session between turns has no turn to advise; the prompt stays
    /// where it is, in order, rather than being fired into a gap.
    pub(crate) fn steer_queued(&mut self, id: u64) {
        if !self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "nothing is running to guide — the prompt waits in its place".to_string(),
            );
            return;
        }
        let Some(text) = self.remove_queued(id) else {
            self.post_notice("the prompt left the queue before the guide landed".to_string());
            self.clamp_queue_view();
            self.release_queue_if_empty();
            return;
        };
        self.clamp_queue_view();
        self.release_queue_if_empty();
        self.deliver_guidance_literal(text);
    }

    /// Hand `text` to the run in flight, or send it as a turn —
    /// literally.
    ///
    /// The queue-originated counterpart to [`deliver_guidance`]: when
    /// guidance is refused and the text must fall back to its own
    /// turn, a slash-prefixed prompt that the user saved as literal
    /// words is delivered as those words, not dispatched as the
    /// command it happens to spell. The composer's own submissions
    /// still parse commands; this path never does.
    fn deliver_guidance_literal(&mut self, text: String) {
        let steered = self
            .state
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|guidance| guidance.steer(text.clone()))
            || self
                .host
                .guidance
                .as_ref()
                .is_some_and(|guidance| guidance.steer(text.clone()));
        if steered {
            self.state.push_steered(text);
            self.scroll.scroll_to_bottom();
            return;
        }
        self.submit_literal(text);
    }

    /// Send a text as a prompt without command parsing.
    ///
    /// The literal delivery half of the submission surface: whatever
    /// the text starts with, it joins the conversation as the user's
    /// words. Queue-originated fallbacks use this so a saved `/clear`
    /// reaches the model as prompt text rather than clearing the
    /// session.
    fn submit_literal(&mut self, text: String) {
        if self.host.submit_tx.is_some() {
            let was_empty = self.queued_len() == 0;
            let idle = self.state.live_claim.load(Ordering::SeqCst) == u64::MAX
                && !self.state.agent_running.load(Ordering::SeqCst);
            self.queue.handed = None;
            let id = self.push_queued(text.clone());
            if self.send_queued_id(id) {
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
            self.push_message(crate::message::TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            });
        }
        self.scroll.scroll_to_bottom();
    }

    /// Deliver what the last run never reached.
    ///
    /// The user pressed the send-now chord and the run ended before reading
    /// it, so the intent to send stands: the text goes back to the waiting
    /// section *with its turn promised again*, the way any submit promises
    /// one — a prompt standing in a section whose whole meaning is "this
    /// runs next", with nothing to make it run, would be a decoration. It
    /// stays editable up to its own claim: `e` fixes the wording, `d`
    /// takes it out, and a cancel stops only the turn in flight — the
    /// waiting prompts, this one included, keep their place.
    ///
    /// It goes to the *back* of the queue rather than the front, though it
    /// is the older text: the prompts already waiting were promised their
    /// turns first, and reordering them behind an unsent aside would be the
    /// louder surprise.
    ///
    /// A closed channel is the session ending, and the entry simply stays
    /// in the queue — an unrun prompt still held in the queue is the honest last state.
    /// The take is not atomic with the caller's idle check, and the gap is
    /// the window a run starts in: the frame sees no run in flight, and by
    /// the time the slot is emptied a new run is mid-turn. The texts are
    /// the finished run's, though — advice about turns that no longer
    /// stand, which the slot's own contract refuses to hand to a later
    /// run silently — so whatever started in the window, they wait here
    /// with a turn promised, under the notice that says the model never
    /// read them. The new run is not steered with them: a user who meant
    /// them for it still can, seeing the notice, and the alternative
    /// would put words into a conversation their author never aimed at.
    pub(crate) fn return_guidance(&mut self) {
        let mut taken = self
            .state
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or_else(Vec::new, |guidance| guidance.take_returned());
        if taken.is_empty()
            && let Some(guidance) = self.host.guidance.as_ref()
        {
            taken = guidance.take_returned();
        }
        if taken.is_empty() {
            return;
        }
        self.state
            .pending_steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|text| !taken.contains(text));
        let count = taken.len();
        let mut promised = 0usize;
        for text in taken {
            let id = self.push_queued(text);
            if self.send_queued_id(id) {
                promised = promised.saturating_add(1);
            }
        }
        self.clamp_queue_view();
        let (noun, verb) = if count == 1 {
            ("prompt", "is")
        } else {
            ("prompts", "are")
        };
        let mut notice = format!("{count} {noun} the model never read {verb} waiting again");
        if promised < count {
            notice.push_str(" — no driver left to run them");
        }
        self.post_notice(notice);
    }

    /// The steers a run accepted but no turn boundary has read, as
    /// user rows pinned just above the live lane's timer.
    ///
    /// Each held steer renders exactly as a settled user turn renders
    /// — a breathing row, wrapped text in the user's own color,
    /// another breathing row — but just above the running turn's
    /// timer, in the spot the boundary that reads it will land it in:
    /// the model has not read the words, so they hold no place in the
    /// conversation yet, and nothing jumps when they graduate. A run
    /// that ends unread hands them to the waiting section instead,
    /// and the pin empties with that return.
    pub(crate) fn pending_steer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let steers = self
            .state
            .pending_steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut lines = Vec::new();
        for text in steers {
            lines.push(Line::default());
            for segment in text.split('\n') {
                lines.extend(plain_wrapped_lines(
                    segment,
                    usize::from(width.max(1)),
                    self.theme.ui.user_message_fg,
                ));
            }
            lines.push(Line::default());
        }
        lines
    }
}

/// The waiting-prompt section's state: which prompt it shows, the
/// posture it stands in, and whose hands the keyboard is in.
///
/// Everything the queue panel keeps between frames — the walk's
/// display position, the standing-or-folded posture, the focused,
/// confirming, and editing keyboard modes, the handoff hide for a
/// prompt one channel hop from its claim, and the section's screen
/// rect. The queue itself lives in the shared state; this is the
/// panel's own view over it.
pub(crate) struct QueueState {
    /// Which waiting prompt the queue section shows.
    ///
    /// A display position rather than an identity: the ids that survive a
    /// shift are the queue's business, and this is only the row the arrow
    /// keys last stood on. [`clamp_queue_view`](crate::app::TuiApp::clamp_queue_view)
    /// keeps it inside the queue, because a prompt claimed at the front
    /// pulls every row behind it up a place.
    pub(crate) queue_view: usize,

    /// The waiting section's posture, as `Ctrl+Q` or a click leaves
    /// it.
    ///
    /// A view preference, not queue state: the chord — or a click on
    /// the section — folds the section to its one-row badge and lifts
    /// it back, and the folded row names the count and the chord so a
    /// hidden backlog is still a fact on screen. The fold never
    /// survives into the section's own modes — the fold opens only on
    /// `Ctrl+Q` or a click — the walk's chord runs inert over a fold
    /// — and an edit or a confirm rides the full section — so a
    /// folded posture under a non-plain mode is a display relic the
    /// render refuses to honor.
    pub(crate) queue_posture: QueuePosture,

    /// Whose hands the keyboard is in, as far as waiting prompts go.
    ///
    /// [`QueueMode`] is the whole story: the composer's, the section's, or
    /// a prompt out for editing. The mode cannot outlive the section — an
    /// emptied queue takes the keys back by itself, since a mode that
    /// swallows every key for a panel no longer drawn is indistinguishable
    /// from a hung terminal.
    pub(crate) queue_mode: QueueMode,

    /// The prompt whose claim is one channel hop away, hidden from the
    /// queue section for that window.
    ///
    /// A submit into an idle session pushes the prompt into the shared
    /// queue and hands its id to the driver; the claim lands a moment
    /// later, and a frame drawn in that window would show the prompt
    /// under the QUEUED title — the flash every fresh session's first
    /// prompt makes. The recorded id is that window's row: the queue
    /// was empty when it was pushed, so it is the section's last row,
    /// and the display simply pays one fewer row until the driver
    /// takes it. The next submit clears it before pushing (at most one
    /// row hides at a time), and a claim or drop retires it through
    /// [`settle_handed`](crate::app::TuiApp::settle_handed) at the next frame.
    pub(crate) handed: Option<u64>,

    /// The rect the last frame painted the waiting section into.
    ///
    /// A click folds or lifts the section; the render records the
    /// rect it filled — standing rows or the folded badge alike — and
    /// the layout clears it when the queue empties, so a press only
    /// ever answers against geometry the last frame actually painted.
    pub(crate) queue_section_rect: Rect,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            queue_view: 0,
            queue_posture: QueuePosture::Standing,
            queue_mode: QueueMode::Plain,
            handed: None,
            queue_section_rect: Rect::default(),
        }
    }
}

impl QueueState {
    /// Flip the waiting section between its two postures.
    ///
    /// Standing folds to the one-row badge and a fold lifts back to
    /// the standing section; the chord and a click on the section
    /// share the flip, and the queue keeps updating underneath
    /// either posture.
    fn toggle_queue_posture(&mut self) {
        self.queue_posture = match self.queue_posture {
            QueuePosture::Standing => QueuePosture::Folded,
            QueuePosture::Folded => QueuePosture::Standing,
        };
    }
}
/// The most rows the waiting-prompt section can hold when drawn.
///
/// The cap, not the count: the section reserves only the rows it
/// paints — the air above it, the title row carrying the badge that
/// names it, the air standing that badge off the prompt below it,
/// and the rows the shown prompt wraps into. A one-line wording ends
/// the section at its own last row; no blank rides beneath it, so the
/// space under the queue is the frame's own furniture rather than the
/// section's margin. Two prompt rows are the most a wording may take —
/// one prompt at a time, counting the rest rather than growing
/// downward, because the rows it takes come straight out of the
/// conversation: a backlog must not cost a row per prompt of the
/// transcript the user is reading to decide what to do about it — and
/// the two blank rows are what stop a filled pill reading as riveted
/// to its neighbors: the transcript it overlays, the wording it
/// labels.
pub(crate) const QUEUE_ROWS: u16 = 5;

/// The glyph marking the waiting-prompt section's rows.
///
/// Short on purpose: the marker shares its row with the prompt's wording,
/// and everything a prefix costs is wording squeezed off the end. Both the
/// renderer and the tests that read a frame locate the section by this
/// glyph, so it is the section's identity on screen rather than ornament.
/// The settled hourglass (U+231B), kin to the running call's flowing one
/// (⏳): one sand-clock family, a shape at rest reading as waiting beside
/// the in-flight clock it never pretends to be. Both twins measure two
/// cells wide, and the wrap that sizes the section measures display width,
/// so the wider marker costs wording columns, never rows.
pub(crate) const QUEUE_MARKER: &str = "\u{231b}";

/// The title row the waiting-prompt section opens with.
///
/// Rendered as a badge — the word on a filled pill, one cell proud of
/// its letters on either side, filled with the accent the composer's
/// edge column wears — at the section's inset, the same left margin
/// the plan section's rows carry, so the prompts waiting behind the
/// turn in flight are a named region of the screen standing in one
/// family with the plan rather than a strip a first-time reader must
/// decode from the hourglass alone. It rides the section's first row,
/// flush with the prompt rows' left edge, and exists only while
/// prompts wait — an empty queue is no panel, title included. The
/// section's chords ride beside it on the same row, dim after the
/// pill: the badge row is short and always drawn, where the prompt
/// rows wrap and a right-edge hint would vanish exactly when the
/// terminal narrows.
pub(crate) const QUEUE_TITLE: &str = "QUEUED";

/// The prefix the shown prompt's first row carries.
///
/// The marker alone when one prompt waits, the marker with its place
/// in the line when more do. Both the section's renderer and the
/// height the layout reserves for it measure the prompt's wrap
/// against this prefix, so the rows reserved are the rows drawn —
/// the wrap the height counted on is the wrap the frame sees.
pub(crate) fn queue_prompt_prefix(total: usize, shown: usize) -> String {
    if total > 1 {
        format!("{QUEUE_MARKER} [{}/{}] ", shown.saturating_add(1), total)
    } else {
        format!("{QUEUE_MARKER} ")
    }
}

/// The waiting section's posture, as `Ctrl+Q` or a click on the
/// section leaves it.
///
/// One chord — or one click on the section — walks the two: standing
/// at whatever height the waiting prompt's wording needs, or folded
/// to the one-row badge that keeps a hidden backlog a fact on screen.
/// Like the plan's postures this is a view preference — the queue
/// keeps updating underneath both, and an emptied queue closes the
/// section out of either one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueuePosture {
    /// The section at its full height, the waiting prompt's wording
    /// across its rows.
    ///
    /// The resting posture: the section exists to be read, and the
    /// words are what get read.
    Standing,

    /// Folded to the one-row badge naming the count and the chord.
    ///
    /// The conversation takes the rows back; the badge keeps the
    /// backlog visible as a figure, and `Ctrl+Q` lifts
    /// the section back to standing.
    Folded,
}

/// Which destructive act the section's confirmation is holding.
///
/// Named rather than a flag: the two acts spend different things —
/// one sends the prompt into the run in flight, the other takes it
/// out of the queue for good — and the ask's wording is the last
/// chance to say which before it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueAct {
    /// Send the shown prompt to the running turn as guidance.
    ///
    /// The gentler spend: the prompt leaves the queue and joins the
    /// conversation the model is already having, read at its next
    /// boundary rather than waiting for its own turn.
    Steer,

    /// Take the shown prompt out of the queue for good.
    ///
    /// The irreversible spend: the prompt never runs, its words never
    /// reach the model, and the queue closes up behind it.
    Drop,
}

/// Whose hands the keyboard is in, as far as waiting prompts go.
///
/// One value rather than a focus flag beside an edit flag, because the two
/// flags could stand together and nothing in the code would have been able
/// to tell what a press meant: the section cannot hold the keys while its
/// prompt is out being typed into — the typing is precisely what the section
/// would have to give up to let it happen. The states the keys actually have
/// are three, so three is what is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueMode {
    /// The composer holds the keys; the section only shows what waits.
    ///
    /// The resting state, and the only one a session with nothing waiting
    /// can be in: the keys go where they went before the section existed,
    /// and its rows, when there are any, are reading matter.
    Plain,

    /// The section holds the keys: the arrows walk, `e` lifts, `d` drops,
    /// and everything else is swallowed.
    ///
    /// Reachable only with something waiting — the chord is refused on an
    /// empty queue rather than parking the user on a list with no rows —
    /// and Esc gives the keys back. Nothing leaks from in here: a letter
    /// typed over the backlog has nowhere it should go, and assembling a
    /// draft behind the user's back is what the swallowing exists to stop.
    Focused,

    /// The ask between a destructive key and its act.
    ///
    /// `s` and `d` are one keypress away from sending a prompt into a
    /// running turn or spending it entirely, so the press asks first:
    /// `y` does the act the ask names, anything else stands it down
    /// without spending anything — a stray key can cancel, never
    /// confirm. The ask holds the keyboard while it stands, for the
    /// same reason the walk does: a key that moved what `y` was about
    /// to act on would change the answer under the finger already
    /// resting on it.
    Confirming {
        /// Which act the ask is holding.
        ///
        /// The answer `y` performs and the ask's wording names — the
        /// one fact the ask exists to settle before anything is spent.
        act: QueueAct,

        /// The prompt the ask was armed against.
        ///
        /// A claim can take the prompt between the arming press and
        /// `y`, sliding a different prompt into its display position.
        /// The confirm resolves against this id — if the prompt has
        /// left the queue, `y` stands down without acting on whatever
        /// shifted into its place.
        id: u64,
    },

    /// A prompt's body is out in the composer, held for the prompt this id
    /// names.
    ///
    /// Enter saves into that slot. Esc and Ctrl+C put the draft away,
    /// leaving the prompt in the queue with the wording it had before the
    /// composer took it.
    /// On save the id may name a prompt the run took in the meantime — that
    /// edge is the commit's to settle, not a state to model. The claim that
    /// takes it often empties the queue outright, and the mode outlives that:
    /// the text in the composer is the user's, held for a prompt whose turn is
    /// already running, which is precisely when a correction is worth sending.
    Editing {
        /// The waiting prompt whose text the composer is holding.
        ///
        /// An entry, not a copy of one: the prompt stays in the queue while
        /// its body is out, so this id is how a save finds the slot to write
        /// back into. A run can claim it in the meantime, and then there is
        /// no slot to write to — the text goes out as a correction to the
        /// turn that took it, which is why the id outlives the entry.
        id: u64,
    },
}

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::message::TuiMessage;

    #[test]
    fn a_fresh_session_first_prompt_never_shows_as_queued() {
        // The handoff window: a submit into an idle session is the
        // driver's very next claim, one channel hop away, and the
        // queue section must not draw the prompt as waiting in
        // between — the flash of QUEUED a fresh session's first
        // prompt made. The shared queue still holds the prompt for
        // the driver; only the display pays one fewer row.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        subject.host.submit_tx = Some(tx);
        subject.submit_text("first prompt".to_string());
        assert_eq!(
            subject.queued_len(),
            1,
            "the driver finds the prompt in the shared queue"
        );
        assert_eq!(
            subject.queued_display_len(),
            0,
            "the handoff window hides the row from the section"
        );
        assert_eq!(
            subject.queue_section_height(60),
            0,
            "no rows to show — no section, no flash"
        );
        // The claim lands: the prompt leaves the queue and the next
        // frame retires the hide.
        let claimed = subject
            .remove_queued(0)
            .expect("the first pushed id is the driver's claim");
        assert_eq!(claimed, "first prompt");
        subject.settle_handed();
        assert!(
            subject.queue.handed.is_none(),
            "the window died with the claim"
        );
        // A busy session queues visibly: a run in flight means the
        // new row genuinely waits, and waiting rows show.
        subject
            .state
            .live_claim
            .store(7, std::sync::atomic::Ordering::SeqCst);
        subject.submit_text("second prompt".to_string());
        assert_eq!(
            subject.queued_display_len(),
            1,
            "genuinely waiting rows show under the title"
        );
    }

    #[test]
    fn a_resume_drops_the_waiting_queue_before_the_send() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // A prompt waiting in the queue belongs to the conversation being
        // left; the resume drops it before the send so the driver's next
        // claim cannot race the resumed session's first turn.
        app.state
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("a waiting prompt".to_string());
        let id = uuid::Uuid::new_v4().to_string();
        let out = command_output(&mut app, &format!("/resume {id}"));
        assert!(out.contains("resuming the session…"), "got {out:?}");
        assert!(
            app.state
                .queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the waiting prompts do not travel to the resumed session"
        );
    }

    #[test]
    fn a_resumed_session_re_seeds_the_transcript_and_empties_the_queue() {
        // The display half of the live swap: what arrives over the session
        // channel replaces the conversation wholesale, drops the prompts
        // waiting for the ended one, and restores the counters to the
        // saved session's accounting.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.seed_messages(vec![TuiMessage::System {
            text: "the old conversation".to_string(),
            timestamp: chrono::Utc::now(),
        }]);
        app.state
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("waiting on the old session".to_string());
        app.apply_session_update(crate::queue::SessionUpdate::Resumed {
            id: "resumed-id".to_string(),
            transcript: vec![TuiMessage::System {
                text: "the resumed conversation".to_string(),
                timestamp: chrono::Utc::now(),
            }],
            tokens: crate::queue::ResumedTokens {
                cumulative_input: 11,
                cumulative_output: 7,
                last_input: 5,
                context_tokens: 3,
            },
            permission_mode: dch_config::PermissionMode::Auto,
        });
        let texts: Vec<String> = app
            .conversation
            .iter()
            .filter_map(|message| match message {
                TuiMessage::System { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["the resumed conversation".to_string()],
            "the loaded transcript is the conversation now"
        );
        assert!(
            app.state
                .queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "prompts waiting for the ended conversation do not survive"
        );
        assert_eq!(app.host.session_id.as_deref(), Some("resumed-id"));
        let (cumulative_input, context_tokens) = {
            let counts = app
                .state
                .tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (counts.cumulative_input, counts.context_tokens)
        };
        assert_eq!(cumulative_input, 11, "the spend continues the save's count");
        assert_eq!(context_tokens, 3, "the usage bar opens on the saved fill");
    }
}
