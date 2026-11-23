//! The exit path: what the session hands back when the loop ends —
//! the waiting prompts' echo, the rescued corrections, the live
//! lanes' capture, and the cancel plumbing the stop settles through.

use super::TuiApp;
use crate::message::{ContentBlock, TuiMessage};
use std::sync::Arc;

impl TuiApp {
    /// The waiting prompts' texts, as the exit finds them.
    ///
    /// Read after the shutdown flag rises and before the exit's cancel
    /// fires, the one window in which every entry in the shared queue
    /// is a prompt no run ever claimed: the run in flight took its own
    /// entry when it started, the exit's requeues land only after the
    /// cancel, and a claim reaching the driver inside the window
    /// declines under the queue's own lock rather than taking its
    /// entry. The steers still pinned at the transcript's edge join
    /// them — words the user aimed at a run that read none of them,
    /// and whose seam never handed them back, or the pin would have
    /// emptied with that return — taken off the pin here so they hold
    /// one home from this moment. The snapshot is therefore exactly
    /// the set of words the user typed that no turn spent — the set
    /// the transcript owes rows to, because the queue they wait in
    /// dies with the process.
    #[must_use]
    pub fn waiting_prompt_texts_at_exit(&mut self) -> Vec<String> {
        if let Some(texts) = self.exit.unclaimed_prompts.take() {
            return texts;
        }
        let queue = self
            .state
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut texts = (0..queue.len())
            .filter_map(|index| queue.get(index).map(|entry| entry.text.clone()))
            .collect::<Vec<_>>();
        drop(queue);
        texts.extend(std::mem::take(
            &mut *self
                .state
                .pending_steers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ));
        texts
    }

