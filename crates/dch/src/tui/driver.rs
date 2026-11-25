//! The submission driver: the loop that owns the runner and
//! executes queued prompts strictly one at a time, with the
//! context, overrides, and shared state every run is built
//! from.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dch_tui::HealthNotice;
use dch_tui::SessionCommand;
use dch_tui::SessionUpdate;
use dch_tui::Submission;
use dch_tui::TuiObserverState;
use tokio::sync::mpsc;

use super::commands::apply_session_command;
use super::commands::post_session_notice;
use super::corrections::{
    apply_correction_cut, exit_owns_cancel, pull_corrected_wording, pull_pending_correction,
    rescue_correction_text, rescue_stranded_corrections,
};
use super::permissions_bridge::InteractiveLanes;
use super::session::filtered_machine_history;
use super::transcript::{MachineSnapshot, TranscriptHandle};

/// Claim a prompt for the turn that asked for it, unless the session
/// is leaving.
///
/// The channel carries an id rather than the text so this read is the last
/// word on what gets sent: a prompt edited between its submit and its turn
/// runs as edited, and a prompt the user dropped in between yields `None`
/// and never runs at all. A prompt that yields nothing is not a failed run
/// and is not treated as one — there is simply no turn to start, and the
/// driver takes the next id the channel holds.
///
/// The shutdown flag is read under the queue's own lock, beside the
/// removal it gates, because the exit's unclaimed-rows snapshot reads
/// the same lock after raising the flag: the mutex orders the two, so
/// a prompt is either claimed here or still waiting when the snapshot
/// reads it — never both, which is what keeps the exit's transcript
/// echo from duplicating a claim that raced it. A claim that finds the
/// flag up declines like any other `None`: the entry is not spent,
/// and the exit's snapshot is what records it.
fn take_queued(state: &TuiObserverState, id: u64, shutting_down: &AtomicBool) -> Option<String> {
    let mut queue = state
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if shutting_down.load(Ordering::SeqCst) {
        return None;
    }
    queue.remove(id)
}

/// Put a prompt back where the waiting section shows it.
///
/// The other half of `take_queued`: the queue is shared with the display, so
/// a prompt the driver can no longer send — a submission the shutdown
/// overtook — returns there to be read, edited, or dropped rather than
/// thrown away. It will not run in a session that has stopped taking
/// submissions; neither do its words vanish with one.
fn push_queued(state: &TuiObserverState, text: String) -> u64 {
    state
        .queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(text)
}

/// Post a run's failure as the transcript's error row.
///
/// The row is the session's record that the turn died and why: it
/// lands in the shared buffer the display drains on its next frame,
/// and the wake beside it makes the failure read as part of the run
/// that produced it rather than arriving a frame late.
fn post_run_error(state: &TuiObserverState, err: &loopctl::error::LoopError) {
    state
        .errors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(err.to_string());
    state.render_notify.notify(1);
}

/// The notice a rate-limit escalation posts beside its error row.
///
/// `None` for every failure but the one whose next step is the
/// user's. An escalation means the retry ladder is spent, so the
/// advice says how long to wait: the provider's own window when its
/// last hint named one, a minute or two when it named none — an
/// immediate retry re-runs the same dead ladder. The fallback
/// sentence rides only when a fallback model is actually armed, and
/// states what the breaker really does: it decides where the turns
/// that follow run. The error row keeps the provider's facts
/// verbatim; the notice carries the part no payload spells: what to
/// do next.
fn rate_limit_escalation_notice(
    err: &loopctl::error::LoopError,
    fallback_armed: bool,
) -> Option<String> {
    let loopctl::error::LoopError::RateLimitEscalation {
        attempts,
        retry_after,
    } = err
    else {
        return None;
    };
    let facts = match retry_after {
        Some(window) => format!("the provider's last hint said {}s", window.as_secs()),
        None => "the provider sent no Retry-After".to_string(),
    };
    let advice = match retry_after {
        Some(window) => format!(
            "Resume after the provider's {}-second window closes: an earlier retry \
             re-runs the same ladder.",
            window.as_secs()
        ),
        None => "Wait a minute or two before resuming: an immediate retry re-runs the \
             same ladder."
            .to_string(),
    };
    let fallback = if fallback_armed {
        " The failure rides the model's breaker; with a fallback model configured, \
         the breaker decides where the turns that follow run."
    } else {
        ""
    };
    Some(format!(
        "rate-limited after {attempts} retries — {facts}. {advice}{fallback}"
    ))
}

/// Land a failed turn where the session reads it.
///
/// The verbatim error row the transcript keeps, plus — for the one
/// failure whose remedy lives outside the session — the notice that
/// names it, on the lane the transient notice row reads. The row and
/// the notice land together or the row lands alone, and nothing else
/// changes.
fn land_run_error(
    state: &TuiObserverState,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    err: &loopctl::error::LoopError,
    fallback_armed: bool,
) {
    post_run_error(state, err);
    if let Some(text) = rate_limit_escalation_notice(err, fallback_armed) {
        post_session_notice(session_tx, text);
    }
}

/// The run-scoped CLI statements a mid-session path must keep honouring.
///
/// `--model` and `--reasoning-effort` are per-run words: the boot
/// resolution gives them their documented precedence over the
/// configuration's own selection steps, and a mid-session rebuild — a
/// `/model` switch, a provider switch, a live resume — runs those same
/// steps again. Carried beside the configs so those paths restore the
/// flags exactly as boot's order did, instead of letting a `[[models]]`
/// entry's own dial or a saved session's model quietly outrank them.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunOverrides {
    /// The `--model` flag's name, when the flag was given.
    ///
    /// A live resume adopts no saved model over it — the same bow a
    /// startup resume's adoption makes.
    pub(crate) model: Option<String>,

    /// The `--reasoning-effort` flag's value, when the flag was given.
    ///
    /// Restored over whatever dial a mid-session selection lays on, on
    /// every path that selects.
    pub(crate) reasoning_effort: Option<String>,

    /// Whether the `--permission-mode` flag named the run's mode.
    ///
    /// A live resume adopts no saved mode over it — the same bow the
    /// startup adoption makes — and the flag's value stays the
    /// session's standing override, recorded on the save target.
    pub(crate) permission_named: bool,

    /// The permission mode the configuration file itself states.
    ///
    /// A launch `--permission-mode` is the opened session's own
    /// override, not a property of every fresh session after it: a
    /// `/clear` returns to this value — exactly what `[runner]
    /// permission_mode` names in the file — never to the flag-tainted
    /// base the session carries.
    pub(crate) permission_default: dch_config::PermissionMode,
}

/// The pieces a driver command needs beyond the runner it holds.
///
/// A [`dch_tui::SessionCommand`] that rebuilds the
/// runner — a cross-provider model switch, a live resume — needs the
/// config to build on, the sessions root a resume loads through, the
/// workdir the tools dispatch through, the shared observer state a
/// fresh observer reads, and the shutdown flag the assembler's nudge
/// gate consults. The config is live truth: every switch the loop
/// accepts updates it here, so a later rebuild builds on what the
/// loop actually runs rather than what the session launched with.
/// Bundled here so the driver loop and its command application stay
/// under the argument count the lints enforce.
#[derive(Clone)]
pub(crate) struct DriverContext {
    /// The session's live configuration, kept current by accepted switches.
    ///
    /// The host builds the first copy from its own load; the driver
    /// updates it whenever the loop accepts a model or provider
    /// switch, so a rebuild — a resume, a later switch — runs on the
    /// settings in effect at that moment, not the launch-time ones.
    pub(crate) config: dch_config::DchConfig,

    /// The user-stated config every selection layers onto.
    ///
    /// The boot-time config before any model selection ran — the file
    /// plus the CLI's non-model overrides — carried beside the live
    /// copy because `select_model` layers rather than replaces: a
    /// switch selecting onto the live config would stack the new
    /// entry over the previous selection's unstated leftovers (the
    /// boot default's context window surviving a switch to an entry
    /// that states none). Every switch clones this base and selects
    /// onto it, so the second switch of a session equals a restart
    /// naming it. Shared behind an [`Arc`] because the config is large
    /// and never mutated after construction — a driver future carrying
    /// the context stays small. Panel-side `/set` overrides never reach
    /// it — exactly as they never reached the driver's live copy
    /// mid-session; the file is their road to a rebuild.
    pub(crate) config_base: Arc<dch_config::DchConfig>,

