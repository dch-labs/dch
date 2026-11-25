//! The loop-rebuild commands: mode moves, model and provider
//! switches, and manual compaction — each settles the live
//! configuration and rebuilds the driver's loop under it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dch_tui::CompactionFiling;
use dch_tui::SessionUpdate;
use tokio::sync::mpsc;

use super::commands::post_session_notice;
use super::driver::{DriverContext, yield_the_running_row};
use super::permissions_bridge::LoopGuidance;
use super::session::{assemble_runner_builder, filtered_machine_history};
use super::transcript::{MachineSnapshot, TranscriptHandle};

/// Retarget the shared mode cell only while it still holds `was`.
///
/// The panel flips the cell the moment `/mode` is keyed, so a
/// rebuild finishing after a newer request landed must not stamp its
/// own mode over the newer one — the write, the success confirmation
/// and the failure rollback alike, goes through only while the cell
/// still carries this command's mode. A cell that has moved on
/// belongs to the newer request, whose own rebuild will settle it;
/// writing over it would let a queued prompt dispatch under a mode
/// the display no longer shows.
fn settle_mode_cell_if_still(
    cell: &dch_config::SharedPermissionMode,
    was: dch_config::PermissionMode,
    to: dch_config::PermissionMode,
) {
    let mut guard = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *guard == was {
        *guard = to;
    }
}

/// Carry the session's live permission mode into a rebuild's config.
///
/// A rebuild selects onto the user-stated base, whose mode is the
/// boot default — but `/mode` moves the session's mode in the shared
/// cell, not in the base. Without the carry, an accepted switch
/// re-seats the loop's config on the base's stale spelling, and the
/// next failed `/mode` rebuild would roll the cell back to it —
/// enabling side effects under a mode the session had left. The cell
/// is the one truth: whatever it holds when the rebuild is prepared
/// is the mode the new loop's config names too.
pub(crate) fn carry_live_permission_mode(
    config: &mut dch_config::DchConfig,
    cell: &dch_config::SharedPermissionMode,
) {
    config.runner.permission_mode = *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
}

/// `/mode`'s driver half: the running gate re-reads the shared mode
/// cell on every dispatch, so the move updates that cell and rebuilds
/// the runner on the new mode — the conversation and its recorded
/// reads carried whole, everything else as it stood — and the loop's
/// own config names the mode the next dispatch enforces. The panel
/// flips the cell before dispatching — that is the mid-run liveness —
/// so the failure direction is this function's to write: a rebuild
/// that cannot stand restores the cell to the mode still in the
/// loop's config, leaving the loop exactly as it was, mode included,
/// and the notice says so. Both cell writes — the confirmation and
/// the rollback — go through only while the cell still holds this
/// command's mode, so a newer `/mode` keyed during the rebuild is
/// never dragged back by the older one.
pub(crate) async fn move_mode(
    runner: &mut dch_loop::Runner,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    context: &mut DriverContext,
    mode: dch_config::PermissionMode,
) {
    let mut config = context.config.clone();
    config.runner.permission_mode = mode;
    let history = runner.committed_history();
    let read_paths = crate::resume::resumed_read_paths_with_machine(&[], Some(&history));
    let observer = context.state.clone().into_observer().0;
    let new_runner = assemble_runner_builder(
        &config,
        &context.workdir,
        observer,
        &context.state,
        &context.lanes,
        &context.shutting_down,
    )
    .with_history(history)
    .build()
    .await;
    match new_runner {
        Ok(built) => {
            for path in read_paths {
                let _recorded = built.record_resumed_read(&path).await;
            }
            context.lanes.attach(&built);
            settle_mode_cell_if_still(&context.lanes.mode_cell, mode, mode);
            transcript.set_permission_mode(Some(mode));
            *runner = built;
            context.state.adopt_runner_handles(
                runner.cancel_signal(),
                Some(Arc::new(LoopGuidance {
                    steering: runner.steering(),
                })),
            );
            context.config = config;
            post_session_notice(
                session_tx,
                format!("mode is {} — the next turn runs under it", mode.as_str()),
            );
        }
        Err(err) => {
            settle_mode_cell_if_still(
                &context.lanes.mode_cell,
                mode,
                context.config.runner.permission_mode,
            );
            if session_tx
                .send(SessionUpdate::PermissionMode {
                    mode: context.config.runner.permission_mode,
                })
                .is_err()
            {
                tracing::warn!("mode restore landed but the display is gone");
            }
            post_session_notice(
                session_tx,
                format!("the loop kept its mode: the rebuild failed ({err})"),
            );
        }
    }
}

/// Apply one manual compaction pass, between runs.
///
/// The `/compact` seam's driver half: the pass folds (or declines) —
/// a settled pass publishes the folded history to the writer,
/// refreshes the cached copy the exit flush reads, and counts itself
/// into the snapshot's total; a declined one yields the running row
/// the engine's observer contract never closes on its own; a refusal
/// names its cause. Every path lands a notice on the session lane.
pub(crate) async fn apply_manual_compaction(
    runner: &mut dch_loop::Runner,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    context: &mut DriverContext,
    compactions: u32,
) {
    match runner.compact_now().await {
        Ok((true, figures)) => {
            let folded = filtered_machine_history(&Arc::new(runner.committed_history()));
            *context
                .state
                .machine_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&folded));
            context
                .state
                .compaction_publications
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(CompactionFiling {
                    pass: context
                        .state
                        .compaction_passes
                        .load(std::sync::atomic::Ordering::SeqCst),
                    folded: Arc::clone(&folded),
                });
            transcript.send_machine(MachineSnapshot {
                messages: folded,
                saved_at: chrono::Utc::now(),
                compactions: compactions.saturating_add(1),
                saver: None,
            });
            post_session_notice(session_tx, figures);
        }
        Ok((false, figures)) => {
            yield_the_running_row(context);
            post_session_notice(session_tx, figures);
        }
        Err(err) => {
            yield_the_running_row(context);
            post_session_notice(session_tx, format!("compaction was declined ({err})"));
        }
    }
}

