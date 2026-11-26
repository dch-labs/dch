//! Correction rescue: matching an edited prompt against the
//! turns already out or in flight, cutting the named history
//! with a notice, and handing stranded wording back at exit.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dch_tui::Submission;
use dch_tui::TuiObserverState;
use tokio::sync::mpsc;

/// What the driver tells the user about a correction's history cut.
///
/// The correction names the committed length its superseded turn
/// started from, and every way the rewind can land divides into one of
/// six shapes. The engine's answer alone does not separate them —
/// `Ok(0)` is a history the run's compaction folded past the cut, one
/// a fold returned to exactly the cut, and one the superseded turn
/// never wrote into — so the verdict reads the length before and after
/// the cut beside the result, and every non-clean shape is something
/// the user should hear: a silent partial replace would pass as the
/// replacement.
///
/// Pure over its inputs so every shape is pinnable without a canned
/// provider; the driver feeds it the recorded length, the committed
/// lengths around the rewind, and the rewind's own result.
///
/// Returns the notice to post, or `None` when the cut landed exactly
/// where the correction said it would and there is nothing to say.
fn correction_cut_notice(
    before: usize,
    committed_before_cut: usize,
    removed: &Result<usize, loopctl::error::LoopError>,
    committed_after_cut: usize,
    interrupted: bool,
) -> Option<String> {
    match removed {
        Err(error) => Some(format!(
            "the superseded turn stayed in the conversation: {error}"
        )),
        _ if committed_before_cut == before && interrupted => None,
        Ok(_)
            if committed_before_cut == before
                && committed_after_cut == before
                && !interrupted =>
        {
            Some(
                "the turn it superseded had stopped before it wrote anything; the fix was sent in its place"
                    .to_string(),
            )
        }
        _ if committed_before_cut <= before => Some(
            "the superseded turn had already been compacted away, so the correction followed it rather than replacing it"
                .to_string(),
        ),
        Ok(_) if committed_after_cut != before => Some(
            "the correction's cut landed on a different boundary than the turn it named"
                .to_string(),
        ),
        Ok(_) => None,
    }
}

/// Whether the correction `wanted` is already waiting in the channel.
///
/// Pulls submissions until it finds it or the channel runs dry; every
/// submission pulled — the matched correction included — is pushed
/// onto `revived`, which the loop drains ahead of the channel, so
/// peeking costs no submission its place in line. The display sends a
/// correction before raising the cancel that stops its turn and names
/// it on the shared flag, so by the time a cancelled run settles its
/// correction is already buffered.
pub(crate) fn pull_pending_correction(
    wanted: u64,
    receiver: &mut mpsc::UnboundedReceiver<Submission>,
    revived: &mut std::collections::VecDeque<Submission>,
) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(submission @ Submission::Correct { id, .. }) if id == wanted => {
                revived.push_back(submission);
                return true;
            }
            Ok(submission) => revived.push_back(submission),
            Err(_) => return false,
        }
    }
}

/// Collect one correction's words for the exit's transcript echo.
///
/// The queue is process state and dies with the session, so the one
/// durable hand-back a correction's words can take is a transcript
/// row: the exit echo drains the shared buffer this fills.
pub(crate) fn rescue_correction_text(state: &TuiObserverState, text: String) {
    state
        .rescued_corrections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(text);
}

/// Hand every buffered correction to the exit's transcript echo.
///
/// The exit's cancel can land on a channel still holding corrections
/// that name other prompts — sent the moment their edits committed,
/// their words already gone from the queue with the claims — and the
/// exit can also win the race to the loop top over a correction whose
/// own run just landed. Every correction still buffered at the exit
/// drain is collected here rather than dropped with the channel: the
/// queue it once could wait in dies with the process, and an
/// unclaimed row is the one durable record the words can take.
/// Returns how many wordings were collected.
pub(crate) fn rescue_stranded_corrections(
    revived: &mut std::collections::VecDeque<Submission>,
    receiver: &mut mpsc::UnboundedReceiver<Submission>,
    state: &TuiObserverState,
) -> usize {
    let mut handed = 0usize;
    while let Some(submission) = revived.pop_front() {
        if let Submission::Correct { text, .. } = submission {
            rescue_correction_text(state, text);
            handed = handed.saturating_add(1);
        }
    }
    while let Ok(submission) = receiver.try_recv() {
        if let Submission::Correct { text, .. } = submission {
            rescue_correction_text(state, text);
            handed = handed.saturating_add(1);
        }
    }
    handed
}

/// The corrected wording buffered for the stopped run's claim, if one
/// is already on its way.
///
/// A correction buffered for the interrupted claim carries the newest
/// wording — the words are the user's, and the newest ones win — so
/// the exit collects it for the transcript echo: an unclaimed row is
/// the one durable hand-back the words take. Without one, the claimed
/// prompt's own text was
/// already sent, and its row already stands in the transcript — the
/// record the deliberate-stop rule reads — so nothing is collected.
pub(crate) fn pull_corrected_wording(
    claim: Option<ClaimBoundary>,
    receiver: &mut mpsc::UnboundedReceiver<Submission>,
    revived: &mut std::collections::VecDeque<Submission>,
) -> Option<String> {
    claim.and_then(|(id, _, _)| {
        if pull_pending_correction(id, receiver, revived) {
            revived.pop_back().and_then(|submission| match submission {
                Submission::Correct { text, .. } => Some(text),
                Submission::Run { .. } | Submission::Command { .. } => None,
            })
        } else {
            None
        }
    })
}