    /// The directory the session's tools dispatch through.
    ///
    /// The workdir every tool's paths resolve against — the same root
    /// the runner was built on, carried so a rebuilt runner inherits
    /// it unchanged.
    pub(crate) workdir: std::path::PathBuf,

    /// The root under which saved sessions live.
    ///
    /// A live resume loads through this root — the same one the
    /// session saver writes under. Carried here rather than derived
    /// at load time so a test points it at a throwaway directory.
    pub(crate) sessions_root: std::path::PathBuf,

    /// The shared observer state the display drains.
    ///
    /// The atomics and queues the driver writes and the panel reads —
    /// tokens, run flags, the todo and tool lanes — one handle shared
    /// by both halves of the session.
    pub(crate) state: TuiObserverState,

    /// The shutdown flag the nudge gate consults.
    ///
    /// Raised by the exit path before its cancel; the todo nudge
    /// refuses to speak for a session on its way out, so a quitting
    /// user's last turn carries no reminder text.
    pub(crate) shutting_down: Arc<AtomicBool>,

    /// The session's interactive lanes: permission gate and ask tool.
    ///
    /// Built once at startup from the very channels the panel holds,
    /// so every runner rebuild re-attaches the same pair — see
    /// [`InteractiveLanes`] for why a rebuild may never build its
    /// own.
    pub(crate) lanes: InteractiveLanes,

    /// The run-scoped CLI statements a rebuild must keep honouring.
    ///
    /// `--model` and `--reasoning-effort` as the flags gave them
    /// (`None` when absent): a switch restores the effort dial over
    /// whatever the selected entry states, and a live resume adopts no
    /// saved model over the flag's own — the precedences the boot
    /// resolution already gave them, kept for the whole session rather
    /// than the stretch before the first rebuild.
    pub(crate) overrides: RunOverrides,
}

/// Fire the session's naming call when a claim owes one.
///
/// A session is named once: the ask that opens the conversation is
/// its subject, and the caller hands that subject over — the claimed
/// prompt for a fresh session, the restored history's first user
/// prompt for a resumed one whose naming never answered (see
/// [`crate::messages::naming_subject`]). A prompt still waiting —
/// queued behind a run, or dropped — has not opened anything. A
/// session that already carries a title (a resume, or an earlier
/// answer that landed) is never renamed; a call already in flight for
/// the same session stands that session's later claims down until it
/// settles, while a session the host swapped in meanwhile owes — and
/// fires — its own. The call is fire-and-forget and bounded: a run
/// never waits on its session's name, the answer lands on the saver
/// cell only if the cell still holds the session the call was fired
/// for, and a call that fails or outlives its bound leaves the
/// session as it was — unnamed, its slot settled, and named on a
/// later claim.
pub(crate) fn name_session_if_owed(
    transcript: &TranscriptHandle,
    api: &dch_config::ApiConfig,
    naming_prompt: &str,
) {
    let id = transcript.current_session_id();
    if transcript.has_title() || !transcript.arm_naming(id) {
        return;
    }
    let transcript = transcript.clone();
    let api = api.clone();
    let prompt = naming_prompt.to_string();
    tokio::spawn(async move {
        let title = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            dch_loop::session_title(&api, &prompt),
        )
        .await
        .unwrap_or_default();
        if let Some(title) = title {
            transcript.set_title_for(id, title);
        }
        transcript.settle_naming(id);
    });
}

/// The claim-edge publisher that carries retired plan entries to
/// the display.
///
/// The runner drops never-started entries at each claim past the
/// first, and the drop is invisible to the transcript until the next
/// `TodoWrite` lands — so the runner's retirement sink hands the batch
/// here the moment the claim makes it, while the run it precedes is
/// still in flight, and the shared state's own publication records the
/// plan generation and wakes the frame beside it: the plan section
/// drops the retired rows on the next render, not a blink late.
pub(crate) fn retirement_publisher(
    state: &TuiObserverState,
) -> impl Fn(&[dch_tools::TodoEntry]) + Send + Sync + 'static {
    let published = state.clone();
    move |entries: &[dch_tools::TodoEntry]| {
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|entry| (entry.id.clone(), entry.subject.clone()))
            .collect();
        published.publish_claim_retirements(&pairs);
    }
}

/// Apply one queued command and retire the boundary it ended.
///
/// A command that replaced the conversation — a landed rotation, an
/// adopted resume — takes the recorded claim with it: the runner the
/// boundary named is gone, and a correction naming that runner must
/// not rewind its replacement with the old prefix. A command that
/// left the conversation unchanged keeps both states, so its own
/// correction semantics still apply.
async fn apply_command_and_retire_boundary(
    runner: &mut dch_loop::Runner,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    command: SessionCommand,
    context: &mut DriverContext,
    last_claim: &mut Option<crate::tui::corrections::ClaimBoundary>,
    interrupted_for: &mut Option<u64>,
) {
    let replaced = apply_session_command(runner, transcript, session_tx, command, context).await;
    if replaced {
        *last_claim = None;
        *interrupted_for = None;
    }
}

/// Cut a correction's superseded turn when the claim still names it.
///
/// The claim taken here is the driver's whole correction state: a
/// claim for another turn is left untouched, so an older correction
/// runs as an ordinary turn, and the claimed prefix rides beside the
/// length for the boundary check the cut performs.
fn cut_superseded_turn(
    runner: &mut dch_loop::Runner,
    state: &TuiObserverState,
    last_claim: &mut Option<crate::tui::corrections::ClaimBoundary>,
    id: u64,
    interrupted: bool,
) {
    let superseded = if last_claim
        .as_ref()
        .is_some_and(|(claimed, _, _)| *claimed == id)
    {
        last_claim.take()
    } else {
        None
    };
    if let Some((before, claimed)) =
        superseded.map(|(_, before, claimed)| (before, std::sync::Arc::clone(&claimed)))
    {
        apply_correction_cut(runner, state, before, interrupted, &claimed);
    }
}

