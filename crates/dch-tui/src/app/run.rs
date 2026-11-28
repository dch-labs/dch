//! The TUI event loop: terminal input, the render notify, and the
//! periodic ticks selected against each other, a frame drawn only
//! when something changed and a full repaint claimed when the
//! screen and the buffer can no longer be assumed to agree.

use std::sync::Arc;
use std::time::Instant;

use ratatui::backend::CrosstermBackend;

use crate::events::TerminalEvents;

use super::TuiApp;
use super::questions::PendingQuestion;

use super::exit::QuitState;
use std::time::Duration;

impl TuiApp {
    /// Run the UI event loop until the user exits.
    ///
    /// Selects over terminal events, the shared notify (the path
    /// background state uses to request a redraw), and a periodic
    /// tick. A notify listener stays registered across draws and
    /// event handling, so a notification sent while the loop is busy
    /// is delivered on the next iteration rather than lost.
    /// Background redraws are frame-capped: a notify renders at most
    /// once per frame interval, a request arriving sooner parks
    /// itself for the tick to claim, and any queued graduation
    /// forces the frame. Input-driven redraws are not capped. The
    /// tick redraws while tool calls are in flight (keeping their
    /// elapsed stamps live), claims parked background requests, and
    /// otherwise draws nothing. The caller owns terminal setup and
    /// teardown.
    ///
    /// # Errors
    /// Propagates terminal draw failures; the caller's guard still
    /// restores the terminal.
    pub async fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.quit = QuitState::default();
        self.pacing.full_repaint_due = Some(Instant::now());
        let mut events = TerminalEvents::spawn();
        self.input_quiesce = Some(events.quiesce_handle());
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        let mut animation = tokio::time::interval(ANIMATION_INTERVAL);
        animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let notify = Arc::clone(&self.state.render_notify);
        let mut listener = notify.listen();
        let mut permission_rx = self.permission.permission_rx.take();
        let mut permission_closed = false;
        let mut question_rx = self.questions.question_rx.take();
        let mut question_closed = false;
        let mut session_rx = self.host.session_rx.take();
        let mut session_closed = false;

        terminal.draw(|frame| self.render(frame))?;
        if self.retry_the_startup_background_ask() {
            terminal.clear()?;
            terminal.draw(|frame| self.render(frame))?;
        }