/// Whether a run's error is the exit's own cancel of it.
///
/// A `Cancelled` result alone cannot say whose hand stopped the turn:
/// an in-app cancel and the exit's stop resolve identically at the
/// engine. The shutdown flag separates them — the exit raises it
/// before its cancel fires — so a cancelled run under it is the exit
/// taking the turn with it, owing the exit's own record and requeue
/// rather than an error row.
pub(crate) fn exit_owns_cancel(
    err: &loopctl::error::LoopError,
    shutting_down: &Arc<AtomicBool>,
) -> bool {
    matches!(err, loopctl::error::LoopError::Cancelled) && shutting_down.load(Ordering::SeqCst)
}

/// The boundary a claimed turn recorded.
///
/// The claim's id, the committed length its turn started from, and the
/// history that length named — the prefix a later correction's cut
/// must still find in place before it rewinds.
pub(crate) type ClaimBoundary = (u64, usize, std::sync::Arc<Vec<loopctl::message::Message>>);

/// Cut the superseded turn out of the committed history and post
/// the verdict's notice, if any.
///
/// The correction names the committed length its turn started from;
/// the cut is attempted wherever that claim still stands, and the
/// notice — or the verdict's silence — is the driver's honest word
/// about how the rewind landed. The claim's own history rides beside
/// the length: a compaction during the turn folds the conversation
/// and regrowth can carry its length back past the recorded boundary,
/// so the length alone cannot say the cut still names the turn — the
/// conversation must still begin with exactly the messages the claim
/// recorded, and where it does not, the correction follows the fold
/// with a notice instead of cutting into the turns that came after.
pub(crate) fn apply_correction_cut(
    runner: &mut dch_loop::Runner,
    state: &TuiObserverState,
    before: usize,
    interrupted: bool,
    claimed: &std::sync::Arc<Vec<loopctl::message::Message>>,
) {
    let committed = runner.committed_history();
    let boundary_moved = committed.len() >= before && {
        let standing = boundary_digest(committed.get(..before));
        let recorded = boundary_digest(Some(claimed.as_slice()));
        standing != recorded
    };
    if boundary_moved {
        state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(
                "the conversation was rewritten since the superseded turn began — a compaction \
                 folded it into the summary that stands there now, so the correction follows \
                 the fold rather than cutting into the turns that came after it"
                    .to_string(),
            );
        return;
    }
    let removed = runner.rewind_history_to(before);
    let notice = correction_cut_notice(
        before,
        committed.len(),
        &removed,
        runner.committed_history().len(),
        interrupted,
    );
    if let Some(notice) = notice {
        state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(notice);
    }
}