/// Execute submitted tasks one at a time until the session ends.
///
/// Owns the runner for the session's whole lifetime, so runs never
/// overlap: each submitted line runs to completion or failure before
/// the next begins, preserving the order the user submitted in. A
/// failed run records its error into the shared buffer the display
/// drains and wakes the frame — the session continues, and the next
/// submission still runs. A cancelled run changes nothing beyond the
/// turn it stopped: the submissions queued behind it stand, and the
/// next is claimed and sent as soon as the cancelled one settles —
/// one cancel never takes the backlog with it.
/// A correction asks for the turn that already went out to be un-asked:
/// [`Submission::Correct`] cuts the superseded turn out of the committed
/// history — back to the length that turn started from, which this loop
/// records at each claim — and then sends the edited text, so the model
/// reads one prompt where the user asked a question twice. Only the most
/// recently claimed turn is cuttable: a later answer may rest on the turn
/// a correction names, and cutting it would take that answer with it, so
/// an older correction runs as an ordinary turn instead.
/// A correction stops the turn it names rather than queueing behind
/// it: the display raises the runner's cancel at the moment the edit
/// is saved, so the superseded turn resolves cancelled and the cut
/// lands on whatever it had committed by then — the cancel is the
/// correction's own hand, so it raises no error row of its own.
///
/// The loop ends when the channel closes or
/// the shutdown flag is set: the engine clears its cancel signal at
/// the end of every run, so the flag — raised by the display at the
/// very moment the quit ladder fires, before its cancel lands on
/// the run in flight, and stored once more after the display loop
/// as the backstop for every other route out — is what keeps
/// submissions queued behind a quitting user from ever
/// starting. The flag gates the loop condition, so it is checked
/// before a queued submission is admitted, not only after a run
/// returns — and once more when a parked receive wakes, so a
/// submission the flag overtakes mid-park is dropped unclaimed
/// rather than started — and the claim itself reads the flag under
/// the queue's lock, so a submission already past those checks still
/// leaves its entry waiting for the exit's snapshot rather than
/// spending it on a run the exit is stopping. Corrections still
/// buffered when the exit wins — sitting in the channel, or pulled
/// a moment before the flag rose — are collected by the exit drain
/// into the transcript echo's shared buffer: their words left the
/// queue with the claim, so an unclaimed row is the only durable
/// hand-back they take.
pub(crate) async fn run_driver_loop(
    mut runner: dch_loop::Runner,
    mut receiver: mpsc::UnboundedReceiver<Submission>,
    mut context: DriverContext,
    transcript: TranscriptHandle,
    session_tx: mpsc::UnboundedSender<SessionUpdate>,
) {
    let mut last_claim: Option<crate::tui::corrections::ClaimBoundary> = None;
    let mut revived: std::collections::VecDeque<Submission> = std::collections::VecDeque::new();
    let mut interrupted_for: Option<u64> = None;
    context.state.record_todos_undone(runner.todo_undone());
    loop {
        if context.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        let submission = match revived.pop_front() {
            Some(submission) => submission,
            None => {
                if let Ok(submission) = receiver.try_recv() {
                    submission
                } else {
                    context.state.mark_run_ended();
                    match receiver.recv().await {
                        Some(submission) => submission,
                        None => break,
                    }
                }
            }
        };
        if context.shutting_down.load(Ordering::SeqCst) {
            revived.push_front(submission);
            break;
        }
        let text = match submission {
            Submission::Run { id } => {
                let Some(text) = take_queued(&context.state, id, &context.shutting_down) else {
                    continue;
                };
                let claimed = std::sync::Arc::new(runner.committed_history());
                last_claim = Some((id, claimed.len(), claimed));
                context.state.live_claim.store(id, Ordering::SeqCst);
                interrupted_for = None;
                text
            }
            Submission::Correct { id, text } => {
                let interrupted = interrupted_for
                    .take()
                    .is_some_and(|cancelled| cancelled == id);
                cut_superseded_turn(
                    &mut runner,
                    &context.state,
                    &mut last_claim,
                    id,
                    interrupted,
                );
                text
            }
            Submission::Command { command } => {
                Box::pin(apply_command_and_retire_boundary(
                    &mut runner,
                    &transcript,
                    &session_tx,
                    command,
                    &mut context,
                    &mut last_claim,
                    &mut interrupted_for,
                ))
                .await;
                continue;
            }
        };
        let naming_prompt = crate::messages::naming_subject(&runner.committed_history(), &text);
        name_session_if_owed(&transcript, &context.config.api, &naming_prompt);
        context.state.mark_run_started();
        context.state.push_sent(text.clone());
        context
            .state
            .cancel_signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reset();
        context
            .state
            .correction_cancel
            .store(u64::MAX, Ordering::SeqCst);
        let result = runner.run(&text).await;
        context.state.live_claim.store(u64::MAX, Ordering::SeqCst);
        let resolved = Arc::new(runner.committed_history());
        transcript.send_machine(MachineSnapshot {
            messages: filtered_machine_history(&resolved),
            saved_at: chrono::Utc::now(),
            compactions: 0,
            saver: None,
        });
        *context
            .state
            .machine_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(resolved);
        context.state.record_todos_undone(runner.todo_undone());
        if let Err(err) = &result {
            context
                .state
                .active_tools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            if exit_owns_cancel(err, &context.shutting_down) {
                post_run_error(&context.state, err);
                if let Some(corrected) =
                    pull_corrected_wording(last_claim.clone(), &mut receiver, &mut revived)
                {
                    rescue_correction_text(&context.state, corrected);
                } else {
                    push_queued(&context.state, text.clone());
                }
                context.state.mark_run_ended();
                break;
            }
            let interrupted_id = if matches!(err, loopctl::error::LoopError::Cancelled) {
                context
                    .state
                    .correction_cancel
                    .swap(u64::MAX, Ordering::SeqCst)
            } else {
                u64::MAX
            };
            let names_this_turn = last_claim
                .as_ref()
                .is_none_or(|(claimed, _, _)| *claimed == interrupted_id);
            let correction = if interrupted_id != u64::MAX
                && names_this_turn
                && pull_pending_correction(interrupted_id, &mut receiver, &mut revived)
            {
                Some(interrupted_id)
            } else {
                None
            };
            if let Some(cancelled_for) = correction {
                interrupted_for = Some(cancelled_for);
            } else {
                land_run_error(
                    &context.state,
                    &session_tx,
                    err,
                    context.config.api.fallback_model.is_some(),
                );
            }
        }
    }
    if context.shutting_down.load(Ordering::SeqCst) {
        rescue_stranded_corrections(&mut revived, &mut receiver, &context.state);
    }
    context.state.mark_run_ended();
    context.state.record_todos_undone(runner.todo_undone());
}

/// Yield the conversation's running row once a compaction settles.
///
/// The engine's observer contract has no declined event, so the row
/// the pass's opening event raised has no closer on the decline path —
/// this is the closer. An early refusal (a live turn) never raised
/// the row, and an engine failure clears it through the failed
/// event; setting the lane idle is right either way.
pub(crate) fn yield_the_running_row(context: &DriverContext) {
    *context
        .state
        .health
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = HealthNotice::Idle;
}