        while !self.quit.requested {
            let mut needs_redraw = tokio::select! {
                maybe_event = events.recv() => {
                    match maybe_event {
                        Some(result) => match result {
                            Ok(event) => {
                                let mut handled = self.handle_event(&event);
                                handled |= self.drain_ready(|| events.poll())?;
                                handled
                            }
                            Err(err) => return Err(err.into()),
                        },
                        None => return Ok(()),
                    }
                }
                () = &mut listener => self.notify_wake(&notify, &mut listener),
                maybe_question = async {
                    match question_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(prompt) = maybe_question {
                        match question_rx.as_mut() {
                            Some(rx) => self.questions.land_question_requests(prompt, rx),
                            None => {
                                self.questions.pending_questions
                                    .push_back(PendingQuestion::new(prompt));
                            }
                        }
                        true
                    } else {
                        question_closed = true;
                        false
                    }
                }
                _instant = tick.tick() => self.tick_wake(Instant::now()),
                _anim = animation.tick() => {
                    let animating = self.pulse_animation_living();
                    if animating {
                        self.pacing.frame_stats.anim_wakes =
                            self.pacing.frame_stats.anim_wakes.saturating_add(1);
                    }
                    animating
                }
                maybe_request = async {
                    match permission_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(request) = maybe_request {
                        match permission_rx.as_mut() {
                            Some(rx) => self.permission.land_permission_requests(request, rx),
                            None => self.permission.pending_permissions.push_back(request),
                        }
                        true
                    } else {
                        permission_closed = true;
                        false
                    }
                }
                maybe_update = async {
                    match session_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(update) = maybe_update {
                        self.apply_session_update(update);
                        true
                    } else {
                        session_closed = true;
                        false
                    }
                }
            };
            if permission_closed {
                permission_rx = None;
                permission_closed = false;
            }
            if question_closed {
                question_rx = None;
                question_closed = false;
            }
            if session_closed {
                session_rx = None;
                session_closed = false;
            }
            if self.questions.retire_withdrawn_questions() {
                needs_redraw = true;
            }
            if self.take_full_repaint() {
                if let Err(err) = terminal.clear() {
                    tracing::warn!("full repaint clear failed: {err}");
                }
                needs_redraw = true;
            }
            if needs_redraw {
                let theme_burst = self.browse.pending_theme_osc.take();
                let took = self.draw_frame(terminal, theme_burst)?;
                if let Some(lane) = self.pacing.frame_stats.note_draw(took, Instant::now()) {
                    tracing::debug!(
                        target: "dch_tui::frames",
                        anim_wakes = lane.anim_wakes,
                        draws = lane.draws,
                        worst_draw_ms = lane.worst_draw_ms,
                        "frame lane"
                    );
                }
            }
        }
        Ok(())
    }

    /// Draw one frame, closing a parked theme burst's window whether
    /// the draw succeeds or fails.
    ///
    /// A parked burst brackets the frame's cell diff with the
    /// synchronized-output window so the theme's canvas write lands
    /// as the burst's final beat. The close is owed when the draw
    /// fails as well: the restore bytes the caller's terminal guard
    /// writes after the error must not sit inside a window that never
    /// ends — a terminal implementing the mode buffers a window's
    /// content until it closes and would never paint the restore — so
    /// the closing write goes out before the draw's error escapes.
    ///
    /// # Errors
    /// Propagates the terminal draw's error, with the burst's window
    /// close already written.
    pub(super) fn draw_frame(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
        theme_burst: Option<(ratatui::style::Color, ratatui::style::Color)>,
    ) -> std::io::Result<Duration> {
        if theme_burst.is_some() {
            crate::terminal::begin_synchronized_output();
        }
        let started = Instant::now();
        let drawn = terminal.draw(|frame| self.render(frame));
        let took = started.elapsed();
        if let Some((background, primary)) = theme_burst {
            crate::terminal::finish_theme_burst(background, primary);
        }
        drawn.map(|_| took)
    }
}
/// The minimum spacing between background-initiated frames.
///
/// Redraws requested through the shared notify are coalesced to at
/// most one per interval, so a burst of stream deltas costs one
/// frame whatever the token rate.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How long a frame-lane window stands before it reports and reopens.
///
/// One second: long enough that a slow frame shows up as a rate a
/// reader can act on rather than as one sample, short enough that the
/// lane answers a question asked about the moment just now. Chosen to
/// match the log line's own promise, which names the figure per second.
pub(crate) const FRAME_REPORT_SECONDS: std::time::Duration = std::time::Duration::from_secs(1);

/// The frame lane's per-second counters.
///
/// Reset each time a second's line is logged; each reopened window
/// opens at the draw that closed the last, and a lane that never
/// draws logs nothing at all.
#[derive(Default)]
pub(crate) struct FrameStats {
    /// Animation-lane wakes this second.
    ///
    /// Only beats whose gate found something animating are counted,
    /// so the figure measures drawn frames rather than raw timer
    /// ticks; it feeds the per-second frame-lane debug line and
    /// drops back to zero once that line is logged.
    pub(crate) anim_wakes: u32,

    /// Terminal draws this second.
    ///
    /// Counted after each draw the run loop puts on screen, whatever
    /// lane requested the frame; the number feeds the per-second
    /// frame-lane debug line and resets to zero when that line is
    /// logged.
    pub(crate) draws: u32,

    /// The slowest single draw's duration this second.
    ///
    /// `None` until the second's first draw completes, then the
    /// running maximum over every measured draw; the debug line
    /// reports it in whole milliseconds — zero while `None` — and
    /// the per-second reset returns it to `None`.
    pub(crate) worst_draw: Option<std::time::Duration>,

    /// When the current second started.
    ///
    /// `None` until the run's first draw, whose instant becomes the
    /// epoch; each later draw compares its own clock against that
    /// instant, and the rollover that logs a second's line moves the
    /// epoch to the rollover instant so the next window measures a
    /// full second.
    pub(crate) second_start: Option<std::time::Instant>,
}

impl FrameStats {
    /// Count one draw, and hand out the frame lane when its second ends.
    ///
    /// Every draw joins the count and the running worst. The window
    /// opens on the draw that finds it shut, and its epoch is that
    /// draw's own instant: the window always measures a full span of
    /// draws, so a second of quiet reports an honest zero rather than
    /// closing on the draw that happened to stand there. Once an epoch
    /// has stood [`FRAME_REPORT_SECONDS`] its counters go out as a
    /// [`FrameLane`] and a fresh window opens at that draw; until then
    /// nothing is reported.
    pub(crate) fn note_draw(
        &mut self,
        took: std::time::Duration,
        now: std::time::Instant,
    ) -> Option<FrameLane> {
        self.draws = self.draws.saturating_add(1);
        self.worst_draw = Some(match self.worst_draw {
            Some(worst) if worst >= took => worst,
            _ => took,
        });
        let Some(epoch) = self.second_start else {
            self.second_start = Some(now);
            return None;
        };
        if now.duration_since(epoch) < FRAME_REPORT_SECONDS {
            return None;
        }
        let lane = FrameLane {
            anim_wakes: self.anim_wakes,
            draws: self.draws,
            worst_draw_ms: u64::try_from(self.worst_draw.map_or(0, |d| d.as_millis()))
                .unwrap_or(u64::MAX),
        };
        *self = FrameStats {
            second_start: Some(now),
            ..FrameStats::default()
        };
        Some(lane)
    }
}