/// The cross-provider switch: rebuild the runner on the profile's wire.
///
/// A different provider cannot ride a rename — the wire shape differs
/// — so the switch travels as a rebuild: the committed history carries
/// forward, the observer state is shared (the same buffers the display
/// drains), and every wire the session's build installed rides the same
/// assembler the first build used. An empty api type inherits the
/// running protocol; an empty base URL — the panel's absent marker for
/// an entry that stated only its protocol — inherits the endpoint the
/// user stated in `[api]`, never the live one: the live endpoint may
/// be a previous entry's layer while the credential always comes from
/// the stated base or the selected entry, and a stated key must never
/// travel to another profile's endpoint. A spelled api type must name a
/// protocol the config knows, or the switch is refused whole rather than
/// half-moving the endpoint. The context's config takes the settings
/// the rebuild installed, so later rebuilds build on them.
pub(crate) async fn switch_provider(
    runner: &mut dch_loop::Runner,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    context: &mut DriverContext,
    model: String,
    base_url: String,
    api_type: String,
) {
    let protocol = if api_type.is_empty() {
        Ok(context.config.api.api_type)
    } else {
        serde_json::from_str::<dch_config::ApiType>(&format!("\"{api_type}\""))
    };
    let Ok(protocol) = protocol else {
        post_session_notice(
            session_tx,
            format!("kept the current provider — {model}'s api type is not a known protocol"),
        );
        return;
    };
    let mut config = context.config_base.as_ref().clone();
    config.api.model = model;
    config.api.base_url = if base_url.is_empty() {
        context.config_base.api.base_url.clone()
    } else {
        base_url
    };
    config.api.api_type = protocol;
    let selected = config.api.model.clone();
    if config.select_model(Some(&selected)).is_err() {
        tracing::debug!(model = %selected, "provider switch without a profile entry");
    }
    crate::headless::reapply_reasoning_effort(
        &mut config,
        context.overrides.reasoning_effort.as_deref(),
    );
    carry_live_permission_mode(&mut config, &context.lanes.mode_cell);
    let history = runner.committed_history();
    let read_paths = crate::resume::resumed_read_paths_with_machine(&[], Some(&history));
    let observer = context.state.clone().into_observer().0;
    let new_runner = assemble_runner_builder(
        &config,
        &context.workdir,
        observer,
        &context.state,
        &context.lanes,
        &context.shutting_down,
    )
    .with_history(history)
    .build()
    .await;
    match new_runner {
        Ok(built) => {
            for path in read_paths {
                let _recorded = built.record_resumed_read(&path).await;
            }
            context.lanes.attach(&built);
            let window = built.context_window();
            context.state.context_window.store(window, Ordering::SeqCst);
            *runner = built;
            context.state.adopt_runner_handles(
                runner.cancel_signal(),
                Some(Arc::new(LoopGuidance {
                    steering: runner.steering(),
                })),
            );
            context.config = config.clone();
            transcript.relabel_model(&config.api.model);
            post_session_notice(
                session_tx,
                format!(
                    "switched to {} — context window {window} tokens",
                    config.api.model
                ),
            );
            let update = SessionUpdate::Provider {
                model: config.api.model.clone(),
                base_url: config.api.base_url.clone(),
                api_type: serde_json::to_value(config.api.api_type)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
            };
            if session_tx.send(update).is_err() {
                tracing::warn!("provider switch landed but the display is gone");
            }
        }
        Err(err) => post_session_notice(
            session_tx,
            format!("kept the current provider — the rebuild failed ({err})"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::commands::apply_session_command;
    use crate::tui::driver::DriverContext;
    use crate::tui::driver::RunOverrides;
    use crate::tui::driver::run_driver_loop;
    use crate::tui::permissions_bridge::InteractiveLanes;
    use crate::tui::session::filtered_machine_history;
    use crate::tui::test_support::*;
    use crate::tui::transcript::MachineSnapshot;
    use crate::tui::transcript::TranscriptWorker;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use dch_tui::HealthNotice;
    use dch_tui::SessionCommand;
    use dch_tui::SessionUpdate;
    use dch_tui::Submission;
    use dch_tui::TuiMessage;
    use dch_tui::TuiObserverState;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn a_mode_move_rebuilds_the_loop_under_the_new_mode() {
        // /mode's driver half: the gate re-reads the shared mode cell
        // on every dispatch, so the move updates that cell and
        // rebuilds the runner — the conversation carried — and the
        // loop's own config names the mode the next dispatch enforces.
        let dir = tempfile::tempdir().expect("session tempdir");
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let workdir = tempfile::tempdir().expect("temp workdir");
        let server =
            SseServer::start_with_delay(vec![sse_text_turn("grown")], std::time::Duration::ZERO)
                .await;
        let mut config = sse_config(server.port);
        config.api.context_window = Some(1_000_000);
        let mut runner = dch_loop::Runner::builder(&config, workdir.path())
            .with_history(vec![
                loopctl::message::Message::user("hi"),
                loopctl::message::Message::assistant("hello"),
            ])
            .build()
            .await
            .expect("runner over a tiny history");
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: config.clone(),
            config_base: std::sync::Arc::new(config),
            workdir: workdir.path().to_path_buf(),
            sessions_root: root.clone(),
            state: state.clone(),
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Mode {
                mode: dch_config::PermissionMode::ReadOnly,
            },
            &mut context,
        )
        .await;
        worker.join();
        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the move reports its verdict")
            .expect("the session channel is open");
        if let SessionUpdate::Notice { text } = notice {
            assert!(
                text.contains("mode is read_only"),
                "the notice names the mode the loop took: {text}"
            );
        } else {
            panic!("expected a notice from the mode move");
        }
        assert_eq!(
            context.config.runner.permission_mode,
            dch_config::PermissionMode::ReadOnly,
            "the loop's config carries the mode the next dispatch reads"
        );
    }

    #[tokio::test]
    async fn a_declining_manual_compact_yields_the_running_row() {
        // /compact on a fitting context is the decline path, and the
        // engine's observer contract has no declined event: the running
        // row the pass's opening event raised has no closer but this
        // arm. Pin that the driver yields the lane beside the notice —
        // an orphaned "Compacting" row reads as a pass that never
        // ends, which is what every small session's manual compact
        // showed before the yield.
        let dir = tempfile::tempdir().expect("session tempdir");
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let workdir = tempfile::tempdir().expect("temp workdir");
        let server =
            SseServer::start_with_delay(vec![sse_text_turn("grown")], std::time::Duration::ZERO)
                .await;
        let mut config = sse_config(server.port);
        config.api.context_window = Some(1_000_000);
        let mut runner = dch_loop::Runner::builder(&config, workdir.path())
            .with_history(vec![
                loopctl::message::Message::user("hi"),
                loopctl::message::Message::assistant("hello"),
            ])
            .build()
            .await
            .expect("runner over a tiny history");
        let (_, state) = TuiObserverState::new().into_observer();
        {
            let mut health = state
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *health = HealthNotice::Compacting {
                began: std::time::Instant::now(),
            };
        }
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: config.clone(),
            config_base: std::sync::Arc::new(config),
            workdir: workdir.path().to_path_buf(),
            sessions_root: root.clone(),
            state: state.clone(),
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Compact { compactions: 0 },
            &mut context,
        )
        .await;
        worker.join();
        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the decline reports its verdict")
            .expect("the session channel is open");
        if let SessionUpdate::Notice { text } = notice {
            assert!(
                text.contains("nothing to compact"),
                "the decline notice carries the pass's verdict: {text}"
            );
        } else {
            panic!("expected a notice from the declined pass");
        }
        assert!(
            matches!(
                *state
                    .health
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                HealthNotice::Idle
            ),
            "the running row yields with the decline"
        );
    }

    #[tokio::test]
    async fn a_manual_compact_publishes_the_folded_history_and_refreshes_the_cache() {
        // /compact then quit before another run: without this publish
        // the machine snapshot on disk is the pre-compaction history the
        // last turn-end hook stamped, and a resume restores the very
        // context the pass removed. The driver lands the folded history
        // in the file, refreshes the cached copy the exit flush reads,
        // and records the pass in the snapshot's total so the resume's
        // stale-freeze guard cannot misread the shorter folded history.
        let dir = tempfile::tempdir().expect("session tempdir");
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        transcript.send(
            vec![TuiMessage::User {
                text: "the conversation so far".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );

        let workdir = tempfile::tempdir().expect("temp workdir");
        let seeds: Vec<loopctl::message::Message> = (0..20)
            .map(|turn| {
                let filler = "a line of context for the compactor ".repeat(120);
                if turn % 2 == 0 {
                    loopctl::message::Message::user(format!("earlier exchange {turn}: {filler}"))
                } else {
                    loopctl::message::Message::assistant(format!("earlier answer {turn}: {filler}"))
                }
            })
            .collect();
        // One scripted run first: the truncator folds material older
        // than the last run, so the seeded bulk must sit behind a
        // completed exchange to be droppable — and the seeds must sit
        // between the threshold and the window so the run itself is
        // sent while a pass is still warranted after it.
        let server =
            SseServer::start_with_delay(vec![sse_text_turn("grown")], std::time::Duration::ZERO)
                .await;
        let mut config = sse_config(server.port);
        config.api.context_window = Some(40_000);
        config.runner.compactor = dch_config::CompactorSelection::Truncating;
        let (observer, state) = TuiObserverState::new().into_observer();
        let mut runner = dch_loop::Runner::builder(&config, workdir.path())
            .with_history(seeds)
            .with_observer(Arc::new(observer))
            .build()
            .await
            .expect("runner over a fat history");
        tokio::time::timeout(std::time::Duration::from_secs(20), runner.run("grow"))
            .await
            .expect("the scripted run completes")
            .expect("the run finishes");
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: config.clone(),
            config_base: std::sync::Arc::new(config),
            workdir: workdir.path().to_path_buf(),
            sessions_root: root.clone(),
            state: state.clone(),
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Compact { compactions: 1 },
            &mut context,
        )
        .await;
        let cached = context
            .state
            .machine_history
            .lock()
            .expect("the machine-history lock")
            .as_ref()
            .map(|history| history.len());
        let filed = context
            .state
            .compaction_publications
            .lock()
            .expect("the publications lock")
            .first()
            .map(|filing| (filing.pass, filing.folded.len()));
        assert_eq!(
            filed,
            cached.map(|len| (1, len)),
            "the manual pass files its folded feed under its own pass's \
             stamp — the session's first compaction — beside the cache \
             the same path refreshed"
        );
        assert_eq!(
            context
                .state
                .compactions
                .lock()
                .expect("the compaction lock")
                .last()
                .map(|event| event.pass),
            Some(1),
            "the observer numbered the pass's row one — the stamp the filing carries"
        );
        worker.join();

        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the pass reports its figures")
            .expect("the session channel is open");
        let text = match notice {
            SessionUpdate::Notice { text } => text,
            _ => String::new(),
        };
        assert!(
            text.starts_with("compacted: "),
            "the notice carries the pass's own figures: {text}"
        );

        let cached = state.machine_history.lock().expect("history lock").clone();
        assert!(
            cached.as_ref().is_some_and(|history| history.len() < 20),
            "the cached machine history is the folded one: {:?}",
            cached.as_ref().map(|history| history.len())
        );
        let outcome = crate::resume::load_for_resume_in(id, &root)
            .expect("the session pair loads after the pass");
        let folded = outcome.machine.expect("the machine half landed");
        assert!(
            folded.len() < 20,
            "the snapshot on disk is the folded history, {} messages",
            folded.len()
        );
        assert_eq!(
            outcome.machine_compactions, 2,
            "the recorded total adds this pass to the rows the display carried"
        );
    }

    #[tokio::test]
    async fn a_no_op_compact_publishes_nothing_and_counts_nothing() {
        // A fitting context declines to fold: the machine snapshot the
        // last turn-end hook stamped stands (no machine half lands
        // beside the display), and the recorded total gains no pass —
        // the resume guard's legitimate-shrink exemption must be earned
        // by folds that actually ran.
        let dir = tempfile::tempdir().expect("session tempdir");
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        transcript.send(
            vec![TuiMessage::User {
                text: "the conversation so far".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        let workdir = tempfile::tempdir().expect("temp workdir");
        let mut runner = dch_loop::Runner::builder(&unreachable_config(), workdir.path())
            .build()
            .await
            .expect("an empty runner constructs");
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: workdir.path().to_path_buf(),
            sessions_root: root.clone(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Compact { compactions: 4 },
            &mut context,
        )
        .await;
        worker.join();

        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the declined pass still reports")
            .expect("the session channel is open");
        let text = match notice {
            SessionUpdate::Notice { text } => text,
            _ => String::new(),
        };
        assert!(
            text.contains("nothing to compact"),
            "the notice names the decline: {text}"
        );
        let session_at = crate::session::load_with_meta_in(id, &root)
            .unwrap()
            .saved_at;
        assert!(
            crate::session::load_machine_in(id, &root, session_at).is_none(),
            "a declined pass republishes nothing"
        );
    }

    #[tokio::test]
    async fn a_model_command_at_rest_rebuilds_the_loop_and_publishes_the_name() {
        // The session lane's model leg, end to end: a `/model` the app
        // already resolved to a name arrives as a command, the driver
        // selects it onto the user-stated base, rebuilds the loop on it
        // (conversation carried, no turn), and the applied name is
        // published back so the status bar and the next request agree.
        // An offline runner is enough — the rebuild itself never
        // touches the network.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Model {
                    name: "gpt-4o-mini".to_string(),
                },
            })
            .expect("driver channel open");
        // Dropping the submit side lets the driver carry out the command,
        // then see an empty channel and leave its loop.
        drop(submit_tx);

        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: dch_config::DchConfig::default(),
                config_base: std::sync::Arc::new(dch_config::DchConfig::default()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a model update is published")
            .expect("the session channel is open when the command lands");
        let update = match update {
            SessionUpdate::Notice { .. } => {
                tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                    .await
                    .expect("the verdict follows the receipt")
                    .expect("the session channel is open")
            }
            verdict => verdict,
        };
        assert_eq!(
            update,
            SessionUpdate::ModelName {
                name: "gpt-4o-mini".to_string()
            },
            "the applied switch is published by its own name"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_failed_rebuild_names_the_model_the_loop_kept() {
        // A switch whose rebuild cannot be built names the model still
        // running — the loop's own, never the requested one — so the two
        // failure notices (a declined selection, a failed build) agree
        // about what "kept" means. The build is made to fail offline: a
        // blank fallback name is refused by the fallback breaker at
        // construction, no network involved.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = unreachable_config();
        context.models = vec![dch_config::ModelProfile {
            model: "wanted-model".to_string(),
            ..dch_config::ModelProfile::default()
        }];
        context.api.model = "kept-model".to_string();
        // The blank fallback is what the rebuild refuses; the loop's own
        // construction predates it and stands.
        context.api.fallback_model = Some(String::new());
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Model {
                    name: "wanted-model".to_string(),
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(context.clone()),
                config: context,
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let mut notice = None;
        for _ in 0..3 {
            let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the driver answers the command")
                .expect("the session channel is open when the command lands");
            if let SessionUpdate::Notice { text } = update {
                notice = Some(text);
                break;
            }
        }
        let notice = notice.unwrap_or_else(|| {
            panic!("a failed rebuild posts a notice, not a silent drop");
        });
        assert!(
            notice.contains("the loop kept kept-model"),
            "the notice names the model still running: {notice}"
        );
        assert!(
            !notice.contains("wanted-model"),
            "the requested model is not the one the loop kept: {notice}"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_failed_rebuild_leaves_the_enforced_mode_untouched() {
        // The `/mode` twin of the model pin: a switch whose rebuild
        // cannot be built posts the kept-mode notice — and, because the
        // shared cell moves only after the rebuild stands, the gate the
        // old runner reads still enforces the boot mode. Before the
        // ordering fix the cell moved first: the notice said "kept"
        // while the very next dispatch ran under the requested mode.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = unreachable_config();
        context.api.fallback_model = Some(String::new());
        let lanes = InteractiveLanes::detached();
        let mode_cell = Arc::clone(&lanes.mode_cell);
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Mode {
                    mode: dch_config::PermissionMode::ReadOnly,
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(context.clone()),
                config: context,
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes,
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let mut notice = None;
        for _ in 0..3 {
            let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the driver answers the command")
                .expect("the session channel is open when the command lands");
            if let SessionUpdate::Notice { text } = update {
                notice = Some(text);
                break;
            }
        }
        let notice = notice.unwrap_or_else(|| {
            panic!("a failed rebuild posts a notice, not a silent drop");
        });
        assert!(
            notice.contains("the loop kept its mode"),
            "the notice names the mode still enforced: {notice}"
        );
        assert_eq!(
            *mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::default(),
            "the enforced mode is still the boot mode — the cell moves only after the rebuild stands"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_failed_mode_rebuild_restores_a_preflipped_cell_to_the_boot_mode() {
        // The panel half of the same contract: `/mode` flips the shared
        // cell before the command dispatches — that is the mid-run
        // liveness — so a rebuild that cannot stand must hand the old
        // mode back. The cell arrives armed with the requested mode and
        // must leave holding the boot mode; the no-app pin above cannot
        // see this shape, because nothing there pre-flips the cell it
        // watches.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = unreachable_config();
        context.api.fallback_model = Some(String::new());
        let lanes = InteractiveLanes::detached();
        let mode_cell = Arc::clone(&lanes.mode_cell);
        *mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            dch_config::PermissionMode::ReadOnly;
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Mode {
                    mode: dch_config::PermissionMode::ReadOnly,
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(context.clone()),
                config: context,
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes,
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let mut notice = None;
        let mut restored = None;
        for _ in 0..6 {
            let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the driver answers the command")
                .expect("the session channel is open when the command lands");
            match update {
                SessionUpdate::Notice { text } => notice = Some(text),
                SessionUpdate::PermissionMode { mode } => restored = Some(mode),
                _ => {}
            }
            if notice.is_some() && restored.is_some() {
                break;
            }
        }
        let notice =
            notice.unwrap_or_else(|| panic!("a failed rebuild posts a notice, not a silent drop"));
        assert!(
            notice.contains("the loop kept its mode"),
            "the notice names the mode still enforced: {notice}"
        );
        assert_eq!(
            *mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::default(),
            "the pre-flipped cell is restored to the boot mode the loop still runs under"
        );
        assert_eq!(
            restored,
            Some(dch_config::PermissionMode::default()),
            "the standing mode is published back — the panel's display \
             copy rolls to what the loop enforces"
        );
        let _ = driver.await;
    }

    #[test]
    fn the_mode_cell_settle_writes_only_while_the_command_still_owns_it() {
        // Both arms of the conditional in one pin: the settle lands
        // while the cell holds the mode the command requested, and is
        // a no-op the moment anything newer has moved the cell — the
        // newer request's own rebuild is the one that settles it.
        let cell: dch_config::SharedPermissionMode =
            std::sync::Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::Auto));
        settle_mode_cell_if_still(
            &cell,
            dch_config::PermissionMode::Auto,
            dch_config::PermissionMode::ReadOnly,
        );
        assert_eq!(
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::ReadOnly,
            "a still-owned cell settles onto the target mode"
        );
        settle_mode_cell_if_still(
            &cell,
            dch_config::PermissionMode::Auto,
            dch_config::PermissionMode::Interactive,
        );
        assert_eq!(
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::ReadOnly,
            "a moved-on cell is left to its newer owner"
        );
    }

    #[test]
    fn a_rebuilds_config_carries_the_live_mode_not_the_bases() {
        // The whole carry in one pin: the base names Auto, the
        // session's cell holds the ReadOnly a `/mode` moved it to, and
        // the config a rebuild prepares names ReadOnly — so the loop's
        // own record, and any later failed `/mode` rollback, restore
        // what the gate actually enforces, never the base's stale
        // spelling.
        let mut config = unreachable_config();
        config.runner.permission_mode = dch_config::PermissionMode::Auto;
        let cell: dch_config::SharedPermissionMode =
            std::sync::Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::ReadOnly));
        carry_live_permission_mode(&mut config, &cell);
        assert_eq!(
            config.runner.permission_mode,
            dch_config::PermissionMode::ReadOnly,
            "the rebuild's config names the cell's live mode"
        );
    }

    #[tokio::test]
    async fn an_older_failed_mode_rebuild_leaves_a_newer_flip_alone() {
        // The race the conditional settle closes, in the interleaving
        // that produced it: `/mode ReadOnly` is keyed and its rebuild
        // starts, `/mode Interactive` is keyed while that rebuild
        // still stands — the panel's second flip is the cell's newest
        // word. The older rebuild's rollback must leave the newer flip
        // where it is (the old unconditional write dragged the cell
        // back mid-window, and a queued prompt could dispatch under
        // the reverted mode); the newer command's own rebuild is the
        // one that settles the cell.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = unreachable_config();
        context.api.fallback_model = Some(String::new());
        let lanes = InteractiveLanes::detached();
        let mode_cell = Arc::clone(&lanes.mode_cell);
        // The panel keyed ReadOnly first, Interactive second: the cell
        // holds the newest flip before the driver drains the first
        // command.
        *mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            dch_config::PermissionMode::Interactive;
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Mode {
                    mode: dch_config::PermissionMode::ReadOnly,
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(context.clone()),
                config: context,
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes,
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let mut notice = None;
        for _ in 0..6 {
            let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the driver answers the command")
                .expect("the session channel is open when the command lands");
            if let SessionUpdate::Notice { text } = update {
                notice = Some(text);
                break;
            }
        }
        let notice =
            notice.unwrap_or_else(|| panic!("a failed rebuild posts a notice, not a silent drop"));
        assert!(
            notice.contains("the loop kept its mode"),
            "the failed rebuild names the mode it kept: {notice}"
        );
        assert_eq!(
            *mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Interactive,
            "the newer flip survives the older rebuild's rollback — \
             the cell belongs to the newest request"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_model_switch_selects_onto_the_base_not_the_accreted_live_config() {
        // The driver's half of the no-accretion rule, read off the
        // switch's own receipt: the boot selection laid the seed entry's
        // 200k window onto the live config, and a switch to an entry
        // stating no window must rebuild on the base's own 64k — the
        // receipt names the figure the engine actually measures, never
        // the previous selection's leftover.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut base = unreachable_config();
        base.api.context_window = Some(64_000);
        base.models = vec![
            dch_config::ModelProfile {
                model: "seed".to_string(),
                context_window: Some(200_000),
                ..dch_config::ModelProfile::default()
            },
            dch_config::ModelProfile {
                model: "plain".to_string(),
                ..dch_config::ModelProfile::default()
            },
        ];
        base.api.model = "seed".to_string();
        let mut live = base.clone();
        live.select_model(None)
            .expect("the boot selection lays the seed entry");
        assert_eq!(
            live.api.context_window,
            Some(200_000),
            "fixture premise: the live config carries the seed's window after boot"
        );
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Model {
                    name: "plain".to_string(),
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(base),
                config: live,
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let mut receipt = None;
        for _ in 0..3 {
            let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the driver answers the command")
                .expect("the session channel is open when the command lands");
            if let SessionUpdate::Notice { text } = &update
                && text.contains("switched to plain")
            {
                receipt = Some(text.clone());
                break;
            }
        }
        let receipt = receipt.unwrap_or_else(|| panic!("the switch posts its receipt"));
        assert!(
            receipt.contains("context window 64000 tokens"),
            "the receipt names the base's own window — the seed entry's 200_000 did not \
             accrete: {receipt}"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_model_command_rebuilds_on_the_profile_and_writes_the_file() {
        // A `/model` is a rebuild, not a rename, and this is the whole
        // contract read off disk and shared state: the engine comes
        // back built on the selected entry — its context window is the
        // new model's own figure, the one the bar and the compaction
        // threshold read — and session.json is rewritten at once under
        // the new name, verified by loading the file the writer left.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "first-model".to_string(),
            dir.path().to_path_buf(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let hook_transcript = transcript.clone();

        let config = dch_config::DchConfig {
            models: vec![dch_config::ModelProfile {
                model: "wide-model".to_string(),
                context_window: Some(123_456),
                ..dch_config::ModelProfile::default()
            }],
            ..dch_config::DchConfig::default()
        };

        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Model {
                    name: "wide-model".to_string(),
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(config.clone()),
                config,
                workdir: std::path::PathBuf::from("."),
                sessions_root: dir.path().to_path_buf(),
                state: state.clone(),
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            transcript.clone(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a model update is published")
            .expect("the session channel is open when the command lands");
        // The switch posts its receipt first (the notice naming what
        // landed and at what window), then the verdict.
        let update = match update {
            SessionUpdate::Notice { text } => {
                assert!(
                    text.contains("switched to wide-model") && text.contains("123456"),
                    "the receipt names the model and the engine's window: {text}"
                );
                tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                    .await
                    .expect("the verdict follows the receipt")
                    .expect("the session channel is open")
            }
            verdict => verdict,
        };
        assert_eq!(
            update,
            SessionUpdate::ModelName {
                name: "wide-model".to_string()
            },
            "the applied switch is published by its own name"
        );
        let _ = driver.await;
        assert_eq!(
            state.context_window.load(Ordering::SeqCst),
            123_456,
            "the rebuilt engine carries the selected profile's window — the figure the bar shows and the compaction threshold reads"
        );
        // The display half of the loop: the same wiring run_tui_session
        // installs — the save hook on the app, the session channel in —
        // and the verdict applied the way the run loop applies it. The
        // publish the verdict carries is what writes the file; nothing
        // else in this flow saves.
        let mut app = dch_tui::TuiApp::from_observer_state(
            dch_config::DchConfig {
                models: vec![dch_config::ModelProfile {
                    model: "wide-model".to_string(),
                    context_window: Some(123_456),
                    ..dch_config::ModelProfile::default()
                }],
                ..dch_config::DchConfig::default()
            },
            state.clone(),
        );
        let (_display_tx, display_rx) = mpsc::unbounded_channel();
        app.set_session_requests(display_rx);
        app.set_turn_end_hook(Box::new(move |conversation, machine| {
            hook_transcript.send(
                conversation.to_vec(),
                crate::session::SessionTokens::default(),
            );
            if let Some(machine) = machine {
                hook_transcript.send_machine(MachineSnapshot {
                    messages: filtered_machine_history(&Arc::new(machine.to_vec())),
                    saved_at: chrono::Utc::now(),
                    compactions: 0,
                    saver: None,
                });
            }
        }));
        app.apply_session_update(update);
        worker.join();
        let model = crate::session::load_with_meta_in(id, dir.path())
            .expect("the file the writer left")
            .model;
        assert_eq!(
            model, "wide-model",
            "session.json names the switched model, read off disk"
        );
    }

    #[tokio::test]
    async fn typing_a_model_switch_rewrites_the_file_and_the_window() {
        // The full production path, no shortcuts: the app types the
        // command through its own key handling — parse, the profile
        // match, the send — the driver rebuilds the engine on the
        // selected entry and relabels the saver, the verdict travels
        // back and the display publishes the save, the writer drains,
        // and the test reads session.json off disk and the shared
        // window. This is the path a user's `/model` actually takes.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "first-model".to_string(),
            dir.path().to_path_buf(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let hook_transcript = transcript.clone();

        let config = dch_config::DchConfig {
            models: vec![dch_config::ModelProfile {
                model: "wide-model".to_string(),
                context_window: Some(123_456),
                ..dch_config::ModelProfile::default()
            }],
            ..dch_config::DchConfig::default()
        };

        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();

        let mut app = dch_tui::TuiApp::from_observer_state(config, state.clone());
        app.set_submit_tx(submit_tx);
        let (_drain_tx, session_rx_for_app) = mpsc::unbounded_channel();
        app.set_session_requests(session_rx_for_app);
        app.set_turn_end_hook(Box::new(move |conversation, machine| {
            hook_transcript.send(
                conversation.to_vec(),
                crate::session::SessionTokens::default(),
            );
            if let Some(machine) = machine {
                hook_transcript.send_machine(MachineSnapshot {
                    messages: filtered_machine_history(&Arc::new(machine.to_vec())),
                    saved_at: chrono::Utc::now(),
                    compactions: 0,
                    saver: None,
                });
            }
        }));

        let mut switch_config = unreachable_config();
        switch_config.models = vec![dch_config::ModelProfile {
            model: "wide-model".to_string(),
            context_window: Some(123_456),
            ..dch_config::ModelProfile::default()
        }];
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config_base: std::sync::Arc::new(switch_config.clone()),
                config: switch_config,
                workdir: std::path::PathBuf::from("."),
                sessions_root: dir.path().to_path_buf(),
                state: state.clone(),
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            transcript,
            session_tx,
        )));

        // Type the command the way a user does: key by key, then Enter.
        for glyph in "/model wide-model".chars() {
            app.handle_event(&Event::Key(KeyEvent::new(
                KeyCode::Char(glyph),
                KeyModifiers::NONE,
            )));
        }
        app.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        // The driver's receipt comes first (the notice), then the
        // verdict; apply the verdict the way the run loop does.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the switch lands")
            .expect("the session channel is open");
        let verdict = match first {
            SessionUpdate::Notice { .. } => {
                tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                    .await
                    .expect("the verdict follows the receipt")
                    .expect("the session channel is open")
            }
            verdict => verdict,
        };
        app.apply_session_update(verdict);
        drop(app);
        driver.abort();
        assert_eq!(
            state.context_window.load(Ordering::SeqCst),
            123_456,
            "the shared window carries the selected profile's figure — what the bar and the compaction threshold read"
        );
        worker.join();
        let model = crate::session::load_with_meta_in(id, dir.path())
            .expect("the file the writer left")
            .model;
        assert_eq!(
            model, "wide-model",
            "session.json names the switched model, read off disk"
        );
    }

    #[tokio::test]
    async fn a_declined_model_switch_publishes_a_notice() {
        // The `Model` leg's failure path: a name the live loop refuses must
        // not publish a `ModelName` — the status bar would claim a model the
        // loop never took — and the refusal is said aloud on the notice lane
        // instead of dying in a log. A blank name is refused at the runner.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Model {
                    name: String::new(),
                },
            })
            .expect("driver channel open");
        drop(submit_tx);

        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: dch_config::DchConfig::default(),
                config_base: std::sync::Arc::new(dch_config::DchConfig::default()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a refusal is published")
            .expect("the session channel is open when the refusal lands");
        let SessionUpdate::Notice { text } = update else {
            panic!("a refused switch posts a notice, got {update:?}");
        };
        assert!(
            text.contains("kept"),
            "the notice names the model that stands: {text}"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_mode_move_records_the_override_and_a_rotation_drops_it() {
        // `/mode`'s remembering half: the move lands the override on
        // the save target, so the next crash-atomic save writes it —
        // and a `/clear`'s rotation drops it, because a fresh session
        // has no override of its own and runs the configured default.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        handle.set_permission_mode(Some(dch_config::PermissionMode::Interactive));
        assert_eq!(
            handle
                .saver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .permission_mode,
            Some(dch_config::PermissionMode::Interactive),
            "the move records the session's standing override"
        );

        let fresh = handle.rotate("m");
        assert_ne!(fresh, id.to_string(), "the rotation mints a fresh id");
        assert_eq!(
            handle
                .saver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .permission_mode,
            None,
            "a fresh session carries no override — the configured default governs"
        );
        worker.join();
    }

    #[tokio::test]
    async fn a_model_switch_restores_the_flag_effort_over_the_entrys_dial() {
        // The flag is a per-run statement: the entry states its own dial,
        // the switch selects it, and the restore puts the flag's value
        // back on top — the precedence the boot order gives it, kept
        // past the session's first rebuild instead of ending there.
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut base = unreachable_config();
        base.models = vec![dch_config::ModelProfile {
            model: "dialled".to_string(),
            reasoning_effort: Some("high".to_string()),
            ..dch_config::ModelProfile::default()
        }];
        let mut live = base.clone();
        live.select_model(None)
            .expect("the boot selection lays the entry's dial");
        assert_eq!(
            live.api.reasoning_effort.as_deref(),
            Some("high"),
            "fixture premise: the entry's dial lands without a flag to restore"
        );
        live.api.reasoning_effort = Some("low".to_string()); // the flag's boot restore
        let mut context = DriverContext {
            config: live,
            config_base: std::sync::Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: crate::session::sessions_dir(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides {
                model: None,
                reasoning_effort: Some("low".to_string()),
                permission_named: false,
                ..RunOverrides::default()
            },
        };
        let transcript = parked_transcript();
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Model {
                name: "dialled".to_string(),
            },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.reasoning_effort.as_deref(),
            Some("low"),
            "the flag's dial survives the switch onto the entry"
        );
    }

    #[tokio::test]
    async fn a_provider_switch_restores_the_flag_effort_over_the_entrys_dial() {
        // The same law on the cross-provider wire: an entry naming the
        // switched-to model contributes its own dial through the
        // selection, and the run's flag goes back on top before the
        // rebuild reads it.
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut base = unreachable_config();
        base.models = vec![dch_config::ModelProfile {
            model: "elsewhere".to_string(),
            reasoning_effort: Some("high".to_string()),
            ..dch_config::ModelProfile::default()
        }];
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: crate::session::sessions_dir(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides {
                model: None,
                reasoning_effort: Some("low".to_string()),
                permission_named: false,
                ..RunOverrides::default()
            },
        };
        let transcript = parked_transcript();
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::SwitchProvider {
                model: "elsewhere".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                api_type: "openai".to_string(),
            },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.reasoning_effort.as_deref(),
            Some("low"),
            "the flag's dial survives the provider switch onto the entry"
        );
    }

    #[tokio::test]
    async fn a_provider_switch_without_a_base_url_inherits_the_stated_endpoint() {
        // The panel's absent marker: an entry that stated only its
        // protocol sends an empty base_url, and the switch inherits the
        // endpoint the user stated in [api] — never the live one, which
        // may be a previous entry's layer — rather than rebuilding the
        // client against an empty URL.
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut base = unreachable_config();
        base.models = vec![dch_config::ModelProfile {
            model: "elsewhere".to_string(),
            ..dch_config::ModelProfile::default()
        }];
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: crate::session::sessions_dir(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        context.config.api.base_url = "http://127.0.0.1:9/v1".to_string();
        let transcript = parked_transcript();
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::SwitchProvider {
                model: "elsewhere".to_string(),
                base_url: String::new(),
                api_type: "openai".to_string(),
            },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.base_url, "http://127.0.0.1:1",
            "the absent marker inherits the stated endpoint, never the live one"
        );
    }

    #[tokio::test]
    async fn a_stated_key_never_rides_to_a_previous_profile_s_endpoint() {
        // The credential-to-endpoint rule, end to end: a first switch
        // lands another profile's endpoint and key, and a second switch
        // to an entry that stated only its protocol pairs the stated
        // base key with the stated base endpoint — the previous
        // profile's endpoint and key both stay behind.
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut base = unreachable_config();
        base.models = vec![
            dch_config::ModelProfile {
                model: "other".to_string(),
                base_url: Some("http://other/v1".to_string()),
                api_key: Some("other-key".to_string()),
                ..dch_config::ModelProfile::default()
            },
            dch_config::ModelProfile {
                model: "elsewhere".to_string(),
                api_type: Some(dch_config::ApiType::Anthropic),
                ..dch_config::ModelProfile::default()
            },
        ];
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: crate::session::sessions_dir(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        let transcript = parked_transcript();
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::SwitchProvider {
                model: "other".to_string(),
                base_url: "http://other/v1".to_string(),
                api_type: "openai".to_string(),
            },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.base_url, "http://other/v1",
            "fixture premise: the first switch landed the other profile's endpoint"
        );
        assert_eq!(
            context.config.api.api_key.as_deref(),
            Some("other-key"),
            "fixture premise: and its key"
        );
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::SwitchProvider {
                model: "elsewhere".to_string(),
                base_url: String::new(),
                api_type: "anthropic".to_string(),
            },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.base_url, "http://127.0.0.1:1",
            "the stated key travels only to the stated endpoint — the previous profile's \
             endpoint stays behind"
        );
        assert_eq!(
            context.config.api.api_key.as_deref(),
            Some("dummy"),
            "and the previous profile's key stays behind with it"
        );
    }

    #[tokio::test]
    async fn a_provider_switch_with_an_unknown_protocol_is_refused_whole() {
        // The api type names the wire shape: one the config's vocabulary
        // rejects is refused before any rebuild, so the endpoint is never
        // half-moved and the loop keeps the conversation it holds.
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::SwitchProvider {
                    model: "elsewhere-model".to_string(),
                    base_url: "http://127.0.0.1:1".to_string(),
                    api_type: "not-a-protocol".to_string(),
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: dch_config::DchConfig::default(),
                config_base: std::sync::Arc::new(dch_config::DchConfig::default()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            parked_transcript(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a notice is published")
            .expect("the session channel is open when the refusal lands");
        let SessionUpdate::Notice { text } = update else {
            panic!("an unknown protocol posts a notice, got {update:?}");
        };
        assert!(
            text.contains("not a known protocol"),
            "the refusal names the half-move it prevented: {text}"
        );
        let _ = driver.await;
    }
}