    /// Write the still-waiting prompts into the transcript as
    /// unclaimed rows.
    ///
    /// The one place a user row is written without a send behind it:
    /// the queue is process state, the process is leaving, and words
    /// the user typed would otherwise leave no trace anywhere — the
    /// interrupted claim keeps the row its send earned, but a prompt
    /// no turn took has nothing. The rows land after the stop's own
    /// record, in queue order, and stand as the durable record of
    /// words no turn spent: the queue they waited in dies with the
    /// process, and a resume reads the rows as history rather than
    /// resurrecting them into a queue nothing can run. Each row is
    /// stamped `unsent`, so the renderer dims it and the claim rules
    /// leave it out — it is a record, not a delivery. The flush
    /// beside them is the same unconditional persist the settle
    /// performs — an exit with no
    /// run in flight skips the settle entirely, and these rows must
    /// outlive that exit too.
    pub fn echo_waiting_prompts_at_exit(&mut self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        for text in texts {
            self.conversation.push(TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: true,
            });
        }
        self.flush_turn_end();
    }

    /// Write the corrections the exit collected into the transcript
    /// as unclaimed rows.
    ///
    /// The drain half of the exit's correction rescue: the driver
    /// collects each correction still buffered at its exit breaks —
    /// words that left the queue with the claim, travelled only in
    /// the channel, and would otherwise leave no trace anywhere — and
    /// this runs once the driver task has settled, after the waiting
    /// prompts' own echo, so the transcript ends on everything the
    /// user typed that no turn spent. The flush beside the rows is
    /// the same unconditional persist the settle performs; the rows
    /// stand as the record, and a resume leaves them as history.
    pub fn echo_rescued_corrections_at_exit(&mut self) {
        let texts: Vec<String> = std::mem::take(
            &mut *self
                .state
                .rescued_corrections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if texts.is_empty() {
            return;
        }
        for text in texts {
            self.conversation.push(TuiMessage::User {
                text,
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: true,
            });
        }
        self.flush_turn_end();
    }

    /// Arm the exit's pre-cancel snapshots: the partial reply and the
    /// waiting prompts exactly as they stand at this instant.
    ///
    /// Both captures must precede the cancel an exit fires. The
    /// failed-turn observer clears the streaming buffer without
    /// graduating its partial text, so a snapshot taken after the
    /// cancel reads the wreckage rather than the words; and the
    /// driver's cancel handling can requeue a prompt the run already
    /// claimed, so a queue read after the cancel double-counts it.
    /// Called at the moment the exit is confirmed, before any cancel
    /// leaves the display.
    pub(crate) fn snapshot_exit_state_before_cancel(&mut self) {
        let blocks = self.capture_live_lanes();
        if !blocks.is_empty() {
            self.exit.partial_reply = Some(blocks);
        }
        self.exit.unclaimed_prompts = Some(self.waiting_prompt_texts_at_exit());
    }

    /// Land an exit-confirmed stop as the record a cancel leaves.
    ///
    /// An exit confirmed under a running agent is a cancel first:
    /// [`cancel_run_in_flight`](Self::cancel_run_in_flight) fires
    /// while the frame loop still lives, and frames pump until the
    /// run settles — the stop drains into the conversation as the
    /// same record an in-app cancel leaves, the turn-end hook
    /// persists it, and the resumed session reads one shape whichever
    /// way the run was stopped. The partial reply rides along when
    /// the drain did not already graduate it — the snapshot the
    /// confirming press took, or the one taken here before this
    /// cancel, keeps text the failed-turn observer would otherwise
    /// clear without a trace — and a drain that did graduate the
    /// reply wins, so the row is never duplicated. With nothing
    /// running this is a no-op.
    pub fn settle_exit_stop(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    ) {
        if self.exit.partial_reply.is_none() {
            let blocks = self.capture_live_lanes();
            if !blocks.is_empty() {
                self.exit.partial_reply = Some(blocks);
            }
        }
        if !self.cancel_run_in_flight() {
            self.exit.partial_reply = None;
            return;
        }
        let before = self.conversation.len();
        drop(terminal.draw(|frame| self.render(frame)));
        self.drain_shared_state();
        let reply_graduated = self.conversation.get(before..).is_some_and(|landed| {
            landed
                .iter()
                .any(|message| matches!(message, TuiMessage::Assistant { .. }))
        });
        if !reply_graduated && let Some(blocks) = self.exit.partial_reply.take() {
            self.conversation.push(TuiMessage::Assistant {
                blocks,
                timestamp: chrono::Utc::now(),
                duration_ms: None,
            });
        } else {
            self.exit.partial_reply = None;
        }
        let record_landed = self.conversation.get(before..).is_some_and(|landed| {
            landed
                .iter()
                .any(|message| matches!(message, TuiMessage::Error { text, .. } if text == "Agent cancelled"))
        });
        if !record_landed {
            self.conversation.push(TuiMessage::Error {
                text: "Agent cancelled".to_string(),
                timestamp: chrono::Utc::now(),
            });
        }
        self.flush_turn_end();
    }

    /// Snapshot the run's unfinished lanes.
    ///
    /// The thinking and the partial reply text the run had begun, as
    /// they stand at the moment of the call. A cancelled run
    /// graduates nothing, and the failed-turn path clears the
    /// streaming buffer without a trace, so the exit reads this
    /// before its cancel fires and keeps what it finds as the turn's
    /// own assistant row.
    fn capture_live_lanes(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        let thinking = self
            .state
            .thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !thinking.text.trim().is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: thinking.text.clone(),
                redacted: false,
                duration_ms: 0,
            });
        }
        drop(thinking);
        let stream = self
            .state
            .streaming_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !stream.trim().is_empty() {
            blocks.push(ContentBlock::Text { text: stream });
        }
        blocks
    }

    /// Fire the run-cancel at the runner the session is running now.
    ///
    /// Every cancel the app lands — Esc-Esc, `/stop`, a correction's,
    /// the exit handshake — goes through the shared signal cell here,
    /// the one a runner rebuild swaps, so the stop reaches the live
    /// runner and never the startup runner's dead handle. The boxed
    /// canceller rides along after it: at startup it names the same
    /// signal, and a mode or test that installed a counting sink
    /// still sees the fire.
    pub(crate) fn fire_cancel(&mut self) {
        self.state
            .cancel_signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
        if let Some(cancel) = &self.exit.run_canceller {
            cancel();
        }
    }

    /// Cancel the run in flight, when one is.
    ///
    /// The exit path's handshake: an exit confirmed under a running
    /// agent fires the canceller here — while the frame loop still
    /// drains — so the stop lands as the same record an in-app
    /// cancel leaves, with the turn's end hook persisting it, before
    /// the teardown owns the screen. Returns whether a run was in
    /// flight to stop.
    pub fn cancel_run_in_flight(&mut self) -> bool {
        if !self
            .state
            .agent_running
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.fire_cancel();
        true
    }

    /// Install the run canceller.
    ///
    /// The mode driver's half of the Ctrl+C contract: while
    /// [`agent_running`](crate::observer::TuiObserverState::agent_running) is set, a
    /// Ctrl+C press on an empty composer calls this to stop the
    /// submission in flight.
    pub fn set_run_canceller(&mut self, canceller: Box<dyn Fn()>) {
        self.exit.run_canceller = Some(canceller);
    }

    /// Install the shared shutdown flag the engine driver gates on.
    ///
    /// The display's half of the exit contract: the press that fires
    /// the quit ladder stores this flag before calling the run
    /// canceller, so the driver — whose next claim the exit's own
    /// cancel would otherwise release — refuses every submission
    /// queued behind a quitting user. An ordinary cancel, Esc-Esc
    /// or a correction's, never touches it: those release the queue
    /// by design.
    pub fn set_shutdown_flag(&mut self, flag: Arc<std::sync::atomic::AtomicBool>) {
        self.exit.shutdown_flag = flag;
    }
}