/// One closed second of frame-lane counters, ready for the log.
///
/// The numbers the per-second line carries, read off the window as it
/// closes: what animated, what was drawn, and the slowest single draw
/// in whole milliseconds.
pub(crate) struct FrameLane {
    /// Wakes the animation lane asked for during the window.
    ///
    /// Counted only on beats whose gate found something on screen
    /// animating, so the figure measures frames the lane actually
    /// requested rather than raw timer ticks; it reaches the log as
    /// the window closes and the reopened window starts again at zero.
    pub(crate) anim_wakes: u32,

    /// Draws the run loop put on screen during the window.
    ///
    /// Counted after every draw whatever lane requested it, so the
    /// figure reads as painted frames rather than requested ones —
    /// the two differ whenever a beat's work coalesced into a single
    /// frame.
    pub(crate) draws: u32,

    /// The window's slowest single draw, in whole milliseconds.
    ///
    /// The maximum over every measured draw, not the last one, so a
    /// laggy frame early in the second is still what the line reports;
    /// whole milliseconds is the resolution the lane is worth reading
    /// at, and a window that measured no draw reports zero.
    pub(crate) worst_draw_ms: u64,
}

/// The frame lane's pacing state: the redraw cap, the repaint debts,
/// and the layout's recorded geometry.
///
/// Everything the run loop's timing decisions read — the parked
/// redraw a tick claims, the frame-interval stamp, the full-repaint
/// debt a focus regain arms, and the layout line and viewport counts
/// the scroll's compensation arithmetic needs between frames.
#[derive(Default)]
pub(crate) struct FramePacing {
    /// Whether a capped background redraw is still owed.
    ///
    /// Set when a notify arrives inside the frame interval; the
    /// periodic tick claims it so a stream's tail renders even if no
    /// further delta arrives.
    pub(crate) render_pending: bool,

    /// When the last background-initiated frame rendered.
    ///
    /// Gates redraws to one per [`FRAME_INTERVAL`]; `None` until the
    /// first background frame.
    pub(crate) last_frame: Option<Instant>,

    /// Total lines the previous layout produced.
    ///
    /// While detached, each layout compensates the scroll offset for
    /// growth or shrinkage since this value, holding the viewport
    /// steady as the streaming region extends below it or collapses.
    pub(crate) last_layout_lines: usize,

    /// The viewport height the last layout measured.
    ///
    /// The jump's arithmetic needs the top row's absolute index —
    /// total minus viewport minus offset — outside the render pass,
    /// so the same settle that records the line count records the
    /// pane height it laid out against.
    pub(crate) last_layout_viewport: usize,

    /// The frame lane's own bookkeeping, for the log.
    ///
    /// One line per drawn second under `RUST_LOG=dch_tui::frames=debug`
    /// — a lane that never draws logs nothing — naming:
    /// how many animation wakes fired, how many terminal draws ran,
    /// and the slowest draw's duration — the three numbers that
    /// separate a stalled wake lane (wakes low), a blocked loop
    /// (wakes high, draws low), and a slow terminal (draws high with
    /// a large worst draw) at a glance.
    pub(crate) frame_stats: FrameStats,

    /// The pulse painter's clock, pinned by the glow pins.
    ///
    /// Exists only in the test build: `None` leaves the painter on
    /// the real clock, while a pin sets both the run's anchor and
    /// this clock so the asserted phase is the phase the math
    /// derives, whatever the runner's scheduling.
    #[cfg(test)]
    pub(crate) pulse_clock: std::cell::Cell<Option<Instant>>,

    /// The instant the terminal regained focus and a whole-screen
    /// repaint is still owed, if one is.
    ///
    /// Set when the terminal regains focus: some terminals skip
    /// painting while their window is hidden or occluded, so frames
    /// drawn during that time leave stale cells that the incremental
    /// diff cannot know about. The run loop claims it, clears the
    /// screen, and lets the next draw repaint every cell — the same
    /// full repaint a resize forces.
    pub(crate) full_repaint_due: Option<Instant>,
}