/// Test-facing driver entry: the loop over a session channel nobody reads.
///
/// The production driver reports between-turns model and session changes on a
/// channel the panel reads; a driver test cares about runs and corrections,
/// not those reports, so it drives through here with a sender whose receiver
/// is dropped immediately. No existing test sends a [`Submission::Command`],
/// so the closed sender is never written to; a test that does exercise a
/// command calls [`run_driver_loop`] directly and keeps the receiver to
/// assert on the published update.
#[cfg(test)]
pub(crate) async fn drive_submissions(
    runner: dch_loop::Runner,
    receiver: mpsc::UnboundedReceiver<Submission>,
    state: TuiObserverState,
    shutting_down: Arc<AtomicBool>,
    transcript: TranscriptHandle,
) {
    let (session_tx, _session_rx) = mpsc::unbounded_channel();
    state.adopt_runner_handles(runner.cancel_signal(), None);
    Box::pin(run_driver_loop(
        runner,
        receiver,
        DriverContext {
            config: dch_config::DchConfig::default(),
            config_base: std::sync::Arc::new(dch_config::DchConfig::default()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: crate::session::sessions_dir(),
            state,
            shutting_down,
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        },
        transcript,
        session_tx,
    ))
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::corrections::pull_pending_correction;
    use crate::tui::permissions_bridge::InteractiveLanes;
    use crate::tui::session::session_driver_context;
    use crate::tui::session::tear_down_driver_bounded;
    use crate::tui::test_support::*;
    use crate::tui::transcript::TranscriptWorker;
    use dch_tui::SessionCommand;
    use dch_tui::SessionUpdate;
    use dch_tui::Submission;
    use dch_tui::TuiObserverState;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    #[test]
    fn an_escalation_notice_names_the_facts_and_the_next_step() {
        // The hintless escalation: the notice carries the payload's
        // facts — four retries, no Retry-After — and advice that
        // matches them: a generic minute-or-two, because the provider
        // named no window. With a fallback model armed, the routing
        // sentence rides along and holds whoever the breaker is
        // currently serving: it decides, it does not promise a
        // destination.
        let notice = rate_limit_escalation_notice(
            &loopctl::error::LoopError::RateLimitEscalation {
                attempts: 4,
                retry_after: None,
            },
            true,
        )
        .expect("an escalation earns its notice");
        assert!(
            notice.contains("4 retries")
                && notice.contains("no Retry-After")
                && notice.contains("Wait a minute or two")
                && notice.contains("breaker decides")
                && notice.contains("fallback model"),
            "the notice names the facts, the honest routing, and the next step: {notice}"
        );
        // Without a fallback model the routing promise is absent — a
        // session with nowhere to reroute is told nothing false.
        let bare = rate_limit_escalation_notice(
            &loopctl::error::LoopError::RateLimitEscalation {
                attempts: 4,
                retry_after: None,
            },
            false,
        )
        .expect("the notice itself does not depend on the fallback");
        assert!(
            !bare.contains("fallback"),
            "no fallback armed, no routing claimed: {bare}"
        );
        // A hinted escalation takes its advice from the hint: the wait
        // is the provider's own window, in seconds, never the generic
        // minute-or-two.
        let hinted = rate_limit_escalation_notice(
            &loopctl::error::LoopError::RateLimitEscalation {
                attempts: 3,
                retry_after: Some(std::time::Duration::from_secs(3600)),
            },
            false,
        )
        .expect("a hinted escalation earns its notice too");
        assert!(
            hinted.contains("3 retries")
                && hinted.contains("3600s")
                && hinted.contains("3600-second window")
                && !hinted.contains("minute or two"),
            "the hint's own window is the advice: {hinted}"
        );
        // Every other failure stays wordless: only the escalation's
        // remedy lives outside the session.
        assert!(
            rate_limit_escalation_notice(&loopctl::error::LoopError::Cancelled, true).is_none(),
            "a cancel owes no advice"
        );
    }

    #[tokio::test]
    async fn an_escalating_failure_lands_the_error_row_and_the_notice_together() {
        // The wired arm's pairing, driven through the seam itself: an
        // escalation writes the verbatim error row the transcript
        // keeps and posts the actionable notice the session shows,
        // while a plain failure writes the row alone — no notice a
        // user could mistake for advice.
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        land_run_error(
            &state,
            &session_tx,
            &loopctl::error::LoopError::RateLimitEscalation {
                attempts: 4,
                retry_after: None,
            },
            true,
        );
        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(errors.len(), 1, "the verbatim error row lands");
            assert!(
                errors[0].contains("Rate-limit escalation"),
                "the row keeps the provider's facts verbatim: {:?}",
                errors[0]
            );
        }
        let SessionUpdate::Notice { text } = session_rx.try_recv().expect("the notice is posted")
        else {
            panic!("the escalation's update is a notice");
        };
        assert!(
            text.contains("Wait a minute or two"),
            "the posted notice is the actionable one: {text}"
        );
        assert!(
            session_rx.try_recv().is_err(),
            "one escalation posts one notice"
        );

        let (_, plain_state) = TuiObserverState::new().into_observer();
        let (plain_tx, mut plain_rx) = mpsc::unbounded_channel();
        land_run_error(
            &plain_state,
            &plain_tx,
            &loopctl::error::LoopError::Cancelled,
            false,
        );
        assert_eq!(
            plain_state.errors.lock().unwrap().len(),
            1,
            "a plain failure still records its row"
        );
        assert!(
            plain_rx.try_recv().is_err(),
            "a plain failure posts no notice"
        );
    }

    #[tokio::test]
    async fn an_escalated_run_posts_the_notice_because_the_config_arms_a_fallback() {
        // The loop derives the notice's routing sentence from
        // `api.fallback_model` itself, not from a caller's guess: a
        // throttled endpoint — 429s without `Retry-After`, answered on
        // every attempt — with a fallback configured must reach the
        // session channel as exactly one notice carrying the breaker
        // sentence.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback throttle");
        let port = listener
            .local_addr()
            .expect("the throttle has an address")
            .port();
        let _server = std::thread::spawn(move || {
            for accepted in listener.incoming() {
                let Ok(mut stream) = accepted else {
                    continue;
                };
                let mut probe = [0_u8; 512];
                let _read = std::io::Read::read(&mut stream, &mut probe);
                let body = "{\"error\":{\"message\":\"rate limit exceeded\"}}";
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _written = std::io::Write::write_all(&mut stream, response.as_bytes());
                let _flushed = std::io::Write::flush(&mut stream);
            }
        });
        let mut config = unreachable_config();
        config.api.base_url = format!("http://127.0.0.1:{port}");
        config.api.fallback_model = Some("test-fallback".to_string());
        config.runner.rate_limit_delay_secs = 1;
        let dir = tempfile::tempdir().expect("runner tempdir");
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("a runner against the loopback throttle");
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do a thing");
        drop(tx);
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        state.adopt_runner_handles(runner.cancel_signal(), None);
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            run_driver_loop(
                runner,
                rx,
                DriverContext {
                    config: config.clone(),
                    config_base: Arc::new(config),
                    workdir: dir.path().to_path_buf(),
                    sessions_root: crate::session::sessions_dir(),
                    state: state.clone(),
                    shutting_down: shutdown_flag(false),
                    lanes: InteractiveLanes::detached(),
                    overrides: RunOverrides::default(),
                },
                parked_transcript(),
                session_tx,
            ),
        )
        .await
        .expect("the throttled run resolves the loop");
        let SessionUpdate::Notice { text } = session_rx
            .try_recv()
            .expect("the wired loop posts the notice")
        else {
            panic!("the escalation's update is a notice");
        };
        assert!(
            text.contains("breaker decides"),
            "the routing sentence rides because the config armed the fallback: {text}"
        );
        assert!(
            session_rx.try_recv().is_err(),
            "one escalation posts one notice"
        );
    }

    #[tokio::test]
    async fn a_failing_run_routes_its_error_into_the_shared_buffer() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do a thing");
        drop(tx);
        let listener = state.render_notify.listen();

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            queued_prompts(&state).is_empty(),
            "the driver claims the submission, leaving nothing waiting"
        );
        assert_eq!(
            claimed(&state),
            ["do a thing"],
            "the claimed prompt is handed to the transcript once, as sent"
        );

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(errors.len(), 1, "exactly one failure is recorded");
            assert!(!errors[0].is_empty(), "the failure carries its message");
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener)
                .await
                .is_ok(),
            "the driver wakes the frame, so the error row renders without a keypress"
        );
    }

    #[tokio::test]
    async fn a_correction_behind_a_conversation_replacing_resume_cuts_nothing() {
        // The replaced conversation retires the boundary: a claim the
        // ended runner recorded — the prompt taken before a queued
        // `/resume` — must not rewind the resumed runner's history when
        // its correction lands behind the command. The resumed turn
        // stands; only a claim the new conversation mints may cut it.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let workdir = dir.path().to_path_buf();
        let root = dir.keep();
        let b = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            b,
            "test-model".to_string(),
            root.clone(),
        ));
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the resumed turn".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .expect("the resumed session lands");
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let claim = queue_prompt(&state, &tx, "the ended runner's prompt");
        tx.send(Submission::Command {
            command: SessionCommand::Resume { id: b.to_string() },
        })
        .expect("driver channel open");
        correct_prompt(&tx, claim, "the correction rides behind the resume");
        drop(tx);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_driver_loop(
                runner,
                rx,
                DriverContext {
                    config: unreachable_config(),
                    config_base: Arc::new(unreachable_config()),
                    workdir,
                    sessions_root: root,
                    state: state.clone(),
                    shutting_down: shutdown_flag(false),
                    lanes: InteractiveLanes::detached(),
                    overrides: RunOverrides::default(),
                },
                parked_transcript(),
                session_tx,
            ),
        )
        .await
        .expect("the loop ends once the channel closes");
        let history = state
            .machine_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("the correction's turn published the history");
        assert_eq!(
            history.len(),
            2,
            "the resumed turn and the correction behind it both stand — a \
             rewind to the ended runner's boundary would have emptied the \
            resumed one: {history:?}"
        );
        let first = history.first().expect("the resumed turn leads the history");
        assert!(
            format!("{first:?}").contains("the resumed turn"),
            "the resumed turn leads the conversation the new runner runs: {first:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_run_retires_stranded_tool_rows() {
        let runner = unreachable_runner().await;
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do a thing");
        drop(tx);
        // A tool stranded mid-flight: the engine will not fire its
        // completion for a run that fails outright.
        state
            .active_tools
            .lock()
            .unwrap()
            .push(dch_tui::ActiveTool {
                call_id: "call-strand".to_string(),
                name: "Read".to_string(),
                input_summary: String::new(),
                start: std::time::Instant::now(),
            });
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            state.active_tools.lock().unwrap().is_empty(),
            "a failed run leaves no spinner spinning behind it"
        );
    }

    #[tokio::test]
    async fn a_run_marks_the_agent_running_until_it_lands() {
        let runner = unreachable_runner().await;
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do a thing");
        drop(tx);

        let flag = Arc::clone(&state.agent_running);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("a refused connection resolves without hanging");
        assert!(
            !flag.load(Ordering::SeqCst),
            "the running flag settles false once the run lands, failed or not"
        );
        assert!(
            queued_prompts(&state).is_empty(),
            "nothing stays queued behind the landed run"
        );
    }

    #[tokio::test]
    async fn the_driver_ends_when_the_channel_closes() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);

        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            drive_submissions(runner, rx, state, shutdown_flag(false), parked_transcript()),
        )
        .await;
        assert!(ended.is_ok(), "a closed channel ends the driver promptly");
    }

    #[test]
    fn a_claim_the_exit_flag_gates_never_takes_its_entry() {
        // The exit's unclaimed-rows snapshot and a claim racing it read
        // the queue under one lock; the claim's half of that ordering
        // is the flag check inside `take_queued` itself. Without it, a
        // submission that passed the loop's earlier checks in the same
        // instant the flag rose would take its entry — spending it on
        // a run the exit is stopping — while the snapshot still read
        // it waiting, and the exit would echo it as never spent: the
        // prompt told twice, and resurrected on resume past its own
        // cancel record. The pin drives the guard directly: the flag
        // up, the entry present, the claim declines and the entry
        // stays.
        let (_, state) = TuiObserverState::new().into_observer();
        let id = push_queued(&state, "waiting when the exit fired".to_string());

        assert_eq!(
            take_queued(&state, id, &shutdown_flag(true)),
            None,
            "the claim declines under the flag — the exit's snapshot must \
             be able to read this entry as never spent"
        );
        assert_eq!(
            queued_prompts(&state)
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["waiting when the exit fired"],
            "a declined claim leaves its entry waiting — declined, not spent"
        );
        assert_eq!(
            take_queued(&state, id, &shutdown_flag(false)),
            Some("waiting when the exit fired".to_string()),
            "the flag down, the same claim takes its entry — the guard is \
             the exit's alone"
        );
    }

    #[tokio::test]
    async fn queued_prompts_do_not_run_after_shutdown() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first");
        queue_prompt(&state, &tx, "second");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(true),
                parked_transcript(),
            ),
        )
        .await
        .expect("the driver leaves the queue behind instead of draining it");

        let errors = state.errors.lock().unwrap();
        assert!(
            errors.is_empty(),
            "no submission is admitted once shutdown has begun — neither queued task ran"
        );
        assert_eq!(
            queued_prompts(&state)
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"],
            "unclaimed prompts stay queued, in order, for the panel to show — the queue outlives the driver"
        );
    }

    #[tokio::test]
    async fn a_submission_overtaken_by_shutdown_mid_park_never_starts() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let flag = shutdown_flag(false);

        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&flag));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        queue_prompt(&state, &tx, "late");
        flag.store(true, Ordering::SeqCst);
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the driver ends once the channel closes")
            .expect("the driver task itself does not panic");

        let errors = state.errors.lock().unwrap();
        assert!(
            errors.is_empty(),
            "a submission the shutdown flag overtakes mid-park never starts a run"
        );
        assert_eq!(
            queued_prompts(&state)
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["late"],
            "the overtaken prompt stays queued as unclaimed"
        );
    }

    #[tokio::test]
    async fn a_run_that_never_settles_leaves_no_undone_figure() {
        // The undone count is recorded beside a settled run, and a new
        // run's start is where an older run's answer stops being about
        // this session's plan: a run taken before it settles must leave
        // the exit silent, not repeating what came before it.
        let (_, state) = TuiObserverState::new().into_observer();
        state.record_todos_undone(2);
        state.mark_run_started();
        assert_eq!(
            state.todos_undone_captured(),
            None,
            "the new run's start retires the old run's figure"
        );
        let wedged = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        tear_down_driver_bounded(wedged).await;
        assert_eq!(
            state.todos_undone_captured(),
            None,
            "an aborted run states no figure of its own"
        );
    }

    #[tokio::test]
    async fn a_settled_figure_survives_a_driver_cut_down_late() {
        // The bound is on the join, not on what the run reported: a run
        // that settled its plan and then stalled going out still owns
        // the exit lines. Clearing the figure at the abort would tell
        // the user nothing about the work the run really left open.
        let (_, state) = TuiObserverState::new().into_observer();
        state.mark_run_started();
        state.record_todos_undone(1);
        let wedged = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        tear_down_driver_bounded(wedged).await;
        assert_eq!(
            state.todos_undone_captured(),
            Some(1),
            "a settled figure stands whatever the teardown does after"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_publication_in_flight_at_the_abort_lands_before_the_teardown_returns() {
        // The abort's reap: a poll already in flight on another worker
        // runs its synchronous tail out before the task can be dropped
        // — the exit-owned cancel's publication into the error lane is
        // exactly such a tail, and the late drain that follows the
        // teardown strips it by name. The teardown must not return
        // ahead of that publication, or the strip reads an empty lane
        // and the twin record lands after it. The driver here is one
        // synchronous sleep that spans the bound and publishes at its
        // end; without the reap the teardown would return at the bound
        // with the publication still pending. The runtime is
        // multi-threaded for the same reason: on one thread the sleep
        // would block the bound itself and the two orders could not
        // be told apart.
        let lane = Arc::new(std::sync::Mutex::new(Vec::new()));
        let published = Arc::clone(&lane);
        let in_flight = tokio::spawn(async move {
            std::thread::sleep(std::time::Duration::from_millis(550));
            published
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("the cancel's record".to_string());
            std::future::pending::<()>().await;
        });
        tear_down_driver_bounded(in_flight).await;
        let landed = lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            landed,
            ["the cancel's record"],
            "the in-flight publication landed before the teardown returned"
        );
    }

    #[test]
    fn the_session_context_carries_the_runs_flag_statements() {
        // The driver's halves read the flags through this one wiring: a
        // dropped population would leave every mid-session restore
        // inert while each arm's own test still passed, so the mapping
        // from the parsed args is pinned directly.
        use clap::Parser as _;
        let args = crate::args::Args::parse_from([
            "dch",
            "--model",
            "flag-model",
            "--reasoning-effort",
            "low",
        ]);
        let context = session_driver_context(
            dch_config::DchConfig::default(),
            std::sync::Arc::new(dch_config::DchConfig::default()),
            std::path::PathBuf::from("."),
            TuiObserverState::new(),
            shutdown_flag(false),
            InteractiveLanes::detached(),
            &args,
        );
        assert_eq!(context.overrides.model.as_deref(), Some("flag-model"));
        assert_eq!(context.overrides.reasoning_effort.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn a_cancelled_run_still_runs_the_prompt_queued_behind_it() {
        // The response is held back far longer than the test runs, so the
        // first run is provably mid-request when the cancel lands. What
        // the pin observes is the queued prompt's claim — the seam every
        // submit's turn takes — rather than its provider connection: the
        // canned endpoint answers one connection at a time behind the
        // delay, so a second request's arrival is not observable inside
        // this test's lifetime.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("too late")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "slow");
        queue_prompt(&state, &tx, "queued behind the cancel");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();

        let mut second_taken = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "queued behind the cancel")
            {
                second_taken = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            second_taken,
            "the prompt queued behind a cancelled run is claimed for its own \
             turn — one cancel stops the turn in flight, not the backlog: \
             claimed {:?}, waiting {:?}, errors {:?}, requests {}",
            claimed(&state),
            queued_prompts(&state),
            state.errors.lock().expect("errors lock"),
            server.request_count(),
        );
        assert!(
            queued_prompts(&state).is_empty(),
            "both prompts are claimed — the waiting section keeps no orphan"
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert_eq!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the cancelled run records its own failure; the queued prompt's \
                 turn is the next story, not part of this one"
            );
        }
        // The queued prompt's own request has no answer coming inside this
        // test's lifetime; the driver is stopped rather than left to wait
        // on it — the claim was already observed at the seam every turn
        // takes.
        driver.abort();
    }

    #[tokio::test]
    async fn a_run_ending_with_a_clean_plan_gets_no_nudge() {
        // The nudge exists for the filing-and-forgetting shape, not as
        // a tax on every run: a list the run closed — or a session
        // that never planned at all — costs nothing. The echo trail
        // proves the negative: no reminder text ever reaches the
        // transcript.
        let clean_script = vec![
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "completed"}]
                }),
            ),
            sse_text_turn("all done"),
        ];
        let server =
            SseServer::start_with_delay(clean_script, std::time::Duration::from_millis(50)).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let runner = with_nudge(dch_loop::Runner::builder(&config, dir.path()), &state)
            .build()
            .await
            .expect("offline runner construction");
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do the thing");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;

        let echoes = claimed(&state);
        assert!(
            !echoes.iter().any(|text| text.contains("reminder:")),
            "a clean plan nudges nothing: {echoes:?}"
        );
        assert_eq!(
            server.request_count(),
            2,
            "the tool-call turn and the speak turn — no third request, \
             because there was nothing to remind about"
        );
    }

    #[tokio::test]
    async fn a_run_stamps_its_start_and_clears_the_stamp_at_rest() {
        // The live elapsed line is only as honest as the stamp it
        // reads: the stamp rises beside the running flag at the
        // claim, stands for the run's whole flight — the nudge
        // included, which rides the same window — and returns to
        // zero with the flag, so nothing ticks between turns.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("the answer")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "a prompt");
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            state.run_started_ms.load(Ordering::SeqCst) != 0,
            "the stamp stands while the run is in flight"
        );
        assert!(
            state.agent_running.load(Ordering::SeqCst),
            "the flag stands beside it"
        );
        // The exit path clears both together.
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        assert_eq!(
            state.run_started_ms.load(Ordering::SeqCst),
            0,
            "the stamp returns to zero with the flag"
        );
    }

    #[tokio::test]
    async fn an_exit_stopping_the_followup_leaves_the_stopped_run_record() {
        // The reminder turn joins the run it nudges, so the exit's
        // cancel stops one flight, not two: the whole run resolves
        // cancelled and settles exactly as any stopped run settles —
        // one record row, the claimed prompt waiting again — with no
        // second settle to double the row or spend another request.
        let script = vec![
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "only", "subject": "the work",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::ZERO,
            ),
            (sse_text_turn("working on it"), std::time::Duration::ZERO),
            (
                sse_text_turn("noted, but not closing it"),
                std::time::Duration::from_secs(30),
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let runner = with_nudge(dch_loop::Runner::builder(&config, dir.path()), &state)
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt");
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if server.request_count() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 3,
            "the follow-up's request must be in flight before the exit lands"
        );
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let errors = state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            errors.len(),
            1,
            "one stop, one record row — the joined follow-up adds no second \
             settle: {errors:?}"
        );
        drop(errors);
        assert!(
            queued_prompts(&state)
                .iter()
                .any(|(_, text)| text == "the prompt"),
            "the stopped run's prompt waits again — the run never landed as a \
             whole: {:?}",
            queued_prompts(&state)
        );
    }

    #[tokio::test]
    async fn a_flag_raised_mid_run_declines_the_followup_without_a_cancel() {
        // The window the cancel-path twin cannot reach: the quit
        // ladder raises the shutdown flag, and only later — after the
        // session snapshot and the settle — does its cancel reach the
        // engine. In between, the run can end clean with the plan
        // undone, and the engine offers the reminder; the nudge's
        // leave-to-extend answers from the same flag the ladder
        // raised, so the offer dies unspent — no third request, no
        // echo row for a turn the exit was already abandoning.
        let script = vec![
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "t1", "subject": "close the list",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::ZERO,
            ),
            (
                sse_text_turn("the answer that landed"),
                std::time::Duration::from_millis(300),
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let gate = Arc::new(AtomicBool::new(false));
        let runner = with_gated_nudge(
            dch_loop::Runner::builder(&config, dir.path()),
            &state,
            Arc::clone(&gate),
        )
        .build()
        .await
        .expect("offline runner construction");
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt that ran");
        drop(tx);
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        for _ in 0..1800 {
            if server.request_count() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 1,
            "the run's first turn must be in flight before the flag rises"
        );
        gate.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        assert_eq!(
            server.request_count(),
            2,
            "the run's own two turns and no reminder — the raised flag \
             declined the offer before any request was spent"
        );
        assert!(
            claimed(&state)
                .iter()
                .all(|sent| !sent.contains("reminder:")),
            "no reminder row lands for a turn the exit was abandoning: {:?}",
            claimed(&state)
        );
    }

    #[tokio::test]
    async fn an_exit_cancelled_run_is_never_offered_its_followup() {
        // The engine offers the follow-up only on a clean end, so the
        // exit's own shape — the flag up, the cancel fired — never
        // reaches the offer: the run resolves cancelled and the
        // reminder turn is not injected, whatever the plan still
        // holds. The follow-up joins runs that land; it does not
        // chase runs that die.
        let script = vec![
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "t1", "subject": "close the list",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::from_millis(300),
            ),
            (
                sse_text_turn("the answer that landed"),
                std::time::Duration::ZERO,
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let runner = with_nudge(dch_loop::Runner::builder(&config, dir.path()), &state)
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt that ran");
        drop(tx);
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if server.request_count() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 1,
            "the run's first turn must be in flight before the exit lands"
        );
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        assert_eq!(
            server.request_count(),
            1,
            "the run's first turn was in flight and the follow-up was never \
             offered to a dying run"
        );
        assert!(
            claimed(&state)
                .iter()
                .all(|sent| !sent.contains("reminder:")),
            "no reminder text is sent — the offer never happened: {:?}",
            claimed(&state)
        );
    }

    #[tokio::test]
    async fn quitting_mid_run_hands_the_prompt_back_to_the_queue() {
        // The exit's cancel must not eat the prompt it stops: the
        // run is killed by the shutdown, the turn never landed, and
        // the words go back into the queue the session dies holding.
        // That queue is process state — the durable record is the
        // claim row already in the transcript, and a resume reads a
        // recorded stop as deliberate: the words are not lost, and
        // they are not re-run either.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("never lands")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt in flight");
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) != u64::MAX {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // The exit sequence exactly as run_tui performs it: the
        // shutdown flag, then the cancel, then the driver's settle.
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let queued = queued_prompts(&state);
        assert!(
            queued
                .iter()
                .any(|(_, text)| text == "the prompt in flight"),
            "the stopped run's prompt waits again in the dying queue — its claim \
             row is the record: {queued:?}"
        );
        let errors = state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            errors
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
            "the exit's stop lands as the same cancelled record an \
             in-app cancel leaves: {errors:?}"
        );
        assert!(
            state
                .machine_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "the exit's settle publishes the machine half — the \
             turn-end hook that persists the record fires on it"
        );
    }

    #[tokio::test]
    async fn a_run_ending_with_undone_todos_gets_exactly_one_nudge() {
        // The filing-and-forgetting shape the tool contract forbids
        // but a model mid-flow still falls into: a run that lands
        // with work left on its own plan receives exactly one
        // follow-up reminding it to close the list. The nudge's echo
        // rides the steered flag — a transcript row for honesty, but
        // not a plan boundary — and the nudged run's own list is
        // final: whatever it says, the driver moves on.
        let todo_script = vec![
            // turn 1: file the plan (1 pending), then speak
            sse_tool_call_turn(
                "TodoWrite",
                &serde_json::json!({
                    "todos": [{"id": "only", "subject": "the work",
                               "status": "pending"}]
                }),
            ),
            sse_text_turn("working on it"),
            // turn 2 (the nudge's reply): speak without touching the
            // list — the nudge had its chance and the driver moves on
            sse_text_turn("noted, but not closing it"),
        ];
        let server =
            SseServer::start_with_delay(todo_script, std::time::Duration::from_millis(50)).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let runner = with_nudge(dch_loop::Runner::builder(&config, dir.path()), &state)
            .build()
            .await
            .expect("offline runner construction");
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "do the thing");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        // Wait for the driver to drain: the run, reminder turn included,
        // completes against its canned turns.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;

        assert_eq!(
            server.request_count(),
            3,
            "the prompt's run took two requests (the tool-call turn, \
             then the speak turn) and the nudge one more — and only \
             one more: the nudged run left the list as it was and the \
             driver moved on"
        );
        // The nudge's echo is the display's record — held at the edge
        // until a boundary reads it, and flagged as not a plan
        // boundary when it lands.
        let echoes = claimed(&state);
        assert!(
            echoes.iter().any(|text| text.contains("reminder: 1 todo")),
            "the nudge's wording is the display's record: {echoes:?}"
        );
        // The nudge's echo rode the steered flag, so the display's
        // plan survived it — the Graduation::Sent drain clears the
        // plan only on unsteered echoes, and this test's state never
        // saw one after the prompt's own claim.
    }

    #[tokio::test]
    async fn a_claims_retirement_reaches_the_display_while_its_run_is_in_flight() {
        // The bridge the claim-edge publication owes: the runner drops
        // never-started entries at its second claim, the sink the
        // session registers hands the batch to the shared state the
        // same moment, and the frame's wake fires beside it — all while
        // the run that claimed them is still waiting on its held
        // provider answer. A publication that waited for the run to
        // resolve would leave the plan showing retired rows, and an
        // unwoken frame would show them a blink late besides.
        let server = SseServer::start_timed(vec![
            (
                sse_text_turn("first answered"),
                std::time::Duration::from_millis(50),
            ),
            (
                sse_text_turn("held answer"),
                std::time::Duration::from_secs(30),
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .with_retirement_sink(retirement_publisher(&state))
            .build()
            .await
            .expect("offline runner construction");
        {
            let mut store = runner.context().todos.lock().expect("todos lock");
            store.push(dch_tools::TodoEntry {
                id: "idea".to_string(),
                subject: "the filed idea".to_string(),
                description: String::new(),
                status: dch_tools::TodoStatus::Pending,
                active_form: None,
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first");
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        // The spared first claim settles before the wake under test
        // registers: one request made and no live claim left means the
        // first run is done, so the listener can only fire on the
        // second claim's own publication.
        let mut first_settled = false;
        for _ in 0..3600 {
            if server.request_count() == 1 && state.live_claim.load(Ordering::SeqCst) == u64::MAX {
                first_settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(first_settled, "the first prompt's run settles first");
        let wake = state.render_notify.listen();
        queue_prompt(&state, &tx, "second");
        let mut published = false;
        for _ in 0..3600 {
            // The lock is scoped to the read: the runtime is
            // single-threaded here, and a guard held across the sleep
            // below would wedge the sink's publication against it.
            let batch_landed = {
                let retired = state
                    .retired_plan_entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                retired.len() == 1
                    && retired.first().is_some_and(|entry| {
                        entry.id == "idea"
                            && entry.subject == "the filed idea"
                            && entry.generation == 1
                    })
            };
            if batch_landed {
                published = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            published,
            "the second claim's retirement is already in the display's buffer \
             while its run is still in flight"
        );
        assert!(
            !driver.is_finished(),
            "the run the claim precedes has not resolved — the publication \
             did not wait for it"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), wake)
                .await
                .is_ok(),
            "the publication wakes the frame beside it"
        );
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn the_driver_names_its_live_claim_while_the_turn_runs() {
        // The mirror the display's stale-edit guard reads: a claim
        // names the turn it took, and the landing clears the name back
        // to nothing. Held behind the delay, the turn is provably
        // mid-request when the mirror is read, so the pin observes the
        // name while the run is live — the only state in which a
        // correction may raise a cancel against it.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("slow answer")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "slow");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        let mut named = false;
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                named = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            named,
            "the claim names the turn it took while that turn runs"
        );
        cancel.cancel();
        let mut cleared = false;
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == u64::MAX {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            cleared,
            "the landing clears the mirror — no correction can name a \
             finished turn as the one in flight"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_resumed_untitled_session_names_from_the_restored_first_prompt() {
        // A resumed session whose naming never answered owes a retry,
        // and the retry must coin from the conversation's opening ask
        // — the restored history's first user message — not from the
        // follow-up that armed it. The run itself rides the claimed
        // prompt; only the naming side-call carries the old one.
        let server = SseServer::start_with_naming_delay(
            vec![(sse_text_turn("the tides answer"), std::time::Duration::ZERO)],
            std::time::Duration::ZERO,
        )
        .await;
        let workdir = tempfile::tempdir().expect("workdir tempdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, workdir.path())
            .with_history(vec![
                loopctl::message::Message::user("map the coastline roads"),
                loopctl::message::Message::assistant("the coast is mapped"),
            ])
            .build()
            .await
            .expect("runner over the restored history");
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let root = sessions.keep();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "test-model".to_string(),
            root.clone(),
        ));
        let (transcript, _worker) = TranscriptWorker::spawn(saver);
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            rx,
            DriverContext {
                config: config.clone(),
                config_base: std::sync::Arc::new(config),
                workdir: workdir.path().to_path_buf(),
                sessions_root: root.clone(),
                state: state.clone(),
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            transcript.clone(),
            session_tx,
        )));
        queue_prompt(&state, &tx, "now chart the tides");
        for _ in 0..1800 {
            if transcript.has_title() && server.request_count() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            transcript.has_title(),
            "the retry's answer lands on the session it armed for"
        );
        assert!(
            server.request_count() >= 1,
            "the claimed prompt's turn reached the provider"
        );
        let naming = server.naming_bodies();
        assert_eq!(
            naming.len(),
            1,
            "the untitled resume fires exactly one naming call"
        );
        let naming_body = naming.first().expect("the one naming request");
        assert!(
            naming_body.contains("map the coastline roads"),
            "the naming call coins from the restored first prompt: {naming_body}"
        );
        assert!(
            !naming_body.contains("now chart the tides"),
            "the arming follow-up never rides the naming call: {naming_body}"
        );
        assert!(
            server
                .bodies()
                .iter()
                .any(|body| body.contains("now chart the tides")),
            "the run itself rode the claimed prompt"
        );
        drop(tx);
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_cancel_the_correction_flag_does_not_name_keeps_its_error_row() {
        // The state the display's guard cannot exclude by timing
        // alone: its stand-down loses the race, and a correction's
        // flag is still standing when a cancelled run settles —
        // while that run's own claim names someone else. The settle
        // matches ids instead of trusting the race, so the stopped
        // turn keeps its row and the buffered correction runs on as
        // a turn of its own behind it.
        let server = SseServer::start_timed(vec![
            (sse_text_turn("held"), std::time::Duration::from_secs(2)),
            (
                sse_text_turn("replacement answered"),
                std::time::Duration::ZERO,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = queue_prompt(&state, &tx, "the running prompt");
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        let mut live = false;
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                live = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(live, "the prompt's turn is provably in flight");
        // The raced state, raised directly: a flag naming a turn
        // other than the one running, with that other correction
        // already buffered ahead of the cancel — the order the
        // display's commit path uses.
        state.correction_cancel.store(id + 100, Ordering::SeqCst);
        tx.send(Submission::Correct {
            id: id + 100,
            text: "the unrelated replacement".into(),
        })
        .expect("channel open");
        cancel.cancel();
        let mut reported = false;
        for _ in 0..1800 {
            if state
                .errors
                .lock()
                .expect("errors lock")
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string())
            {
                reported = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            reported,
            "a flag naming another run cannot hush this cancel: the \
             stopped turn posts its own error row"
        );
        // The correction was not spent silently: it is claimed as
        // its own turn behind the row, unrewound (its id names no
        // claim), and its echo is the turn's own record. The claim
        // and echo are the driver's contract; the engine's wire send
        // for a turn started immediately after a cancel is its own
        // reset race, not this settle's, so the pin reads the echo.
        let mut echoed = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "the unrelated replacement")
            {
                echoed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            echoed,
            "the buffered correction is claimed and echoed as its own \
             turn: claimed {:?}",
            claimed(&state)
        );
        drop(tx);
        driver.await.expect("driver settles cleanly");
        assert!(
            state
                .errors
                .lock()
                .expect("errors lock")
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
            "the stopped turn's row stands — no hushing for a foreign flag"
        );
    }

    #[tokio::test]
    async fn a_cancel_under_a_foreign_flag_keeps_its_error_row() {
        // The residual sliver the guard's stand-down leaves: the flag
        // still names an older turn when the cancel it raised stops a
        // run claimed under a newer id. Silence belongs only to the
        // run the flag names, so this settle posts its own error row —
        // the documented guarantee, held by comparing ids rather than
        // by trusting the two atomics' order.
        // The foreign prompt completes at once; the live prompt's
        // answer is held, so the cancel lands on the live run while
        // the flag names the finished one.
        let server = SseServer::start_timed(vec![
            (
                sse_text_turn("the older prompt's answer"),
                std::time::Duration::ZERO,
            ),
            (
                sse_text_turn("the answer that never lands"),
                std::time::Duration::from_secs(30),
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        // A pinned window keeps the discovery probes off the script:
        // every canned response belongs to a turn.
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        // A foreign id no live claim ever held: an older turn's
        // correction, buffered the way the display buffers it.
        let foreign = queue_prompt(&state, &tx, "an older prompt's wording");
        let live = queue_prompt(&state, &tx, "the claim in flight");
        let _ = foreign;
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == live {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            state.live_claim.load(Ordering::SeqCst),
            live,
            "the run under test is the live claim before the cancel lands"
        );
        tx.send(Submission::Correct {
            id: foreign,
            text: "an older correction".to_string(),
        })
        .expect("the stale correction sends");
        state.correction_cancel.store(foreign, Ordering::SeqCst);
        cancel.cancel();

        let mut row_appeared = false;
        for _ in 0..1800 {
            if state
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string())
            {
                row_appeared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            row_appeared,
            "a cancel whose flag names another turn is an error to report — \
             the wrongly cancelled run posts its own row"
        );
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn a_nudge_cancel_under_a_foreign_flag_keeps_its_error_row() {
        // The nudge's settle answers to the same id rule: mid-nudge
        // the claim the correction answers is still the prompt's own,
        // so a flag left standing for some other turn cannot hush the
        // nudge's cancel — its row stands, exactly as the turn's own
        // settle would post it.
        // The foreign prompt completes at once; the live prompt files
        // the plan and lands, and its nudge — the request held — is
        // the run the stale flag's cancel stops.
        let script = vec![
            (
                sse_text_turn("the older prompt's answer"),
                std::time::Duration::ZERO,
            ),
            (
                sse_tool_call_turn(
                    "TodoWrite",
                    &serde_json::json!({
                        "todos": [{"id": "only", "subject": "the work",
                                   "status": "pending"}]
                    }),
                ),
                std::time::Duration::ZERO,
            ),
            (sse_text_turn("working on it"), std::time::Duration::ZERO),
            (
                sse_text_turn("noted, but not closing it"),
                std::time::Duration::from_secs(30),
            ),
        ];
        let server = SseServer::start_timed(script).await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let (_, state) = TuiObserverState::new().into_observer();
        let runner = with_nudge(dch_loop::Runner::builder(&config, dir.path()), &state)
            .build()
            .await
            .expect("offline runner construction");
        let cancel = runner.cancel_signal();
        let (tx, rx) = mpsc::unbounded_channel();
        let foreign = queue_prompt(&state, &tx, "an older prompt's wording");
        // The live prompt files the plan; its id rides last_claim
        // through the nudge without the test naming it.
        queue_prompt(&state, &tx, "the claim in flight");
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        for _ in 0..1800 {
            if server.request_count() >= 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 4,
            "the nudge's request must be in flight before the cancel lands"
        );
        tx.send(Submission::Correct {
            id: foreign,
            text: "an older correction".to_string(),
        })
        .expect("the stale correction sends");
        state.correction_cancel.store(foreign, Ordering::SeqCst);
        cancel.cancel();

        let mut row_appeared = false;
        for _ in 0..1800 {
            if state
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string())
            {
                row_appeared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            row_appeared,
            "the nudge's settle answers to the same id rule — a foreign \
             flag cannot hush its cancel row"
        );
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn a_failed_run_still_runs_prompts_queued_behind_it() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first");
        queue_prompt(&state, &tx, "second");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("two refused runs settle without hanging");

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(
                errors.len(),
                2,
                "a failed — not cancelled — run admits the submission queued behind it"
            );
        }
        assert!(
            queued_prompts(&state).is_empty(),
            "both prompts are claimed"
        );
    }

    #[tokio::test]
    async fn a_stale_cancel_trip_does_not_eat_the_next_submission() {
        let runner = unreachable_runner().await;
        let cancel = runner.cancel_signal();
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();

        // Trip the signal while nothing is running — the shape of a
        // cancel pressed just as a run was ending. The pre-run reset
        // must clear it, or the next submission dies instantly.
        cancel.cancel();
        queue_prompt(&state, &tx, "still runs");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("the run settles without hanging");

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(
                errors.len(),
                1,
                "the submission runs rather than being eaten by the stale trip"
            );
            assert_ne!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the run must reach its provider failure, not die on a \
                 cancel nobody pressed for it"
            );
        }
    }

    #[tokio::test]
    async fn a_stale_trip_on_the_swapped_signal_does_not_eat_the_rebuilt_run() {
        // The rebuild half of the stale-trip rule: /model swaps the
        // shared signal cell, and the driver's pre-run re-arm must
        // read that swapped cell — resetting only a launch-captured
        // signal would leave the trip standing on the rebuilt runner,
        // whose first turn then dies on a cancel nobody pressed.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut context = unreachable_config();
        context.models = vec![dch_config::ModelProfile {
            model: "rebuilt-model".to_string(),
            ..dch_config::ModelProfile::default()
        }];
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        tx.send(Submission::Command {
            command: SessionCommand::Model {
                name: "rebuilt-model".to_string(),
            },
        })
        .expect("driver channel open");
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            rx,
            DriverContext {
                config: context,
                config_base: std::sync::Arc::new(dch_config::DchConfig::default()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state: state.clone(),
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        // The rebuild's verdict says the swap already happened — the
        // cell now holds the rebuilt runner's own signal.
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a model update is published")
            .expect("the session channel is open when the command lands");
        let verdict = match update {
            SessionUpdate::Notice { .. } => {
                tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                    .await
                    .expect("the verdict follows the receipt")
                    .expect("the session channel is open")
            }
            verdict => verdict,
        };
        assert_eq!(
            verdict,
            SessionUpdate::ModelName {
                name: "rebuilt-model".to_string()
            },
            "the switch landed on the rebuilt runner before the trip"
        );
        // Trip the swapped signal while nothing runs — the shape of a
        // cancel pressed just as a run was ending.
        state.cancel_signal.lock().unwrap().cancel();
        queue_prompt(&state, &tx, "still runs after the rebuild");
        drop(tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the rebuilt run settles without hanging");

        {
            let errors = state.errors.lock().unwrap();
            assert_eq!(
                errors.len(),
                1,
                "the rebuilt runner's submission runs rather than being \
                 eaten by the stale trip"
            );
            assert_ne!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the run must reach its provider failure, not die on a \
                 cancel nobody pressed for it"
            );
        }
    }

    #[tokio::test]
    async fn a_dropped_prompt_is_never_sent() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt that stays");
        let dropped = queue_prompt(&state, &tx, "the prompt taken out of the list");
        drop(tx);
        // The panel's delete key, issued as the display issues it: the entry
        // leaves the queue while its id is still travelling, which is the
        // only shape of drop the driver can be handed.
        state.queued.lock().expect("queue lock").remove(dropped);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("the surviving run settles without hanging");

        {
            let errors = state.errors.lock().expect("errors lock");
            assert_eq!(
                errors.len(),
                1,
                "one id finds its prompt and one finds nothing — an id naming \
                 nothing starts no run, so a dropped prompt never reaches a \
                 provider"
            );
        }
        assert!(
            queued_prompts(&state).is_empty(),
            "the surviving prompt is claimed and the dropped one leaves nothing \
             stranded behind it"
        );
        assert_eq!(
            claimed(&state),
            ["the prompt that stays"],
            "a prompt removed before its turn is never shown as sent"
        );
    }

    #[tokio::test]
    async fn a_claimed_prompt_echoes_into_the_transcript_exactly_once() {
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "first prompt");
        queue_prompt(&state, &tx, "second prompt");
        drop(tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            drive_submissions(
                runner,
                rx,
                state.clone(),
                shutdown_flag(false),
                parked_transcript(),
            ),
        )
        .await
        .expect("two refused runs settle without hanging");

        assert_eq!(
            claimed(&state),
            ["first prompt", "second prompt"],
            "one row per prompt that ran, in the order they ran — the display \
             echoes what the driver published, so a second publish would put \
             the same prompt in the transcript twice, and once is what the \
             model was actually given"
        );
    }

    #[test]
    fn a_peek_keeps_every_submission_its_place() {
        // The interrupt's channel peek: pulling the correction the display
        // named must not eat anything queued around it — a prompt behind
        // the correction rides `revived` back to the front of the queue,
        // a correction that names nothing walks the whole channel into
        // `revived` untouched, and the drain order puts every pulled
        // submission back ahead of what never left the channel.
        // The prompt queues first, the correction behind it — the interrupt
        // finds its name at the back of the line, and everything the peek
        // walked past to get there rides `revived` in the channel's own
        // order, the correction closing the line so the drain replays it
        // next.
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Submission::Run { id: 8 })
            .expect("the queued prompt sends");
        tx.send(Submission::Correct {
            id: 7,
            text: "the fix".to_string(),
        })
        .expect("the correction sends");
        let mut revived = std::collections::VecDeque::new();
        assert!(
            pull_pending_correction(7, &mut rx, &mut revived),
            "the named correction is found"
        );
        assert_eq!(
            revived.len(),
            2,
            "the peek ate nothing: what it walked past and the correction \
             both ride revived"
        );
        assert!(
            matches!(revived[0], Submission::Run { id: 8 }),
            "the prompt the peek walked past rides revived, not the void"
        );
        assert!(
            matches!(revived[1], Submission::Correct { id: 7, .. }),
            "the matched correction rides revived too — the drain replays \
             it next"
        );

        // A name the channel does not hold: the peek walks everything into
        // `revived` — the unnamed correction included — and answers false,
        // so the run records its cancel as the user's own.
        tx.send(Submission::Correct {
            id: 1,
            text: "someone else's fix".to_string(),
        })
        .expect("the stray correction sends");
        tx.send(Submission::Run { id: 9 })
            .expect("the later prompt sends");
        let mut revived = std::collections::VecDeque::new();
        assert!(
            !pull_pending_correction(123, &mut rx, &mut revived),
            "a name the channel does not hold is not the display's cancel"
        );
        assert_eq!(
            revived.len(),
            2,
            "the walk kept every pulled submission for the drain"
        );
        assert!(
            matches!(revived[0], Submission::Correct { id: 1, .. })
                && matches!(revived[1], Submission::Run { id: 9 }),
            "revived keeps the channel's own order"
        );
    }
}