/// The exit path's state: the shutdown flag, the installed
/// canceller, and the cancel ladder's arm.
///
/// The three facts the exit contract rides on — the flag the driver
/// gates every claim on, the host's canceller the shared signal
/// fires beside, and the one-press arm that makes Ctrl+C Ctrl+C a
/// deliberate double press rather than a slip.
#[derive(Default)]
pub(crate) struct ExitState {
    /// The flag the engine driver gates every claim and start on.
    ///
    /// The exit's share of the quit contract: the press that fires
    /// the ladder raises this before calling the canceller, so the
    /// cancel the exit itself causes cannot release the driver's
    /// claim on the queue's next prompt — a claim made after the
    /// fire spends a prompt the exit still believes waiting, and
    /// resumes it forever as a stop no one asked for. Hosts that
    /// never install one (tests, headless) carry a flag nobody
    /// raises.
    pub(crate) shutdown_flag: Arc<std::sync::atomic::AtomicBool>,

    /// How a double Esc cancels the run in flight, when one is.
    ///
    /// Installed by the mode driver alongside the shared running
    /// flag; absent in tests and headless hosts, where a cancel
    /// press simply reports handled.
    pub(crate) run_canceller: Option<Box<dyn Fn()>>,

    /// Whether one Esc has armed the run cancel.
    ///
    /// True only between the arming press and the Esc that confirms
    /// it or any other key that stands it down — cancel is a
    /// deliberate double press, so one stray Esc cannot kill a run
    /// mid-flight.
    pub(crate) cancel_arm: bool,

    /// The unfinished lanes as they stood when the exit's cancel
    /// fired, when a run was in flight.
    ///
    /// Snapshot at the confirming press, before the cancel leaves the
    /// display: the failed-turn observer clears the streaming buffer
    /// without graduating its partial text, so a later read records
    /// the wreckage instead of the words. Consumed by
    /// [`settle_exit_stop`](Self::settle_exit_stop), which keeps it
    /// only when the drain did not already graduate the reply.
    pub(crate) partial_reply: Option<Vec<ContentBlock>>,