impl FramePacing {
    /// Spend the parked redraw request, stamping the frame time.
    ///
    /// `false`, and nothing moved, while no request is parked;
    /// otherwise the request is spent, `last_frame` takes `now` so the
    /// interval restarts from the frame it pays for, and the answer is
    /// `true` without a look at the interval — the tick that asks is
    /// owed the frame whenever it lands. The reasoning behind that
    /// reading is on the public claim, `TuiApp::take_pending_redraw`.
    pub(super) fn take_pending_redraw(&mut self, now: Instant) -> bool {
        if !self.render_pending {
            return false;
        }
        self.render_pending = false;
        self.last_frame = Some(now);
        true
    }

    /// The instant the strip animates against: a pin's clock when one
    /// stands, the real clock otherwise.
    ///
    /// The glow's phase is a pure function of the time since the
    /// run's anchor, so a pin that fixes both ends asserts an exact
    /// brightness instead of racing the milliseconds between setting
    /// the anchor and drawing the frame.
    pub(super) fn pulse_now(&self) -> Instant {
        #[cfg(test)]
        if let Some(pinned) = self.pulse_clock.get() {
            return pinned;
        }
        #[cfg(not(test))]
        let _ = self;
        Instant::now()
    }
}

/// The periodic beat the run loop wakes on.
///
/// The loop's coarse chores ride this beat: claiming a redraw a
/// capped background request parked, stepping a drag resting on the
/// conversation pane's edge, flipping the composer caret's blink
/// phase, and retiring lapsed notices and prompts. A tick with tool
/// calls in flight also draws, keeping their elapsed stamps' tenth
/// of a second live; every other idle tick draws nothing, and the
/// loop sleeps on.
pub(crate) const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The frame cadence the activity strip animates at while the agent
/// runs.
///
/// The loop's own tick is far too coarse for a moving bar, and deltas
/// alone leave it frozen between provider chunks; for as long as the
/// run lives the animation wake redraws at this step — fifty frames a
/// second, so even the fastest styles jump at most a couple of cells
/// per frame — whatever the wire's granularity.
pub(crate) const ANIMATION_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::message::TuiMessage;
    use crate::observer::{CompactionEvent, CompactionFiling};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use loopctl::middleware::PermissionResolution;
    use std::sync::Arc;

    #[test]
    fn a_public_redraw_claim_is_owed_its_frame_and_restarts_the_interval() {
        // The public claim carries the pacing state's whole contract:
        // nothing parked draws nothing and stamps nothing, a parked
        // request is spent on the first claim whatever the interval
        // says — the frame is owed, not re-checked — and the claim
        // stamps the frame time the interval restarts from.
        let mut app = app();
        let now = std::time::Instant::now();
        assert!(
            !app.take_pending_redraw(now),
            "nothing parked: nothing owed"
        );
        assert_eq!(
            app.pacing.last_frame, None,
            "an unclaimed tick stamps no frame time"
        );
        app.pacing.render_pending = true;
        // Inside the interval: a frame was stamped a millisecond ago.
        app.pacing.last_frame = Some(
            now.checked_sub(std::time::Duration::from_millis(1))
                .expect("a millisecond fits before now"),
        );
        assert!(
            app.take_pending_redraw(now),
            "a parked request is owed its frame inside the interval"
        );
        assert_eq!(
            app.pacing.last_frame,
            Some(now),
            "the claim stamps the frame time"
        );
        assert!(
            !app.take_pending_redraw(now),
            "one request pays for one frame"
        );
    }

    #[test]
    fn drain_shared_state_moves_retry_lines_into_the_conversation() {
        let mut app = app();
        let queue = std::sync::Arc::clone(&app.state.retry_lines);
        queue
            .lock()
            .expect("the retry lock")
            .push(crate::observer::RetryLine {
                episode: 0,
                text: "⟳ connection lost — reconnection attempt 1/3".to_string(),
            });
        app.drain_shared_state();
        queue
            .lock()
            .expect("the retry lock")
            .push(crate::observer::RetryLine {
                episode: 0,
                text: "⟳ connection lost — reconnection attempt 2/3".to_string(),
            });
        app.drain_shared_state();
        let retry_rows: Vec<&TuiMessage> = app
            .conversation()
            .iter()
            .filter(|message| matches!(message, TuiMessage::System { .. }))
            .collect();
        assert_eq!(
            retry_rows.len(),
            1,
            "one episode, one row — the second attempt rewrote it: {retry_rows:?}"
        );
        match retry_rows.first().expect("the retry row") {
            TuiMessage::System { text, .. } => {
                assert!(
                    text.contains("reconnection attempt 2/3"),
                    "the row holds the newest attempt: {text}"
                );
            }
            other => panic!("the retry row is a system row: {other:?}"),
        }
        assert!(
            queue.lock().expect("the retry lock").is_empty(),
            "the queue drains"
        );
    }

    #[test]
    fn two_manual_passes_in_one_drain_each_take_their_own_words() {
        // Two manual passes land between drains: both rows are owed at
        // once, each filing sits stamped behind its own pass, and one
        // drain settles both — each row with its own pass's words, the
        // earlier row's debt surviving alongside the later's instead
        // of being silently dropped.
        let mut app = app();
        let first = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- the first pass's ledger",
        )]);
        let second = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- the second pass's ledger",
        )]);
        {
            let mut filings = app
                .state
                .compaction_publications
                .lock()
                .expect("the publications lock");
            filings.push(CompactionFiling {
                pass: 1,
                folded: first,
            });
            filings.push(CompactionFiling {
                pass: 2,
                folded: second,
            });
        }
        {
            let mut events = app.state.compactions.lock().expect("the compaction lock");
            events.push(CompactionEvent {
                pass: 1,
                record: compacted_row("manual"),
            });
            events.push(CompactionEvent {
                pass: 2,
                record: compacted_row("manual"),
            });
        }
        app.drain_shared_state();
        let rows = app.conversation();
        match rows.first() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary
                    .as_deref()
                    .is_some_and(|text| text.contains("the first pass's ledger")),
                "the earlier row keeps its own debt and its own words: {summary:?}"
            ),
            other => panic!("the first compacted row landed: {other:?}"),
        }
        match rows.last() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary
                    .as_deref()
                    .is_some_and(|text| text.contains("the second pass's ledger")),
                "the later row takes its own pass's words: {summary:?}"
            ),
            other => panic!("the second compacted row landed: {other:?}"),
        }
        assert!(
            app.state
                .compaction_publications
                .lock()
                .expect("the publications lock")
                .is_empty(),
            "each filing served exactly the row carrying its stamp"
        );
    }

    #[test]
    fn ctrl_c_cancels_the_ask_instead_of_quitting() {
        // The hint's promise holds: under a pending ask, ctrl+c
        // denies that ask — cancel, not quit — and the reply that
        // crosses is a refusal.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        app.permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
                input: serde_json::json!({ "command": "cargo test" }),
                always: Vec::new(),
                never: Vec::new(),
                reply: reply_tx,
            });
        // An approval answers a rendered ask: the pin draws the sheet
        // once so the press finds it answerable.
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(true),
            "ctrl+c answers the ask"
        );
        assert_eq!(
            reply_rx.try_recv(),
            Ok(PermissionResolution::DenyOnce),
            "ctrl+c denies — the ask is cancelled, not the session"
        );
    }

    #[test]
    fn a_sheet_s_esc_does_not_arm_the_cancel_ladder() {
        // The restored priority: a sheet (a permission ask) reads its
        // own Esc — the ask denies — without stepping the cancel
        // ladder. Only a bare composer lets the ladder engage, so a
        // user denying rapid asks cannot accidentally arm a cancel,
        // and the run's cancel needs its own deliberate pair.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.state
            .agent_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let cancels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = Arc::clone(&cancels);
        app.set_run_canceller(Box::new(move || {
            sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        app.permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "run it?".to_string(),
                input: serde_json::Value::Null,
                always: Vec::new(),
                never: Vec::new(),
                reply: reply_tx,
            });
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        // An approval answers a rendered ask: the pin draws the sheet
        // once so the press finds it answerable.
        let _ = app.permission.shown_permission();

        // The ask's Esc: denies it, does NOT arm the cancel.
        assert!(app.handle_key(esc), "the sheet's Esc is taken");
        assert_eq!(
            reply_rx.blocking_recv(),
            Ok(PermissionResolution::DenyOnce),
            "the standing ask answers the Esc with its deny — a reply, never a \
             dropped request"
        );
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the sheet's press never arms the cancel"
        );

        // With the ask gone and the composer bare, the ladder works:
        // arm, then cancel.
        assert!(app.handle_key(esc), "the bare-composer Esc is taken");
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "one bare press arms, it does not cancel"
        );
        assert!(app.handle_key(esc), "the second bare Esc is taken");
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second bare Esc cancels the run"
        );
    }
}
