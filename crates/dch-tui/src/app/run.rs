//! The TUI event loop: terminal input, the render notify, and the
//! periodic ticks selected against each other, a frame drawn only
//! when something changed and a full repaint claimed when the
//! screen and the buffer can no longer be assumed to agree.

use std::sync::Arc;
use std::time::Instant;

use ratatui::backend::CrosstermBackend;

use crate::events::TerminalEvents;

use super::TuiApp;
use super::{ANIMATION_INTERVAL, PendingQuestion, QuitState, TICK_INTERVAL};

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
        self.full_repaint_due = Some(Instant::now());
        let mut events = TerminalEvents::spawn();
        self.input_quiesce = Some(events.quiesce_handle());
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        let mut animation = tokio::time::interval(ANIMATION_INTERVAL);
        animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let notify = Arc::clone(&self.state.render_notify);
        let mut listener = notify.listen();
        let mut permission_rx = self.permission_rx.take();
        let mut permission_closed = false;
        let mut question_rx = self.question_rx.take();
        let mut question_closed = false;
        let mut session_rx = self.session_rx.take();
        let mut session_closed = false;

        terminal.draw(|frame| self.render(frame))?;

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
                            Some(rx) => self.land_question_requests(prompt, rx),
                            None => {
                                self.pending_questions
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
                        self.frame_stats.anim_wakes =
                            self.frame_stats.anim_wakes.saturating_add(1);
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
                            Some(rx) => self.land_permission_requests(request, rx),
                            None => self.pending_permissions.push_back(request),
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
            if self.retire_withdrawn_questions() {
                needs_redraw = true;
            }
            if self.take_full_repaint() {
                if let Err(err) = terminal.clear() {
                    tracing::warn!("full repaint clear failed: {err}");
                }
                needs_redraw = true;
            }
            if needs_redraw {
                let theme_burst = self.pending_theme_osc.take();
                if theme_burst.is_some() {
                    crate::terminal::begin_synchronized_output();
                }
                let started = Instant::now();
                terminal.draw(|frame| self.render(frame))?;
                let took = started.elapsed();
                if let Some((background, primary)) = theme_burst {
                    crate::terminal::finish_theme_burst(background, primary);
                }
                if let Some(lane) = self.frame_stats.note_draw(took, Instant::now()) {
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
}
