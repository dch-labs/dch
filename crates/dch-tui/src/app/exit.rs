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
            self.exit.partial_reply_since = self.conversation.len();
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
    /// reply wins, so the row is never duplicated: graduation is
    /// judged over everything landed since the snapshot was taken,
    /// because a run that settles between the confirming press and
    /// this stop can graduate its reply through an ordinary frame
    /// before the settle ever runs. Graduation is read by content:
    /// reply text landing supersedes the snapshot whole, while
    /// reasoning landing alone — the failed turn's thinking-only row —
    /// supersedes only the snapshot's reasoning, and its words still
    /// land, so the reasoning never appears twice and the text never
    /// vanishes beside its own graduated thinking. A run that already
    /// stopped before this moment still owes its record — the
    /// snapshot exists precisely for the press that cancelled it — so
    /// only an idle exit, with neither a run to stop nor a snapshot
    /// to land, returns without writing.
    pub fn settle_exit_stop(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    ) {
        if self.exit.partial_reply.is_none() {
            let blocks = self.capture_live_lanes();
            if !blocks.is_empty() {
                self.exit.partial_reply_since = self.conversation.len();
                self.exit.partial_reply = Some(blocks);
            }
        }
        let cancelled_now = self.cancel_run_in_flight();
        if !cancelled_now && self.exit.partial_reply.is_none() {
            return;
        }
        drop(terminal.draw(|frame| self.render(frame)));
        self.drain_shared_state();
        let graduated_from = self.exit.partial_reply_since.min(self.conversation.len());
        let graduated = self
            .conversation
            .get(graduated_from..)
            .map_or((false, false), |landed| {
                landed
                    .iter()
                    .fold((false, false), |(text, thinking), message| match message {
                        TuiMessage::Assistant { blocks, .. } => (
                            text || blocks
                                .iter()
                                .any(|block| matches!(block, ContentBlock::Text { .. })),
                            thinking
                                || blocks
                                    .iter()
                                    .any(|block| matches!(block, ContentBlock::Thinking { .. })),
                        ),
                        _ => (text, thinking),
                    })
            });
        let (reply_text_graduated, reasoning_graduated) = graduated;
        if reply_text_graduated {
            self.exit.partial_reply = None;
        } else if let Some(blocks) = self.exit.partial_reply.take() {
            let kept: Vec<ContentBlock> = if reasoning_graduated {
                blocks
                    .into_iter()
                    .filter(|block| matches!(block, ContentBlock::Text { .. }))
                    .collect()
            } else {
                blocks
            };
            if !kept.is_empty() {
                self.conversation.push(TuiMessage::Assistant {
                    blocks: kept,
                    timestamp: chrono::Utc::now(),
                    duration_ms: None,
                });
            }
        }
        let record_since = self.exit.partial_reply_since.min(self.conversation.len());
        let record_landed = self.conversation.get(record_since..).is_some_and(|landed| {
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

    /// Drain what the driver published after the settle's own drain.
    ///
    /// The exit's stop drains the shared cells once and flushes, but
    /// the driver task lives on through its bounded teardown — up to
    /// half a second in which a tool that completed during the
    /// shutdown lands its graduation in the cells with no frame left
    /// to drain it, and a graduation no frame drains never reaches the
    /// persisted transcript. The teardown's end is the moment the
    /// stragglers are all in: this drain runs there, ahead of the
    /// rescued corrections' echo whose flush is among the
    /// transcript's last, so everything the driver published before
    /// the process leaves lands in the conversation the final persist
    /// writes. Public because the session's teardown owns the moment
    /// — the app's own exit path already drained, and only the host
    /// knows when the driver after it has settled.
    pub fn drain_late_graduations_at_exit(&mut self) {
        self.drain_shared_state();
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

    /// The conversation's length when [`Self::partial_reply`] was
    /// captured.
    ///
    /// The watermark a later settle graduates against: any assistant
    /// row carrying reply content that landed from this index on is
    /// the reply graduating through an ordinary frame — between the
    /// confirming press and the settle — and supersedes the snapshot,
    /// so the partial is never written beside its own graduated row.
    pub(crate) partial_reply_since: usize,

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
    use crate::message::{ContentBlock, TuiMessage};
    use crate::observer::{Graduation, ToolResultDisplay};
    use std::sync::atomic::Ordering;

    /// A terminal the settle can draw into without a screen.
    fn test_terminal() -> ratatui::Terminal<ratatui::backend::TestBackend> {
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
            .expect("the test terminal draws")
    }

    /// Put a run in flight with a partial reply on the wire.
    fn seed_partial_reply(app: &mut TuiApp, text: &str) {
        *app.state
            .streaming_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text.to_string();
        app.state.agent_running.store(true, Ordering::SeqCst);
    }

    /// What the failed-turn observer leaves behind: the stream cleared
    /// without a graduation, and the run no longer in flight.
    fn stop_as_a_failed_turn_does(app: &mut TuiApp) {
        app.state
            .streaming_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        app.state.agent_running.store(false, Ordering::SeqCst);
    }

    /// The text of every assistant row's reply blocks, in order.
    fn reply_texts(app: &TuiApp) -> Vec<String> {
        app.conversation
            .iter()
            .filter_map(|message| match message {
                TuiMessage::Assistant { blocks, .. } => Some(blocks),
                _ => None,
            })
            .flat_map(|blocks| {
                blocks.iter().filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect()
    }

    /// Whether the transcript carries the cancel's own record.
    fn cancel_record_landed(app: &TuiApp) -> bool {
        app.conversation.iter().any(|message| {
            matches!(message, TuiMessage::Error { text, .. } if text == "Agent cancelled")
        })
    }

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
    fn a_snapshot_outlives_a_run_that_stopped_before_the_settle() {
        // The confirming press snapshots the partial reply and fires
        // the cancel; the run can settle on another task before the
        // settle runs at all — the failed-turn observer clears the
        // stream without graduating, and nothing is in flight to
        // cancel. The snapshot exists for exactly that press: the
        // words the user saw land, and the cancel's record lands with
        // them, rather than both vanishing with the early return an
        // idle exit deserves.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half said");
        app.snapshot_exit_state_before_cancel();
        stop_as_a_failed_turn_does(&mut app);
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        assert_eq!(
            reply_texts(&app),
            ["half said"],
            "the press's snapshot lands as the turn's reply row"
        );
        assert!(
            cancel_record_landed(&app),
            "the stop that took the run down is recorded"
        );
    }

    #[test]
    fn a_cancel_record_drained_before_the_settle_is_not_duplicated() {
        // The press fires the cancel and the run loop draws one more
        // frame before the settle runs: the driver resolves the cancel
        // inside that window, and the ordinary frame drains the
        // "Agent cancelled" record into the conversation. The settle's
        // guard reads from the press's snapshot — the same window the
        // graduation check uses — so the landed record stands alone.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half said");
        app.snapshot_exit_state_before_cancel();
        app.conversation.push(TuiMessage::Error {
            text: "Agent cancelled".to_string(),
            timestamp: chrono::Utc::now(),
        });
        stop_as_a_failed_turn_does(&mut app);
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        let records = app
            .conversation
            .iter()
            .filter(|message| {
                matches!(message, TuiMessage::Error { text, .. } if text == "Agent cancelled")
            })
            .count();
        assert_eq!(
            records, 1,
            "the record an ordinary frame landed stands alone beside the snapshot"
        );
        assert_eq!(
            reply_texts(&app),
            ["half said"],
            "the press's snapshot still lands as the turn's reply row"
        );
    }

    #[test]
    fn a_tool_only_graduation_does_not_stand_in_for_the_reply() {
        // A tool in flight graduates in the cancel's drain: its row is
        // an assistant message carrying only a tool block. That is
        // not the reply graduating — the partial text still standing
        // in the snapshot must land beside it, not be discarded as
        // though the drain had already written the reply.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half said");
        app.snapshot_exit_state_before_cancel();
        app.state
            .graduations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Graduation::Tool(ToolResultDisplay {
                name: "Edit".to_string(),
                call_id: "call_exit".to_string(),
                is_error: false,
                duration: std::time::Duration::ZERO,
                input_summary: "src/lib.rs".to_string(),
                output_preview: "done".to_string(),
                full_input: "{\"k\":1}".to_string(),
                full_output: "done".to_string(),
            }));
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        assert_eq!(
            reply_texts(&app),
            ["half said"],
            "the tool row is not a graduation; the partial reply still lands"
        );
        assert!(
            app.conversation.iter().any(|message| matches!(
                message,
                TuiMessage::Assistant { blocks, .. }
                    if blocks.iter().any(|block| matches!(block, ContentBlock::Tool { .. }))
            )),
            "the tool's own row landed in the same drain"
        );
    }

    #[test]
    fn a_reply_graduated_since_the_snapshot_supersedes_it() {
        // A run that settles between the confirming press and the
        // stop can graduate its whole reply through an ordinary frame
        // first. The snapshot predates that row, so the stop must
        // read the graduation across everything landed since the
        // capture — not only across its own drain — and keep the
        // graduated reply alone, never the partial beside it.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half sa");
        app.snapshot_exit_state_before_cancel();
        app.conversation.push(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "half said and finished".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        stop_as_a_failed_turn_does(&mut app);
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        assert_eq!(
            reply_texts(&app),
            ["half said and finished"],
            "the graduated reply stands alone; the stale partial is not written beside it"
        );
    }

    #[test]
    fn a_settle_under_a_live_run_captures_the_partial_itself() {
        // A host that runs the stop without a press snapshot — the
        // settle is the first thing to touch the run — still reads
        // the lanes before its own cancel clears them.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half said");
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        assert_eq!(
            reply_texts(&app),
            ["half said"],
            "the settle's own capture lands the partial it found in flight"
        );
        assert!(
            cancel_record_landed(&app),
            "a live run is cancelled and recorded"
        );
    }

    #[test]
    fn a_reasoning_only_graduation_keeps_the_snapshots_words() {
        // Graduation read by content: the failed turn's own reasoning
        // lands through an ordinary frame — a thinking-only row, the
        // text cleared without graduating — between the confirming
        // press and the stop. The stop supersedes only what the landed
        // row carries: the snapshot's words land, its reasoning does
        // not — the graduated row already carries it — so nothing
        // appears twice and nothing vanishes.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half said");
        app.state
            .thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text = "thought first".to_string();
        app.snapshot_exit_state_before_cancel();
        app.conversation.push(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Thinking {
                text: "thought first".to_string(),
                redacted: false,
                duration_ms: 0,
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        stop_as_a_failed_turn_does(&mut app);
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        let texts = reply_texts(&app);
        assert_eq!(
            texts,
            ["half said"],
            "the snapshot's words land beside their graduated reasoning"
        );
        let thinkings = app
            .conversation
            .iter()
            .filter_map(|message| match message {
                TuiMessage::Assistant { blocks, .. } => Some(blocks),
                _ => None,
            })
            .flat_map(|blocks| {
                blocks.iter().filter_map(|block| match block {
                    ContentBlock::Thinking { text, .. } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            thinkings,
            ["thought first"],
            "the reasoning appears once, in the row that graduated it"
        );
    }

    #[test]
    fn a_late_graduation_reaches_the_transcript_after_the_teardown() {
        // The window the teardown owns: the stop drained and flushed,
        // the driver lives on through its bounded teardown, and a tool
        // completing in that window publishes its graduation into the
        // shared cells with no frame left to drain it. The drain that
        // follows the teardown lands it in the conversation the final
        // persist writes.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        seed_partial_reply(&mut app, "half said");
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        assert!(
            app.conversation.iter().all(|message| {
                !matches!(message, TuiMessage::Assistant { blocks, .. }
                    if blocks.iter().any(|block| matches!(block, ContentBlock::Tool { .. })))
            }),
            "before the late publication, no tool row stands"
        );
        // The driver's late publication, as the teardown window sees it.
        app.state
            .graduations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Graduation::Tool(ToolResultDisplay {
                name: "Bash".to_string(),
                call_id: "call_late".to_string(),
                is_error: false,
                duration: std::time::Duration::ZERO,
                input_summary: "cargo test".to_string(),
                output_preview: "done".to_string(),
                full_input: "{}".to_string(),
                full_output: "done".to_string(),
            }));
        app.drain_late_graduations_at_exit();
        assert!(
            app.conversation.iter().any(|message| {
                matches!(message, TuiMessage::Assistant { blocks, .. }
                    if blocks.iter().any(|block| matches!(block, ContentBlock::Tool { .. })))
            }),
            "the graduation published during the teardown reaches the transcript"
        );
    }

    #[test]
    fn an_idle_exit_writes_nothing() {
        // Nothing in flight and no snapshot: the settle is a no-op,
        // drawing no record the transcript never earned.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let mut terminal = test_terminal();
        app.settle_exit_stop(&mut terminal);
        assert!(
            app.conversation.is_empty(),
            "an idle exit lands no rows: {:?}",
            app.conversation
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