/// The prefix identity a correction's boundary is checked against.
///
/// The claimed history's Debug rendering stands in for content
/// equality: the cut is placed only where the conversation still
/// begins with exactly the messages the claim recorded, so a
/// compaction-then-regrowth cannot pass its rewritten prefix off as
/// the turn the correction named.
fn boundary_digest(history: Option<&[loopctl::message::Message]>) -> String {
    format!("{history:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::driver::drive_submissions;
    use crate::tui::test_support::*;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use dch_tui::Submission;
    use dch_tui::TuiApp;
    use dch_tui::TuiObserverState;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    #[test]
    fn a_cut_that_lands_exactly_where_the_correction_named_says_nothing() {
        // The clean shape: the history held four messages of turn past
        // the recorded length, the rewind removed exactly those four,
        // and the committed history ends where the superseded turn
        // began. Nothing to report — the replacement happened.
        assert_eq!(
            correction_cut_notice(10, 14, &Ok(4), 10, false),
            None,
            "an exact cut is the correction doing what it said"
        );
    }

    #[test]
    fn a_history_folded_past_the_cut_says_compacted_away() {
        // The shrink shape: a compaction during the run dropped the
        // history below the recorded length, so the turn being
        // superseded is already gone and the correction follows the
        // summary that replaced it rather than swapping in place.
        let notice =
            correction_cut_notice(10, 8, &Ok(0), 8, false).expect("the shape is not silent");
        assert!(
            notice.contains("compacted away"),
            "the user learns the correction went on top, not in place: {notice}"
        );
    }

    #[test]
    fn an_uninterrupted_turn_that_committed_nothing_says_stopped_not_compacted() {
        // The equal shape without the correction's own hand: a turn
        // that ended having written nothing leaves the history
        // standing exactly where the cut lands, and `Ok(0)` reads the
        // same as a fold back to the recorded length. Nothing was
        // compacted, and the row must not say so — the verdict names
        // the stopped turn, the shape a session actually reaches.
        let notice =
            correction_cut_notice(10, 10, &Ok(0), 10, false).expect("the shape is not silent");
        assert!(
            notice.contains("stopped before it wrote anything"),
            "a turn that never wrote is named as stopped, not folded away: {notice}"
        );
    }

    #[test]
    fn an_interrupted_turn_that_committed_nothing_says_nothing() {
        // The stopped shape: the correction cancelled the turn mid-flight
        // and it committed nothing, so the history already stands where
        // the cut would land — the corrected turn is the next story, and
        // there is nothing to report about a swap that needed no cut.
        assert_eq!(
            correction_cut_notice(10, 10, &Ok(0), 10, true),
            None,
            "a stop the correction itself caused is not news"
        );
    }

    #[test]
    fn an_interrupted_turn_whose_history_folded_below_the_cut_says_compacted_away() {
        // The stopped-and-folded shape: the correction cancelled the
        // turn mid-flight, and a compaction during that same turn
        // dropped the history below the recorded length — the
        // superseded turn is gone into a summary, exactly the fold the
        // non-interrupted twin reports. Interrupted or not, the
        // correction goes on top of the fold rather than in place, and
        // silence would pass it as the replacement.
        let notice =
            correction_cut_notice(10, 8, &Ok(0), 8, true).expect("the shape is not silent");
        assert!(
            notice.contains("compacted away"),
            "a fold below the named length is news even when the run was \
             stopped: {notice}"
        );
    }

    #[test]
    fn a_refused_cut_under_the_interrupted_shape_still_reports_the_error() {
        // The refused-and-stopped shape: the machine declined the
        // rewind while the lengths beside it read as the silent
        // interrupted case. A refused cut is never silent — the turn
        // the correction meant to replace is still in the
        // conversation, and the error is the only thing that says so.
        let refused = loopctl::error::LoopError::InvalidInput("a turn is in flight".to_string());
        let notice = correction_cut_notice(10, 10, &Err(refused), 10, true)
            .expect("the shape is not silent");
        assert!(
            notice.contains("stayed in the conversation"),
            "the refusal outranks the lengths beside it: {notice}"
        );
    }

    #[tokio::test]
    async fn a_correction_cut_after_compaction_and_regrowth_refuses_to_rewind() {
        // The regrowth shape the lengths alone cannot see: a fold
        // during the superseded turn shrinks the conversation, and the
        // turns that follow carry its length back past the recorded
        // boundary. The prefix check refuses the rewind — the fold's
        // summary and the later turns keep their place — and the
        // notice says the correction went on top of the fold instead
        // of cutting into what came after it.
        let mut runner =
            dch_loop::Runner::builder(&unreachable_config(), std::path::Path::new("."))
                .with_history(vec![
                    loopctl::message::Message::user("turn one"),
                    loopctl::message::Message::assistant("answer one"),
                    loopctl::message::Message::user("turn two"),
                    loopctl::message::Message::assistant("answer two"),
                    loopctl::message::Message::user("turn three"),
                    loopctl::message::Message::assistant("answer three"),
                ])
                .build()
                .await
                .expect("runner over the recorded history");
        let (_, state) = TuiObserverState::new().into_observer();
        let claimed = std::sync::Arc::new(runner.committed_history());
        let before = claimed.len();
        runner
            .rewind_history_to(2)
            .expect("the fold lands while the loop is at rest");
        for turn in 0..5 {
            runner
                .run(&format!("the turn after the fold {turn}"))
                .await
                .ok();
        }
        let regrown = runner.committed_history().len();
        assert!(
            regrown >= before,
            "the fixture must reach the regrown shape the fold hides: {regrown} vs {before}"
        );
        apply_correction_cut(&mut runner, &state, before, false, &claimed);
        assert_eq!(
            runner.committed_history().len(),
            regrown,
            "the rewritten conversation refuses the rewind — the fold and the \
             turns after it stand untouched"
        );
        let errors = state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(errors.len(), 1, "the stale boundary is reported once");
        assert!(
            errors
                .first()
                .expect("the notice is present")
                .contains("rewritten since the superseded turn began"),
            "the notice names the fold the correction rides on: {:?}",
            errors.first()
        );
    }

    #[tokio::test]
    async fn a_cut_over_an_untouched_conversation_still_lands_in_place() {
        // The control for the prefix check: the conversation the claim
        // recorded is still the conversation the cut sees — the check
        // passes and the rewind removes exactly the superseded turn,
        // silently, as it always did.
        let mut runner =
            dch_loop::Runner::builder(&unreachable_config(), std::path::Path::new("."))
                .with_history(vec![
                    loopctl::message::Message::user("turn one"),
                    loopctl::message::Message::assistant("answer one"),
                    loopctl::message::Message::user("turn two"),
                    loopctl::message::Message::assistant("answer two"),
                ])
                .build()
                .await
                .expect("runner over the recorded history");
        let (_, state) = TuiObserverState::new().into_observer();
        let claimed = std::sync::Arc::new(runner.committed_history());
        let before = claimed.len();
        runner
            .run("the superseded turn")
            .await
            .expect_err("the unreachable provider refuses the turn");
        apply_correction_cut(&mut runner, &state, before, false, &claimed);
        assert_eq!(
            runner.committed_history().len(),
            before,
            "the cut removed exactly the turn the correction named"
        );
        let errors = state
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            errors.is_empty(),
            "an exact cut over an untouched conversation stays silent: {errors:?}"
        );
    }

    #[test]
    fn a_cut_landing_on_a_different_boundary_names_it() {
        // The mismatch shape: the rewind removed six where the
        // correction named four — a boundary snap past a fold took more
        // than the turn — and a partial replace must not pass as the
        // replacement.
        let notice =
            correction_cut_notice(10, 14, &Ok(6), 8, false).expect("the shape is not silent");
        assert!(
            notice.contains("different boundary"),
            "the wrong suffix is named, not silently accepted: {notice}"
        );
    }

    #[test]
    fn a_refused_cut_reports_the_error_verbatim() {
        // Not reachable from the submission loop — the machine refuses
        // only while a turn is in flight — but the verdict has to say
        // so if it ever happens, because the correction still goes out
        // over the top of the turn it meant to replace.
        let refused = loopctl::error::LoopError::InvalidInput("a turn is in flight".to_string());
        let notice = correction_cut_notice(10, 14, &Err(refused), 14, false)
            .expect("the shape is not silent");
        assert!(
            notice.contains("stayed in the conversation") && notice.contains("a turn is in flight"),
            "the refusal names itself and its reason: {notice}"
        );
    }

    #[tokio::test]
    async fn the_exit_hands_stranded_corrections_back_too() {
        // A correction naming some other prompt can be buffered when
        // the exit's cancel lands — the display sends it the moment
        // an edit commits, claim or no claim. Its words are the
        // user's, and the words are already gone from the queue (the
        // prompt was claimed), so the exit collects every stranded
        // correction for the transcript echo rather than dropping
        // them with the channel.
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
        let id = queue_prompt(&state, &tx, "the wording that went out");
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tx.send(Submission::Correct {
            id: id + 100,
            text: "a different prompt's settled wording".to_string(),
        })
        .expect("the foreign correction sends");
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            rescued
                .iter()
                .any(|text| text == "a different prompt's settled wording"),
            "the stranded correction's words are collected for the transcript echo — never \
             dropped with the channel: {rescued:?}"
        );
        drop(rescued);
        let queued = queued_prompts(&state);
        assert!(
            queued
                .iter()
                .any(|(_, text)| text == "the wording that went out"),
            "and the stopped run's own wording waits too: {queued:?}"
        );
    }

    #[tokio::test]
    async fn the_exit_reclaims_the_newest_wording_for_the_stopped_run() {
        // The words are the user's, and the newest wording is the
        // one a buffered correction carries: when the exit stops a
        // run whose correction was already on its way, the collected
        // wording — the one the transcript echo writes back as an
        // unclaimed row — is the corrected text, not the wording it
        // was replacing.
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
        let id = queue_prompt(&state, &tx, "the wording that went out");
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == id {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tx.send(Submission::Correct {
            id,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        down.store(true, Ordering::SeqCst);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            rescued.iter().any(|text| text == "the wording settled on"),
            "the collected correction carries the newest wording — the \
             newest words win: {rescued:?}"
        );
    }

    #[tokio::test]
    async fn an_exit_before_any_run_collects_the_buffered_correction() {
        // The exit drain, not the flag check alone: with the flag
        // already up, the driver never admits a submission, and
        // everything still buffered — a waiting prompt's run and a
        // correction behind it — splits by what its words need. The
        // run's entry stays in the queue the exit's snapshot reads;
        // the correction's words left no entry anywhere, so they are
        // collected for the transcript echo.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let waiting = queue_prompt(&state, &tx, "never started");
        tx.send(Submission::Correct {
            id: waiting,
            text: "the settled wording".to_string(),
        })
        .expect("the correction sends");
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
        .expect("the driver settles at once under a raised flag");

        assert!(
            claimed(&state).is_empty(),
            "nothing runs once the flag is up: {:?}",
            claimed(&state)
        );
        let queued = queued_prompts(&state);
        assert!(
            queued.iter().any(|(_, text)| text == "never started"),
            "the unclaimed prompt's entry stays for the exit's snapshot: {queued:?}"
        );
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            rescued.as_slice(),
            ["the settled wording".to_string()].as_slice(),
            "the buffered correction is collected for the transcript echo, not \
             dropped with the channel"
        );
    }

    #[tokio::test]
    async fn a_correction_the_exit_overtakes_a_landed_run_is_collected() {
        // The run landed Ok and the exit won the race to the loop
        // top before the driver dequeued the correction committed
        // over it: the fix's words travelled only in the channel, and
        // dropping them there would lose the user's newest words
        // with no row anywhere — the exit drain collects them for
        // the transcript echo instead.
        let server = SseServer::start_timed(vec![(
            sse_text_turn("the answer that landed"),
            std::time::Duration::from_millis(300),
        )])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the wording that went out");
        let down = shutdown_flag(false);
        let driver = spawn_driver(runner, rx, state.clone(), Arc::clone(&down));
        for _ in 0..1800 {
            if state.live_claim.load(Ordering::SeqCst) == running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tx.send(Submission::Correct {
            id: running,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        down.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), driver).await;
        assert_eq!(
            claimed(&state),
            ["the wording that went out".to_string()],
            "the run itself completed once — only its correction was overtaken"
        );
        let rescued = state
            .rescued_corrections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            rescued.as_slice(),
            ["the wording settled on".to_string()].as_slice(),
            "the overtaken correction is collected for the transcript echo, not \
             dropped with the channel"
        );
    }

    #[tokio::test]
    async fn a_correction_during_the_todo_nudge_is_the_next_story_without_an_error_row() {
        // The nudge runs while the display still names the original
        // prompt as the live claim, so a correction committed against
        // that claim stops the nudge — the run that happened to be in
        // flight, not the turn the correction replaces. The buffered
        // correction is the next story: it runs as the very next turn
        // and no bare "cancelled" row surfaces for a stop the user
        // caused by saving an edit.
        let script = vec![
            // turn 1: file a one-item plan with the item pending, then
            // speak — the shape that arms the nudge.
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
            // the nudge's reply, held: the correction lands mid-nudge
            (
                sse_text_turn("noted, but not closing it"),
                std::time::Duration::from_secs(30),
            ),
            // the corrected turn's reply, answered at once
            (
                sse_text_turn("the settled answer"),
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
        let superseded_id = queue_prompt(&state, &tx, "the wording that went out");
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        // The nudge is provably the request in flight: the server has
        // read it and is holding its answer.
        for _ in 0..1800 {
            if server.request_count() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            server.request_count() >= 3,
            "the nudge's request must be in flight before the correction lands"
        );
        tx.send(Submission::Correct {
            id: superseded_id,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        state
            .correction_cancel
            .store(superseded_id, Ordering::SeqCst);
        cancel.cancel();

        let mut corrected_claimed = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "the wording settled on")
            {
                corrected_claimed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            corrected_claimed,
            "the buffered correction runs as the very next turn after the \
             nudge it stopped: claimed {:?}, errors {:?}",
            claimed(&state),
            state.errors.lock().expect("errors lock"),
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert!(
                !errors
                    .iter()
                    .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
                "the nudge's cancel is the correction's own hand, not an \
                 error to report — the corrected turn is the next story: \
                 {errors:?}"
            );
        }
        drop(tx);
        driver.abort();
    }

    #[tokio::test]
    async fn a_stale_edit_correction_spares_the_turn_in_flight() {
        // The interleaving: the edit opens on
        // waiting prompt A, A's turn runs and lands, and a later
        // prompt is in flight when Enter saves the stale wording. The
        // correction names A — not the turn going — so nothing is
        // cancelled: the live turn lands its own answer, the fix runs
        // as a turn of its own behind it, and no error row reports a
        // stop nobody raised.
        fn press(app: &mut TuiApp, code: KeyCode, mods: KeyModifiers) {
            app.handle_event(&Event::Key(KeyEvent::new(code, mods)));
        }
        fn type_into(app: &mut TuiApp, text: &str) {
            for c in text.chars() {
                press(app, KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
        fn render(app: &mut TuiApp) {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
        }

        let server = SseServer::start_with_delay(
            vec![
                sse_text_turn("first answered"),
                sse_text_turn("second answered"),
                sse_text_turn("fixed answered"),
            ],
            std::time::Duration::from_millis(300),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        // An explicit window keeps discovery's probe requests away from
        // the canned bodies: every connection this test makes is a
        // turn's own request, so the count at the end is exact.
        config.api.context_window = Some(200_000);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (observer, state) = TuiObserverState::new().into_observer();
        drop(observer);
        let mut app = TuiApp::from_observer_state(config, state.clone());
        let cancels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&cancels);
        app.set_run_canceller(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        app.set_submit_tx(tx);

        // Two prompts queue, then the edit opens on the first before
        // the driver takes either — the claim of A happens under the
        // open edit, exactly as production cannot help doing.
        type_into(&mut app, "the wording that landed");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        type_into(&mut app, "the prompt left running");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(
            app.input(),
            "the wording that landed",
            "the focus chord's `e` lifts waiting prompt A into the composer — \
             the premise the correction below commits against"
        );
        let ids: Vec<u64> = {
            let queue = state.queued.lock().expect("queue lock");
            (0..queue.len())
                .filter_map(|index| queue.get(index).map(|entry| entry.id))
                .collect()
        };
        assert_eq!(ids.len(), 2, "both prompts wait under the open edit");
        let later_id = ids[1];

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        // A lands; the later prompt becomes the turn in flight — the
        // delay holds each answer long enough that the claim is
        // observably live.
        let mut later_live = false;
        for _ in 0..3600 {
            if state.live_claim.load(Ordering::SeqCst) == later_id {
                later_live = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            later_live,
            "the later prompt is the turn in flight when the stale edit saves"
        );

        // Enter saves the stale wording while that unrelated turn runs.
        type_into(&mut app, " — fixed after the fact");
        render(&mut app);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            cancels.load(Ordering::SeqCst),
            0,
            "a correction naming a turn that already landed cancels \
             nothing — the run in flight is not its conversation"
        );
        assert_eq!(
            state.correction_cancel.load(Ordering::SeqCst),
            u64::MAX,
            "the suppression flag stays empty, so the turn in flight keeps \
             whatever error row is rightly its own"
        );

        // The spared turn lands its own answer, then the fix runs as a
        // turn of its own — the resolved history carries both answers,
        // exactly the three requests the turns made, and nothing
        // reports a cancel.
        let mut both_answered = false;
        for _ in 0..3600 {
            let history = state.machine_history.lock().expect("history lock").clone();
            let answered = history.is_some_and(|messages| {
                let texts = assistant_texts(&messages[..]);
                texts.iter().any(|text| text.as_str() == "second answered")
                    && texts.iter().any(|text| text.as_str() == "fixed answered")
            });
            if answered {
                both_answered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            both_answered,
            "the spared turn lands and the correction runs behind it: {:?}",
            state
                .machine_history
                .lock()
                .expect("history lock")
                .as_ref()
                .map(|messages| {
                    assistant_texts(&messages[..])
                        .iter()
                        .map(|text| text.as_str())
                        .collect::<Vec<_>>()
                })
        );
        assert_eq!(
            server.request_count(),
            3,
            "exactly the three turns' requests reached the provider — no \
             cancels, no retries"
        );
        assert_eq!(
            cancels.load(Ordering::SeqCst),
            0,
            "the stale correction never raised the runner's cancel"
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert!(
                errors.is_empty(),
                "no run was cancelled and nothing failed: {errors:?}"
            );
        }
        drop(app); // releases the submission channel; the driver parks
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_correction_interrupts_the_run_it_replaces_without_an_error_row() {
        // The first turn is held mid-request when the correction lands,
        // sent exactly the way the display sends it — the Correct first,
        // the runner's cancel raised beside it. The driver must read the
        // cancellation as the correction's own hand: the superseded turn
        // settles cancelled, the corrected wording runs in its place
        // without waiting the held response out, and no cancel error row
        // surfaces for a stop the user caused by saving an edit.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("superseded answer")],
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
        let superseded_id = queue_prompt(&state, &tx, "the wording that went out");
        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // The display's exact order: the correction is sent, then the
        // cancel is raised beside it.
        tx.send(Submission::Correct {
            id: superseded_id,
            text: "the wording settled on".to_string(),
        })
        .expect("the correction sends");
        drop(tx);
        // The display names the correction on the shared flag before
        // raising the cancel — its way of saying whose hand it was.
        state
            .correction_cancel
            .store(superseded_id, Ordering::SeqCst);
        cancel.cancel();

        let mut corrected_claimed = false;
        for _ in 0..1800 {
            if claimed(&state)
                .iter()
                .any(|sent| sent == "the wording settled on")
            {
                corrected_claimed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            corrected_claimed,
            "the corrected wording runs without waiting the held turn out: \
             claimed {:?}, errors {:?}, requests {}",
            claimed(&state),
            state.errors.lock().expect("errors lock"),
            server.request_count(),
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert!(
                !errors
                    .iter()
                    .any(|row| row == &loopctl::error::LoopError::Cancelled.to_string()),
                "a cancel the correction raised is not an error to report — \
                 the corrected turn is the next story: {errors:?}"
            );
        }
        driver.abort();
    }

    #[tokio::test]
    async fn a_queued_prompt_runs_with_the_text_it_was_edited_to() {
        // Two slow turns: while the first is held back, the second prompt is
        // provably still waiting, so its text can change the way the panel's
        // edit key changes it — while its id is already in flight. The
        // endpoint answers one body per connection and a run opens more than
        // one (the client probes the endpoint before it posts), so the canned
        // bodies outlast the runs: a short queue would answer a probe with a
        // 500 and turn a probe into a retry.
        let server = SseServer::start_with_delay(
            (0..8).map(|_| sse_text_turn("an answer")).collect(),
            std::time::Duration::from_millis(250),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        queue_prompt(&state, &tx, "the prompt already running");
        let edited = queue_prompt(&state, &tx, "the wording first typed");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            state
                .queued
                .lock()
                .expect("queue lock")
                .replace(edited, "the wording it was edited to".to_string()),
            "the edited prompt is still waiting when the edit lands"
        );

        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("both runs settle without hanging")
            .expect("the driver task itself does not panic");

        let requests = server.requests.lock().expect("request lock");
        let posted: Vec<&String> = requests
            .iter()
            .filter(|request| {
                request
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains("/chat/completions"))
            })
            .collect();
        assert_eq!(
            posted.len(),
            2,
            "exactly the two prompts are posted to a provider — a third would \
             mean a run failed and retried rather than ran"
        );
        assert!(
            posted[0].contains("the prompt already running"),
            "the running prompt is posted as submitted"
        );
        assert!(
            posted[1].contains("the wording it was edited to"),
            "the prompt is posted as edited — the claim reads the queue at turn \
             time, not the text that travelled the channel"
        );
        assert!(
            !posted[1].contains("the wording first typed"),
            "the superseded wording is posted to nobody, so the model never \
             reads a text the user replaced"
        );
    }

    #[tokio::test]
    async fn a_corrected_prompt_replaces_the_turn_that_already_went_out() {
        // The case the rewind seam exists for: the prompt is claimed, its
        // turn is in flight, and the user commits an edit to it meanwhile.
        // The fix has to go out, and the superseded turn has to stop
        // standing in the conversation — which the recorded request bodies
        // settle, since every request carries the whole history in front of
        // the prompt it is asking about.
        let server = SseServer::start_with_delay(
            (0..12).map(|_| sse_text_turn("an answer")).collect(),
            std::time::Duration::from_millis(250),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the run and the correction settle without hanging")
            .expect("the driver task itself does not panic");

        let requests = server.requests.lock().expect("request lock");
        let posted: Vec<&String> = requests
            .iter()
            .filter(|request| {
                request
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains("/chat/completions"))
            })
            .collect();
        assert_eq!(
            posted.len(),
            2,
            "the superseded prompt and its correction are the two turns asked \
             of a provider"
        );
        assert!(
            posted[0].contains("the question as first asked"),
            "the prompt goes out as submitted, its own turn unwitnessed by any \
             later edit"
        );
        assert!(
            posted[1].contains("the question as corrected"),
            "the committed edit runs"
        );
        assert!(
            !posted[1].contains("the question as first asked"),
            "and it runs over a conversation the turn it replaces has left: a \
             history still carrying the superseded wording would have the model \
             read a question and its replacement as two asks"
        );
    }

    #[tokio::test]
    async fn a_correction_of_an_older_turn_runs_without_cutting_a_newer_one() {
        // A correction that names a turn which is no longer the newest
        // cannot have that turn cut out: an answer has been built on top of
        // it, and taking it back would take that answer with it. The fix
        // still runs, as a turn of its own — the user's words are never
        // dropped to protect a history that has already been answered.
        let server = SseServer::start_with_delay(
            (0..16).map(|_| sse_text_turn("an answer")).collect(),
            std::time::Duration::from_millis(50),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let first = queue_prompt(&state, &tx, "the first question");
        queue_prompt(&state, &tx, "the second question");

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        // Both turns settle while the channel is still open, so the
        // correction provably arrives after a later prompt has been claimed.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        correct_prompt(&tx, first, "a fix to the first question");
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(30), driver)
            .await
            .expect("the third turn settles without hanging")
            .expect("the driver task itself does not panic");

        let requests = server.requests.lock().expect("request lock");
        let posted: Vec<&String> = requests
            .iter()
            .filter(|request| {
                request
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains("/chat/completions"))
            })
            .collect();
        assert_eq!(
            posted.len(),
            3,
            "the two prompts and the correction each ask a provider once"
        );
        assert!(
            posted[2].contains("a fix to the first question"),
            "the correction runs whatever the history around it looks like"
        );
        assert!(
            posted[2].contains("the first question") && posted[2].contains("the second question"),
            "and nothing is taken out of the conversation to make room for it: \
             a second answer already rests on the older turn"
        );
        assert!(
            state.errors.lock().expect("errors lock").is_empty(),
            "declining the cut is not a failure, so nothing is reported"
        );
    }

    #[tokio::test]
    async fn a_correction_cancelled_before_it_ran_is_promised_its_turn_again() {
        // A correction submitted while its turn is out is waiting in
        // the channel when the cancel lands. The cancel stops the run
        // in flight and nothing else — the correction is received as
        // the very next submission, so the section's showing it and
        // its running next are the same fact rather than a hope.
        //
        // The endpoint holds its answer far past the test, so the run
        // is provably mid-request when the cancel lands — the shape
        // every long-delay cancel test uses — and the correction's
        // own claim is observable the instant the loop receives it,
        // without waiting on a transport of its own.
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
        let running = queue_prompt(&state, &tx, "the prompt that went out");
        correct_prompt(&tx, running, "the fixed wording");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        // The settled moment every long-delay cancel test presses at:
        // the run is provably mid-request on an answer that is not
        // coming inside this test's lifetime.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();

        let mut claimed_the_fix = false;
        for _ in 0..1800 {
            let settled = claimed(&state)
                .iter()
                .any(|sent| sent == "the fixed wording")
                && queued_prompts(&state).is_empty();
            if settled {
                claimed_the_fix = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            claimed_the_fix,
            "the revived correction was claimed by its own turn and left \
             nothing waiting — the section never shows a promise nothing \
             keeps: claimed {:?}, waiting {:?}, errors {:?}, requests {}",
            claimed(&state),
            queued_prompts(&state),
            state.errors.lock().expect("errors lock"),
            server.request_count(),
        );
        {
            let errors = state.errors.lock().expect("errors lock");
            assert_eq!(
                errors.first().map(String::as_str),
                Some(loopctl::error::LoopError::Cancelled.to_string().as_str()),
                "the cancelled run's failure is the first thing the session \
                 learned; the revived turn is still waiting on its own \
                 request, and its story is not the promise's"
            );
        }
        // The revived turn's request has no answer coming inside this
        // test's lifetime; the driver is stopped rather than left to
        // time out — the promise was already observed at its claim.
        driver.abort();
    }

    #[tokio::test]
    async fn a_correction_to_a_turn_already_folded_away_says_the_turn_stood() {
        // The superseded turn can be gone before the correction reaches it.
        // A run whose own turns carry the context past the compaction
        // threshold folds the older history into a summary partway through,
        // and the committed count falls below the length recorded when the
        // prompt was claimed. The cut the correction then asks for has
        // nothing left to remove — which the machine answers as success,
        // truly and uselessly, since the summary may still carry the wording
        // the user meant to withdraw. Silence there reads as "the turn you
        // replaced is gone".
        let dir = tempfile::tempdir().expect("temp workdir");
        // Read in one turn: enough to carry the accumulated history over the
        // compaction threshold at the next turn's check, and small enough
        // that no single turn's request reaches the guard above it — a
        // message too big to fit is a failed run, and the newest message is
        // the one a pass keeps.
        std::fs::write(
            dir.path().join("payload.txt"),
            "a line of payload for the compactor\n".repeat(40),
        )
        .expect("payload file for the compactor");
        // A committed history for the fold to work on: without older messages
        // to remove, compaction has nothing to do and the count never drops.
        let seeds: Vec<loopctl::message::Message> = (0..20)
            .map(|turn| {
                if turn % 2 == 0 {
                    loopctl::message::Message::user(format!("earlier exchange {turn}"))
                } else {
                    loopctl::message::Message::assistant(format!("earlier answer {turn}"))
                }
            })
            .collect();
        let server = SseServer::start_with_delay(
            vec![
                sse_tool_call_turn("Read", &serde_json::json!({"path": "payload.txt"})),
                sse_text_turn("settled"),
                sse_text_turn("the corrected answer"),
            ],
            std::time::Duration::ZERO,
        )
        .await;
        let mut config = sse_config(server.port);
        // A window the opening request fits under its threshold and the
        // read-in payload does not: the guard above the threshold refuses a
        // request outright, so a window smaller than one turn's own size
        // would fail the run rather than compact it.
        config.api.context_window = Some(9_250);
        // The truncating compactor, so the pass is the machine's own work:
        // the summarizers phone the provider for their summaries, and a
        // canned endpoint serving one body per connection cannot tell a run
        // request from a summarizer one — which would fail the pass rather
        // than test what follows it.
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .with_history(seeds)
            .build()
            .await
            .expect("offline runner construction with a tiny window");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
        drop(tx);

        let driver = spawn_driver(runner, rx, state.clone(), shutdown_flag(false));
        tokio::time::timeout(std::time::Duration::from_secs(60), driver)
            .await
            .expect("the run and the correction settle without hanging")
            .expect("the driver task itself does not panic");

        let posted: Vec<String> = server
            .requests
            .lock()
            .expect("request lock")
            .iter()
            .filter(|request| request.starts_with("POST "))
            .cloned()
            .collect();
        let seeds_carried = |request: &str| {
            (0..20)
                .filter(|seed| {
                    request.contains(&format!("earlier exchange {seed}"))
                        || request.contains(&format!("earlier answer {seed}"))
                })
                .count()
        };
        assert!(
            posted
                .first()
                .is_some_and(|request| seeds_carried(request) == 20)
                && posted
                    .last()
                    .is_some_and(|request| seeds_carried(request) < 20),
            "the precondition this test exists for: the older exchanges went out \
             of the history while the run was in flight, which is what leaves the \
             later cut with nothing to remove"
        );
        let errors = state.errors.lock().expect("errors lock");
        assert!(
            errors.iter().any(|error| error.contains("compacted away")),
            "a correction whose turn the history had already shrunk past says so, rather than leaving the superseded wording standing inside a summary the user cannot see: {errors:?}"
        );
        drop(errors);
        let requests = server.requests.lock().expect("request lock");
        assert!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST "))
                .any(|request| request.contains("the question as corrected")),
            "the correction still runs: an unavailable cut is a reason to explain, not a reason to refuse"
        );
    }

    #[tokio::test]
    async fn a_correction_to_a_turn_the_window_refused_says_stopped_not_compacted() {
        // A run the context window refuses outright discards even its
        // own prompt — committing it back would wedge every later run
        // against the same overflow — so the superseded turn leaves
        // the committed history exactly as long as it found it. The
        // correction's cut then has nothing to remove, and the row it
        // earns names the stop rather than a compaction that never
        // ran, while the corrected wording still goes out in the
        // stopped turn's place.
        let server = SseServer::start_timed(vec![(
            sse_text_turn("never reached"),
            std::time::Duration::ZERO,
        )])
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let mut config = sse_config(server.port);
        config.api.context_window = Some(1);
        let runner = dch_loop::Runner::builder(&config, dir.path())
            .build()
            .await
            .expect("offline runner construction");
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
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
        .expect("the refused run and the correction settle without hanging");

        let errors = state.errors.lock().expect("errors lock");
        assert!(
            errors
                .first()
                .is_some_and(|error| error.contains("Context window exceeded")),
            "the precondition this test exists for: the turn died at the window guard, before any \
             provider request: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("stopped before it wrote anything")),
            "the correction names the stopped turn it replaces, not a compaction: {errors:?}"
        );
        assert!(
            !errors.iter().any(|error| error.contains("compacted")),
            "nothing folded — the history never shrank — so no row claims it did: {errors:?}"
        );
        drop(errors);
        assert_eq!(
            claimed(&state),
            ["the question as first asked", "the question as corrected"],
            "the corrected wording is sent in the stopped turn's place"
        );
    }

    #[tokio::test]
    async fn a_correction_after_a_failed_run_stays_silent_on_a_clean_cut() {
        // An ordinary failure salvages the turn's prompt into the
        // committed history — the run's own work is what never lands —
        // so the correction's cut removes exactly that salvaged prompt
        // and the replace is clean. Silence is the truth of it: the
        // compaction wording belongs to a history that shrank below
        // the cut, and a failed run shrinks nothing.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (tx, rx) = mpsc::unbounded_channel();
        let running = queue_prompt(&state, &tx, "the question as first asked");
        correct_prompt(&tx, running, "the question as corrected");
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
        .expect("the failed run and the correction settle without hanging");

        let errors = state.errors.lock().expect("errors lock");
        assert_eq!(
            errors.len(),
            2,
            "two failed runs, two failure rows — the correction between them posts no notice of \
             its own: {errors:?}"
        );
        assert!(
            !errors.iter().any(|error| error.contains("compacted")),
            "a clean cut of the salvaged prompt is not a fold, and must not be reported as one: {errors:?}"
        );
    }
}