    /// The waiting prompts as they stood when the exit's cancel
    /// fired.
    ///
    /// Snapshot at the confirming press for the same reason: the
    /// driver's cancel handling can requeue a prompt the run already
    /// claimed, and a queue read after the cancel double-counts it.
    /// [`waiting_prompt_texts_at_exit`](Self::waiting_prompt_texts_at_exit)
    /// serves this copy first and reads the live queue only when no
    /// exit armed one.
    pub(crate) unclaimed_prompts: Option<Vec<String>>,
}
/// The quit lifecycle's two halves.
///
/// Exit is Ctrl+C pressed twice on an empty buffer: the first
/// arms, the second confirms and requests the exit. The press
/// that clears a non-empty buffer arms nothing — clearing is not
/// consenting — and any other key or mouse press disarms, so the
/// chord never fires from stale intent.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct QuitState {
    /// Whether one press has armed the exit.
    ///
    /// True only between the empty-buffer chord press and the key
    /// that confirms or disarms it.
    pub(crate) armed: bool,

    /// Whether the second press confirmed it.
    ///
    /// The run loop leaves at the top of its next iteration.
    pub(crate) requested: bool,
}

impl QuitState {
    /// Record a first press.
    ///
    /// Called only for the chord's own spelling on an empty buffer;
    /// the press that clears a draft never reaches here.
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }

    /// Take back a first press.
    ///
    /// Any key that is not the chord's own spelling disarms it.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    #[test]
    fn stop_with_nothing_running_says_so_and_does_not_quit() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/stop");
        assert!(out.contains("nothing is running"), "got {out:?}");
        // Stopping a run must not ask the loop to leave the session.
        assert!(!app.quit.requested);
    }

    #[test]
    fn exit_command_requests_the_quit() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        command_output(&mut app, "/exit");
        assert!(app.quit.requested);
    }

    #[test]
    fn a_stolen_multibyte_lead_is_named_not_mangled() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // 0xE2 leads the three-byte spelling of an em dash; the query's
        // reader took only the first byte, so no character can be built
        // from it — the composer must stay empty and the notice must
        // name the loss rather than plant a Latin-1 look-alike.
        app.return_stolen_byte(0xE2);
        assert!(
            app.composer.input.is_empty(),
            "no fabricated character lands in the composer"
        );
        let notice = app
            .transient_notice
            .as_ref()
            .map_or_default(|(text, _)| text.clone());
        assert!(
            notice.contains("multi-byte"),
            "the loss is named honestly: {notice:?}"
        );

        // An ASCII printable byte still lands as if typed, control or not.
        app.return_stolen_byte(b'a');
        app.return_stolen_byte(b' ');
        assert_eq!(
            app.composer.input.text(),
            "a ",
            "printable ASCII arrives exactly as typed"
        );
        app.return_stolen_byte(0x01);
        let notice = app
            .transient_notice
            .as_ref()
            .map_or_default(|(text, _)| text.clone());
        assert!(
            notice.contains("was consumed") && !notice.contains("multi-byte"),
            "a control byte is named as itself: {notice:?}"
        );
    }

    #[test]
    fn a_stolen_esc_says_nothing() {
        // The one silent case: Esc pressed inside the terminal query's
        // window — the half-second a committed theme switch spends
        // asking the terminal about its canvas — is a habitual
        // dismissal, not information. The keystroke is swallowed
        // without a notice: the /theme-plus-Esc flow clears its command
        // and the screen announces nothing, because the press carried
        // nothing worth announcing and the query window already cost
        // the moment.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.return_stolen_byte(0x1b);
        assert!(
            app.transient_notice.is_none(),
            "a swallowed Esc posts no notice"
        );
        assert!(
            app.composer.input.is_empty(),
            "a swallowed Esc inserts nothing into the composer"
        );
        assert!(
            app.conversation.is_empty(),
            "a swallowed Esc writes no message to the transcript"
        );
    }
}
