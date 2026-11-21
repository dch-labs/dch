//! Session command dispatch and the identity moves: applying a
//! command against the driver's loop at rest, then the moves
//! that swap the session itself — clears, resumes, and the
//! rewind a refused move earns.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dch_tui::SessionCommand;
use dch_tui::SessionUpdate;
use tokio::sync::mpsc;

use super::driver::DriverContext;
use super::permissions_bridge::{
    LoopGuidance, fail_permissions_restore_closed, reseed_config_rules_cell,
    tighten_config_rules_cell,
};
use super::rebuilds::{
    apply_manual_compaction, carry_live_permission_mode, move_mode, switch_provider,
};
use super::session::{assemble_runner_builder, resume_into_session};
use super::transcript::TranscriptHandle;

/// Carry out one session command on the driver's loop, at rest.
///
/// Every command's effect lands on the runner or the session state the
/// driver owns. `Model` rebuilds the loop on the selected entry — a
/// switch is not a rename, because every setting the entry selects
/// (the context window above all) belongs to the engine; `Clear` rewinds
/// the committed history to empty and moves the save target to a fresh
/// session; a cross-provider `SwitchProvider` and a `Resume` rebuild the
/// runner on new settings or a restored history, carrying the
/// conversation. The context's config is the session's live truth — an
/// accepted switch updates it here, always by selecting onto the
/// user-stated base so no switch accretes the previous one's leftovers,
/// and every later rebuild builds on what the loop actually runs — and
/// an outcome the display cannot see for itself travels back over the
/// session channel: the applied model, the fresh or adopted id, the
/// loaded transcript, or a notice saying a refusal or failure kept
/// things as they were.
pub(crate) async fn apply_session_command(
    runner: &mut dch_loop::Runner,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    command: SessionCommand,
    context: &mut DriverContext,
) -> bool {
    match command {
        SessionCommand::Model { name } => {
            let mut config = context.config_base.as_ref().clone();
            if let Err(err) = config.select_model(Some(&name)) {
                post_session_notice(
                    session_tx,
                    format!(
                        "the loop kept {}: the switch was declined ({err})",
                        context.config.api.model
                    ),
                );
                return false;
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
                    context.config = config;
                    transcript.relabel_model(&name);
                    post_session_notice(
                        session_tx,
                        format!("switched to {name} — context window {window} tokens"),
                    );
                    if session_tx.send(SessionUpdate::ModelName { name }).is_err() {
                        tracing::warn!("model switch landed but the display is gone");
                    }
                }
                Err(err) => post_session_notice(
                    session_tx,
                    format!(
                        "the loop kept {}: the switch could not be built ({err})",
                        context.config.api.model
                    ),
                ),
            }
            false
        }
        SessionCommand::Mode { mode } => {
            Box::pin(move_mode(runner, transcript, session_tx, context, mode)).await;
            false
        }
        SessionCommand::Clear => {
            land_session_clear(runner.rewind_history_to(0), transcript, session_tx, context)
        }
        SessionCommand::SwitchProvider {
            model,
            base_url,
            api_type,
        } => {
            Box::pin(switch_provider(
                runner, transcript, session_tx, context, model, base_url, api_type,
            ))
            .await;
            false
        }
        SessionCommand::Resume { id } => {
            Box::pin(resume_into_session(
                runner, transcript, session_tx, context, id,
            ))
            .await
        }
        SessionCommand::ListSessions => {
            match crate::session::list_sessions_in(&context.sessions_root) {
                Ok(summaries) => {
                    let rows: Vec<dch_tui::queue::SessionPick> = summaries
                        .into_iter()
                        .map(|summary| dch_tui::queue::SessionPick {
                            id: summary.id.to_string(),
                            model: summary.model,
                            saved_at: summary.last_activity,
                            prompts: summary.user_message_count,
                            title: summary.title,
                            workdir: summary.workdir,
                        })
                        .collect();
                    if session_tx
                        .send(SessionUpdate::SessionCatalog { rows })
                        .is_err()
                    {
                        tracing::warn!("session catalog listed but the display is gone");
                    }
                }
                Err(err) => post_session_notice(
                    session_tx,
                    format!("the saved sessions could not be read ({err})"),
                ),
            }
            false
        }
        SessionCommand::Compact { compactions } => {
            apply_manual_compaction(runner, transcript, session_tx, context, compactions).await;
            false
        }
    }
}

/// Post one driver notice, or drop it with a warn when the display is gone.
///
/// The notice lane's single throat: every refusal, failure, and result
/// the driver owes the user reads the same way, and a display that has
/// already left is a warn rather than an error.
pub(crate) fn post_session_notice(session_tx: &mpsc::UnboundedSender<SessionUpdate>, text: String) {
    if session_tx.send(SessionUpdate::Notice { text }).is_err() {
        tracing::warn!("a session notice was lost with the display gone");
    }
}

/// Adopt a resumed session's remembered permissions, failing damage
/// closed.
///
/// The load's verdict drives everything here: a fresh or restored
/// sibling adopts its lists wholesale, and a damaged one adopts
/// nothing *and* moves both levers of the fallback — the shared cell
/// to the asking posture where the session would run `auto`, and the
/// config-rules cell onto its tightening half, because a configured
/// allow would otherwise cover the ask. A healthy verdict moves the
/// same cell the other way: the adoption replaces the memory
/// wholesale — verdict included — so the cell re-seeds from the
/// configuration's own join, and a damage posture an earlier resume
/// armed cannot outlive the verdict that keyed it. Returns the mode
/// the resumed display should publish beside the damage verdict, so
/// the caller can retain the enforced mode and say the damage aloud:
/// with damage standing, that is the mode the cell enforces after
/// the fallback — the asking posture when the fallback moved an
/// auto cell, the cell's own held mode when it already asks — never
/// a standing mode the gate no longer runs.
pub(crate) fn adopt_resumed_permissions(
    transcript: &TranscriptHandle,
    context: &mut DriverContext,
    standing_mode: dch_config::PermissionMode,
) -> (dch_config::PermissionMode, bool) {
    let restored = transcript
        .saver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .load_permissions();
    let damage_stands = restored.damage_stands();
    let (allow, deny) = restored.into_lists();
    let _adoption = context
        .lanes
        .adoption_guard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    context.lanes.memory.adopt(allow, deny, damage_stands);
    let enforced_mode = if !damage_stands {
        standing_mode
    } else if fail_permissions_restore_closed(&context.lanes.mode_cell) {
        dch_config::PermissionMode::Interactive
    } else {
        *context
            .lanes
            .mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    if damage_stands {
        tighten_config_rules_cell(&context.lanes.config_rules_cell);
    } else {
        let selection = context
            .lanes
            .preset_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reseed_config_rules_cell(
            &context.lanes.config_rules_cell,
            &context.config,
            selection.as_deref(),
        );
    }
    (enforced_mode, damage_stands)
}

/// Whether a `/clear` may rotate onto a fresh session, given the
/// rewind's verdict.
///
/// The one gate the clear path turns on: only a rewind that emptied
/// the loop's history earns the fresh identity, so a refused rewind
/// leaves the ended session standing — same id, same file — and the
/// caller says so to the display instead of publishing a swap that
/// never happened.
fn rewind_earned_fresh_session<E>(rewind: &Result<usize, E>) -> bool {
    rewind.is_ok()
}

/// Apply one `/clear` verdict: rotate and publish on an emptied
/// history, say the refusal aloud on an error.
///
/// The caller-side half of [`rewind_earned_fresh_session`]'s gate, kept
/// as its own step so the wiring is testable beside the verdict: an
/// emptied history rotates the save target and publishes the fresh id;
/// a refused rewind rotates nothing, publishes nothing, and names the
/// failure on the notice lane so the ended session — same id, same
/// file — visibly stands. A rewind that merely had nothing to remove
/// still emptied the history and still earns the rotation. The
/// rotation is also the damage posture's end: the fresh adoption
/// clears the verdict, and the config-rules cell re-seeds from the
/// configuration's own join beside it, so the ended session's
/// fail-closed posture cannot withdraw the config's allows from the
/// fresh one. The rotation also retires the launch permission
/// statement: the flag named the ended session's mode, so a later
/// resume adopts the resumed file's own standing instead of
/// re-reading the cleared default as an override — a refused rewind
/// keeps the statement, the still-open session still runs under it.
fn land_session_clear<E: std::fmt::Display>(
    rewind: Result<usize, E>,
    transcript: &TranscriptHandle,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
    context: &mut DriverContext,
) -> bool {
    let rotated = rewind_earned_fresh_session(&rewind);
    if rotated {
        let _adoption = context
            .lanes
            .adoption_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = transcript.rotate(&context.config.api.model);
        let default = context.overrides.permission_default;
        context.config.runner.permission_mode = default;
        *context
            .lanes
            .mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = default;
        context.overrides.permission_named = false;
        context.lanes.memory.adopt(Vec::new(), Vec::new(), false);
        let selection = context
            .lanes
            .preset_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reseed_config_rules_cell(
            &context.lanes.config_rules_cell,
            &context.config,
            selection.as_deref(),
        );
        if session_tx.send(SessionUpdate::SessionId { id }).is_err() {
            tracing::warn!("session cleared but the display is gone");
        }
        if session_tx
            .send(SessionUpdate::PermissionMode { mode: default })
            .is_err()
        {
            tracing::warn!("session cleared but the display is gone");
        }
        return true;
    } else if let Err(err) = rewind {
        post_session_notice(
            session_tx,
            format!("the session could not be cleared — the loop kept its history ({err})"),
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::driver::DriverContext;
    use crate::tui::driver::RunOverrides;
    use crate::tui::driver::run_driver_loop;
    use crate::tui::permissions_bridge::InteractiveLanes;
    use crate::tui::permissions_bridge::tighten_config_rules_cell;
    use crate::tui::test_support::*;
    use crate::tui::transcript::TranscriptWorker;
    use dch_tui::SessionCommand;
    use dch_tui::SessionUpdate;
    use dch_tui::Submission;
    use dch_tui::TuiObserverState;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn a_clear_command_at_rest_publishes_a_fresh_session_id() {
        // The session lane's clear leg at rest: the driver empties the loop's
        // history and moves the save target to a new session through the
        // transcript handle, then publishes the new id so the bar shows the
        // conversation's replacement, not the ended one. The pin reads the
        // saver's identity before and after: the published id is the save
        // target the rotation opened, never the ended session's — an id
        // that is merely a valid uuid proves nothing, the old one is one
        // too.
        let runner = unreachable_runner().await;
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (transcript, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Clear,
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
                sessions_root: dir.path().to_path_buf(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            transcript.clone(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a session id is published")
            .expect("the session channel is open when the command lands");
        let SessionUpdate::SessionId { id: published } = update else {
            panic!("a clear publishes a fresh session id, got {update:?}");
        };
        assert_ne!(
            published,
            id.to_string(),
            "the rotation moved off the session it ended"
        );
        let live_id = transcript
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id()
            .to_string();
        assert_eq!(
            published, live_id,
            "the published id names the save target the rotation opened"
        );
        worker.join();
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_refused_clear_lands_as_a_notice_and_rotates_nothing() {
        // The Clear arm's caller-side wiring, not only its predicate: a
        // rewind that refuses must post the notice, publish no fresh
        // id, and leave the save target on the session it stood on —
        // the caller that forgets to ask the gate is what this catches.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let refused = loopctl::error::LoopError::InvalidInput("a turn is in flight".to_string());
        let mut context = DriverContext {
            config: dch_config::DchConfig::default(),
            config_base: Arc::new(dch_config::DchConfig::default()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        land_session_clear(Err(refused), &handle, &session_tx, &mut context);
        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the refusal is said aloud")
            .expect("the session channel is open");
        let SessionUpdate::Notice { text } = notice else {
            panic!("a refused clear posts a notice, got {notice:?}")
        };
        assert!(
            text.contains("could not be cleared"),
            "the notice names what was refused: {text}"
        );
        assert!(
            session_rx.try_recv().is_err(),
            "no fresh session id rides a refused clear"
        );
        worker.join();
        let saving = handle
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            saving.session_id(),
            id,
            "the save target never rotated off the refused session"
        );
    }

    #[tokio::test]
    async fn a_cleared_session_resets_the_live_mode_to_the_configured_default() {
        // The live half of the rotation's mode reset — the save-file
        // half is the rotation pin's ("a rotation drops it"): a `/clear`
        // after a mid-session `/mode` hands the loop's config and the
        // gate's shared cell back to the configured default and tells
        // the display, so a fresh session visibly runs what the config
        // wrote, not the ended session's last move.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let base = dch_config::DchConfig::default();
        let mut moved = base.clone();
        moved.runner.permission_mode = dch_config::PermissionMode::Interactive;
        let lanes = InteractiveLanes::detached();
        *lanes
            .mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            dch_config::PermissionMode::Interactive;
        let mut context = DriverContext {
            config: moved,
            config_base: Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes,
            overrides: RunOverrides::default(),
        };
        land_session_clear::<loopctl::error::LoopError>(
            Ok(2_usize),
            &handle,
            &session_tx,
            &mut context,
        );
        let SessionUpdate::SessionId { id: fresh } =
            tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the rotation publishes its fresh id")
                .expect("the session channel is open")
        else {
            panic!("a landed clear publishes a fresh session id first")
        };
        assert_ne!(fresh, id.to_string(), "the rotation minted a fresh id");
        let SessionUpdate::PermissionMode { mode } =
            tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the reset is said aloud")
                .expect("the session channel is open")
        else {
            panic!("a landed clear publishes the mode the fresh session runs under")
        };
        assert_eq!(
            mode,
            dch_config::PermissionMode::Auto,
            "the fresh session runs the configured default"
        );
        assert_eq!(
            context.config.runner.permission_mode,
            dch_config::PermissionMode::Auto,
            "the loop's own config carries the default the next dispatch reads"
        );
        assert_eq!(
            *context
                .lanes
                .mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Auto,
            "the gate's shared cell hands the ended session's move back"
        );
        worker.join();
    }

    #[test]
    fn a_cleared_session_retires_the_launch_permission_statement() {
        // The flag named the launch's mode; the rotation adopted the
        // file default, so the statement retires with the session it
        // named — a later `/resume` must adopt the resumed file's own
        // standing rather than re-read the cleared default as an
        // override. A refused rewind keeps the statement: the
        // still-open session still runs under it.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides {
                model: None,
                reasoning_effort: None,
                permission_named: true,
                permission_default: dch_config::PermissionMode::Auto,
            },
        };
        context.config.runner.permission_mode = dch_config::PermissionMode::ReadOnly;
        land_session_clear::<loopctl::error::LoopError>(
            Ok(1_usize),
            &handle,
            &session_tx,
            &mut context,
        );
        assert!(
            !context.overrides.permission_named,
            "the rotation retires the launch statement it ended"
        );
        assert_eq!(
            context.config.runner.permission_mode,
            dch_config::PermissionMode::Auto,
            "the cleared session runs the file default the statement retired onto"
        );
        let mut kept = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides {
                model: None,
                reasoning_effort: None,
                permission_named: true,
                permission_default: dch_config::PermissionMode::Auto,
            },
        };
        kept.config.runner.permission_mode = dch_config::PermissionMode::ReadOnly;
        land_session_clear(
            Err("a turn is in flight".to_string()),
            &handle,
            &session_tx,
            &mut kept,
        );
        assert!(
            kept.overrides.permission_named,
            "a refused rewind keeps the launch statement — the session it named still stands"
        );
        assert_eq!(
            kept.config.runner.permission_mode,
            dch_config::PermissionMode::ReadOnly,
            "the refusal moves no mode"
        );
        worker.join();
    }

    #[test]
    fn a_damaged_resume_reports_the_mode_the_cell_enforces() {
        // The fallback's return half: with damage standing and the
        // cell already asking, the verdict must publish the mode the
        // cell enforces — not the standing mode the gate no longer
        // runs, which a later failed rebuild would roll the asking
        // cell back to. An auto cell takes the asking posture and
        // reports it.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let root = dir.keep();
        let b = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            b,
            "m".to_string(),
            root.clone(),
        ));
        saver
            .save_permissions(&[], &[], false)
            .expect("the sibling saves");
        let path = root.join(b.to_string()).join("permissions.json");
        let intact = std::fs::read_to_string(&path).expect("the sibling reads");
        let cut = intact.len().saturating_sub(3);
        std::fs::write(&path, intact.get(..cut).unwrap_or("")).expect("the damaged sibling writes");
        let (handle, _worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root.clone(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        *context
            .lanes
            .mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            dch_config::PermissionMode::ReadOnly;
        let (mode, damaged) =
            adopt_resumed_permissions(&handle, &mut context, dch_config::PermissionMode::Auto);
        assert!(damaged, "the truncated sibling fails the restore closed");
        assert_eq!(
            mode,
            dch_config::PermissionMode::ReadOnly,
            "the verdict names the mode the cell enforces — a failed rebuild \
             cannot roll the asking cell back to the standing auto"
        );
        let mut fresh = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root,
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        let (mode, damaged) =
            adopt_resumed_permissions(&handle, &mut fresh, dch_config::PermissionMode::Auto);
        assert!(damaged, "the same damaged sibling stands for both cells");
        assert_eq!(
            mode,
            dch_config::PermissionMode::Interactive,
            "an auto cell takes the asking posture and the verdict says so"
        );
    }

    #[tokio::test]
    async fn a_clear_after_damage_reseeds_the_config_rules_with_the_verdict() {
        // The round-4 lifecycle case: a damaged restore tightened the
        // config-rules cell, and `/clear` adopts a fresh memory —
        // clearing the verdict the tightening was keyed to. The
        // rotation re-seeds the cell from the configuration's own
        // join, so the fresh session runs the config's allows as
        // written instead of inheriting a withdrawn layer whose one
        // door — `/permissions repair` — now answers "nothing to
        // repair" against the cleared verdict.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut config = dch_config::DchConfig::default();
        config.permissions.rules = vec![dch_config::PermissionRuleConfig {
            id: Some("allow-bash".to_string()),
            outcome: dch_config::PermissionRuleOutcome::Allow,
            tool: Some("Bash".to_string()),
            shell_prefix: None,
            path_glob: None,
        }];
        let mut context = DriverContext {
            config: config.clone(),
            config_base: Arc::new(dch_config::DchConfig::default()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        // The startup join seeds the cell; a damaged restore projects
        // it onto its tightening half — the posture the rotation must
        // not leave behind.
        *context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dch_loop::config_rules(&config);
        context.lanes.memory.mark_damaged();
        tighten_config_rules_cell(&context.lanes.config_rules_cell);
        assert!(
            context.lanes.memory.damage_standing(),
            "the damage verdict stands before the clear"
        );
        assert!(
            context
                .lanes
                .config_rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .all(|rule| rule.outcome != dch_permissions::PermissionOutcome::Allow),
            "the damage posture withdraws the configured allow"
        );

        land_session_clear::<loopctl::error::LoopError>(
            Ok(2_usize),
            &handle,
            &session_tx,
            &mut context,
        );

        assert!(
            !context.lanes.memory.damage_standing(),
            "the fresh adoption clears the verdict the repair door reads"
        );
        assert_eq!(
            context
                .lanes
                .config_rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            dch_loop::config_rules(&config),
            "the cell returns to the configuration's own join — the tightening \
             died with the verdict, not with a repair that can no longer fire"
        );
        worker.join();
    }

    #[tokio::test]
    async fn a_preset_selection_survives_the_clear_s_fresh_adoption() {
        // The round-4 preset-regression case on the clear lane: the
        // boot config selected p1, a mid-session `/preset p2` recorded
        // the swap on the lanes, and the `/clear` rotation re-seeds
        // from the selection in effect — p2's join — so the live deny
        // cannot silently leave the gate while the boot preset's
        // allow returns unasked.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut config = dch_config::DchConfig::default();
        config.permissions.preset = Some("p1".to_string());
        config.permissions.presets.insert(
            "p1".to_string(),
            dch_permissions::Preset {
                rules: vec![dch_config::PermissionRuleConfig {
                    id: None,
                    outcome: dch_config::PermissionRuleOutcome::Allow,
                    tool: Some("Bash".to_string()),
                    shell_prefix: None,
                    path_glob: None,
                }],
            },
        );
        config.permissions.presets.insert(
            "p2".to_string(),
            dch_permissions::Preset {
                rules: vec![dch_config::PermissionRuleConfig {
                    id: Some("no-push".to_string()),
                    outcome: dch_config::PermissionRuleOutcome::Deny,
                    tool: None,
                    shell_prefix: Some("git push".to_string()),
                    path_glob: None,
                }],
            },
        );
        let mut context = DriverContext {
            config: config.clone(),
            config_base: Arc::new(dch_config::DchConfig::default()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        *context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dch_loop::config_rules(&config);
        *context
            .lanes
            .preset_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("p2".to_string());

        land_session_clear::<loopctl::error::LoopError>(
            Ok(2_usize),
            &handle,
            &session_tx,
            &mut context,
        );

        assert_eq!(
            context
                .lanes
                .config_rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            dch_loop::config_rules_at(&config, Some("p2")),
            "the fresh session runs the live selection's join — the boot \
             preset does not come back with the rotation"
        );
        worker.join();
    }

    #[tokio::test]
    async fn a_cleared_session_starts_with_no_remembered_rules() {
        // The memory is session state: the ended session answered
        // "allow always" on `cargo test` and "always deny" on a
        // tool, then a `/clear` landed. The fresh session must ask
        // again for both — a carried rule would auto-run work the
        // fresh conversation never approved, and the next remember
        // press would persist the carried pair into the fresh
        // session's own file, the carryover made durable.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let lanes = InteractiveLanes::detached();
        lanes.memory.adopt(
            vec![dch_tui::PermissionRule::ShellPrefix {
                prefix: "cargo test".to_string(),
            }],
            vec![dch_tui::PermissionRule::Tool {
                tool: "WebFetch".to_string(),
            }],
            false,
        );
        let mut context = DriverContext {
            config: dch_config::DchConfig::default(),
            config_base: Arc::new(dch_config::DchConfig::default()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes,
            overrides: RunOverrides::default(),
        };
        land_session_clear::<loopctl::error::LoopError>(
            Ok(2_usize),
            &handle,
            &session_tx,
            &mut context,
        );
        let SessionUpdate::SessionId { id: fresh } =
            tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the rotation publishes its fresh id")
                .expect("the session channel is open")
        else {
            panic!("a landed clear publishes a fresh session id first")
        };
        assert!(
            context.lanes.memory.allow_rules().is_empty(),
            "the fresh session asks again on verbs the ended session allowed"
        );
        assert!(
            context.lanes.memory.deny_rules().is_empty(),
            "the fresh session asks again on tools the ended session refused"
        );
        assert!(
            !dir.path().join(&fresh).join("permissions.json").exists(),
            "the fresh session's own file stays empty until its own press"
        );
        worker.join();
    }

    #[tokio::test]
    async fn a_cleared_session_returns_to_the_files_default_not_the_launch_flags_mode() {
        // A launch `--permission-mode` is the opened session's own
        // override, not a property of every fresh session after it:
        // the base the driver carries is flag-tainted, and a clear
        // that read it would run the fresh session under the ended
        // session's flag — while the rotated save file records no
        // override, so a resume would disagree with what it ran as.
        // The driver carries the file's own default pristine, and the
        // clear lands there, exactly the reference's chain.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, _worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        // The base carries the flag's mode — what a clear must not
        // land on — and the file's own default says otherwise.
        let mut base = dch_config::DchConfig::default();
        base.runner.permission_mode = dch_config::PermissionMode::ReadOnly;
        let lanes = InteractiveLanes::detached();
        *lanes
            .mode_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            dch_config::PermissionMode::ReadOnly;
        let mut context = DriverContext {
            config: base.clone(),
            config_base: Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state: TuiObserverState::new().into_observer().1,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lanes,
            overrides: RunOverrides {
                model: None,
                reasoning_effort: None,
                permission_named: true,
                permission_default: dch_config::PermissionMode::Auto,
            },
        };
        land_session_clear::<loopctl::error::LoopError>(
            Ok(2_usize),
            &handle,
            &session_tx,
            &mut context,
        );
        let SessionUpdate::SessionId { .. } =
            tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the rotation publishes its fresh id")
                .expect("the session channel is open")
        else {
            panic!("a landed clear publishes a fresh session id first")
        };
        let SessionUpdate::PermissionMode { mode } =
            tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                .await
                .expect("the reset is said aloud")
                .expect("the session channel is open")
        else {
            panic!("a landed clear publishes the mode the fresh session runs under")
        };
        assert_eq!(
            mode,
            dch_config::PermissionMode::Auto,
            "the fresh session runs the file's default, not the flag's mode"
        );
        assert_eq!(
            context.config.runner.permission_mode,
            dch_config::PermissionMode::Auto,
            "the loop's own config carries the file's default"
        );
        assert_eq!(
            *context
                .lanes
                .mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Auto,
            "the gate's shared cell lands on the file's default"
        );
    }

    #[tokio::test]
    async fn a_sessions_listing_carries_the_saved_sessions_newest_first() {
        // The `/sessions` ask, driven end to end: the driver lists the
        // sessions root it holds and hands the catalog back newest
        // first — the older session saved earlier, the newer later,
        // and the picker's first row is the newer one.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let older = uuid::Uuid::new_v4();
        let newer = uuid::Uuid::new_v4();
        for (id, model, stamp) in [
            (
                older,
                "older-model",
                chrono::Utc::now() - chrono::Duration::hours(2),
            ),
            (newer, "newer-model", chrono::Utc::now()),
        ] {
            let saver = crate::session::SessionSaver::with_base_dir(
                id,
                model.to_string(),
                dir.path().to_path_buf(),
            );
            saver
                .save_at(
                    &[dch_tui::TuiMessage::User {
                        text: "the saved turn".to_string(),
                        timestamp: stamp,
                        steered: false,
                        unsent: false,
                    }],
                    crate::session::SessionTokens::default(),
                    stamp,
                )
                .expect("the saved session lands");
        }
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::ListSessions,
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: unreachable_config(),
                config_base: std::sync::Arc::new(unreachable_config()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: dir.path().to_path_buf(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            TranscriptWorker::spawn(Arc::new(crate::session::SessionSaver::with_base_dir(
                uuid::Uuid::new_v4(),
                "m".to_string(),
                dir.path().to_path_buf(),
            )))
            .0,
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the catalog is published")
            .expect("the session channel is open when the listing lands");
        let SessionUpdate::SessionCatalog { rows } = update else {
            panic!("a listing answers with the catalog, got {update:?}")
        };
        assert_eq!(rows.len(), 2, "both saved sessions list: {rows:?}");
        assert_eq!(
            rows.first()
                .and_then(|row| uuid::Uuid::parse_str(&row.id).ok()),
            Some(newer),
            "the newest session is the picker's first row"
        );
        assert_eq!(rows[1].model, "older-model", "each row carries its model");
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_resume_command_rebuilds_on_the_saved_history_and_takes_its_file() {
        // The live swap, driven end to end: the driver loads the saved pair
        // from the sessions root it holds, rebuilds the runner on the
        // restored history, retargets the save target onto the session's own
        // file, and publishes the loaded transcript back so the display can
        // re-seed. The handle's saver ends on the resumed id — the file the
        // next save writes.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "saved-model".to_string(),
            dir.path().to_path_buf(),
        ));
        let now = chrono::Utc::now();
        let conversation = vec![dch_tui::TuiMessage::User {
            text: "the saved turn".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        }];
        let tokens = crate::session::SessionTokens {
            cumulative_input: 21,
            cumulative_output: 13,
            last_input_tokens: 5,
            context_tokens: 8,
        };
        saver
            .save_at(&conversation, tokens, now)
            .expect("the saved session lands");
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));

        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Resume { id: id.to_string() },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: unreachable_config(),
                config_base: std::sync::Arc::new(unreachable_config()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: dir.path().to_path_buf(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            handle.clone(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a resumed update is published")
            .expect("the session channel is open when the resume lands");
        let SessionUpdate::Resumed {
            id: resumed_id,
            transcript,
            tokens: resumed_tokens,
            permission_mode: _,
        } = update
        else {
            panic!("a resume publishes the loaded transcript, got {update:?}");
        };
        assert_eq!(resumed_id, id.to_string());
        assert_eq!(
            resumed_tokens.cumulative_input, 21,
            "the accounting continues the saved session's count"
        );
        assert!(
            transcript.iter().any(|message| matches!(
                message,
                dch_tui::TuiMessage::User { text, .. } if text == "the saved turn"
            )),
            "the loaded transcript carries the saved turn: {transcript:?}"
        );
        let _ = driver.await;
        worker.join();
        let saving = handle
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            saving.session_id(),
            id,
            "later saves write the resumed session's own file"
        );
    }

    #[tokio::test]
    async fn a_resume_publishes_the_adopted_machine_under_the_resumed_session() {
        // The ordering the seam demands, with distinct ids so it cannot
        // hide behind a shared one: the save target retargets onto the
        // resumed session before the adopted machine publishes, so the
        // snapshot pins the resumed session's saver and the write lands
        // beside its transcript — never under the outgoing session the
        // driver is leaving, whose newer machine stamp would hand a
        // later resume the wrong pair.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let outgoing = uuid::Uuid::new_v4();
        let target = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            outgoing,
            "outgoing-model".to_string(),
            dir.path().to_path_buf(),
        ));
        let target_saver = crate::session::SessionSaver::with_base_dir(
            target,
            "saved-model".to_string(),
            dir.path().to_path_buf(),
        );
        let now = chrono::Utc::now();
        target_saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the saved turn".to_string(),
                    timestamp: now,
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                now,
            )
            .expect("the saved session lands");
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));

        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Resume {
                    id: target.to_string(),
                },
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: unreachable_config(),
                config_base: std::sync::Arc::new(unreachable_config()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: dir.path().to_path_buf(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            handle.clone(),
            session_tx,
        )));
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("a resumed update is published")
            .expect("the session channel is open when the resume lands");
        assert!(
            matches!(update, SessionUpdate::Resumed { .. }),
            "the resume lands, got {update:?}"
        );
        let _ = driver.await;
        worker.join();
        assert!(
            dir.path()
                .join(target.to_string())
                .join("machine.json")
                .is_file(),
            "the adopted machine lands beside the resumed session's transcript"
        );
        assert!(
            !dir.path()
                .join(outgoing.to_string())
                .join("machine.json")
                .exists(),
            "the outgoing session never receives the resumed machine"
        );
    }

    #[tokio::test]
    async fn a_resume_that_cannot_load_notices_and_keeps_the_loop() {
        // A well-formed id with no saved session behind it: the driver says
        // so on the notice lane, and the conversation it holds stands — no
        // session id is published, nothing is rotated.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Resume {
                    id: uuid::Uuid::new_v4().to_string(),
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
                sessions_root: dir.path().to_path_buf(),
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
            panic!("an unloadable resume posts a notice, got {update:?}");
        };
        assert!(
            text.contains("could not be loaded"),
            "the notice says what stood: {text}"
        );
        let _ = driver.await;
    }

    #[tokio::test]
    async fn a_live_resume_adopts_the_saved_sessions_model_through_its_profile() {
        // The startup rule, kept on the live path: a resumed session
        // continues on the model it last ran, resolved through its
        // `[[models]]` entry — the profile's window is what the rebuilt
        // engine measures — and the run's effort flag goes back on top
        // of whatever the entry states, exactly as the boot order does.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = crate::session::SessionSaver::with_base_dir(
            id,
            "saved-model".to_string(),
            dir.path().to_path_buf(),
        );
        let now = chrono::Utc::now();
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the saved turn".to_string(),
                    timestamp: now,
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                now,
            )
            .expect("the saved session lands");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut base = unreachable_config();
        base.models = vec![dch_config::ModelProfile {
            model: "saved-model".to_string(),
            context_window: Some(123_456),
            reasoning_effort: Some("high".to_string()),
            ..dch_config::ModelProfile::default()
        }];
        let window_probe = state.clone();
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
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
            SessionCommand::Resume { id: id.to_string() },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.model, "saved-model",
            "the resumed session's own model is adopted"
        );
        assert_eq!(
            context.config.api.context_window,
            Some(123_456),
            "the adoption resolves through the entry's profile"
        );
        assert_eq!(
            context.config.api.reasoning_effort.as_deref(),
            Some("low"),
            "the run's flag outranks the adopted entry's dial"
        );
        assert_eq!(
            window_probe.context_window.load(Ordering::SeqCst),
            123_456,
            "the context bar measures the adopted model's own window"
        );
    }

    #[tokio::test]
    async fn a_live_resume_adopts_the_resumed_sessions_remembered_rules_wholesale() {
        // The memory is session state: session A answered "allow
        // always" on `cargo test`, then the user resumes session B
        // live. A's rule must not ride into B — B's own pair replaces
        // the memory wholesale, exactly what adoption promises. The
        // drift shape: the next dispatch in B would auto-run under a
        // rule B never made, and the persist hook would write A's
        // carried pair into B's permissions.json — the carryover made
        // durable on disk.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let root = dir.keep();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        for (id, prefix) in [a, b].iter().zip(["cargo test", "cargo build"]) {
            let saver = crate::session::SessionSaver::with_base_dir(
                *id,
                "test-model".to_string(),
                root.clone(),
            );
            saver
                .save_at(
                    &[dch_tui::TuiMessage::User {
                        text: "the saved turn".to_string(),
                        timestamp: chrono::Utc::now(),
                        steered: false,
                        unsent: false,
                    }],
                    crate::session::SessionTokens::default(),
                    chrono::Utc::now(),
                )
                .expect("the saved session lands");
            saver
                .save_permissions(
                    &[dch_tui::PermissionRule::ShellPrefix {
                        prefix: prefix.to_string(),
                    }],
                    &[],
                    false,
                )
                .expect("the remembered pair saves");
        }
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root.clone(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        // Session A is live and has remembered its verb — the state the
        // resume must replace, not keep.
        context.lanes.memory.adopt(
            vec![dch_tui::PermissionRule::ShellPrefix {
                prefix: "cargo test".to_string(),
            }],
            vec![],
            false,
        );
        // One root, the production shape: the save cell the resume
        // retargets reads the same base the load read, so the
        // adoption's `load_permissions` lands on B's own sibling.
        let parked_saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, _worker) = TranscriptWorker::spawn(parked_saver);
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: b.to_string() },
            &mut context,
        )
        .await;
        assert_eq!(
            context.lanes.memory.allow_rules(),
            vec![dch_tui::PermissionRule::ShellPrefix {
                prefix: "cargo build".to_string(),
            }],
            "the resumed session's own pair replaces the previous session's wholesale"
        );
    }

    #[tokio::test]
    async fn a_live_resume_of_a_damaged_permissions_sibling_fails_closed() {
        // The reviewer's case on the live-resume path: session B saved a
        // standing deny and ran auto, its permissions sibling is then
        // damaged on disk, and the resume must not come back as a quiet
        // empty memory under a mode that never asks. The verdict adopts
        // nothing, the cell moves from the standing auto to the asking
        // posture, the notice names the damage, and the Resumed update
        // publishes the enforced mode — the display never shows a
        // posture the gate does not run.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let root = dir.keep();
        let b = uuid::Uuid::new_v4();
        let saver =
            crate::session::SessionSaver::with_base_dir(b, "test-model".to_string(), root.clone());
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the saved turn".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .expect("the saved session lands");
        saver
            .save_permissions(
                &[],
                &[dch_tui::PermissionRule::ShellPrefix {
                    prefix: "cargo publish".to_string(),
                }],
                false,
            )
            .expect("the deny saves");
        let path = root.join(b.to_string()).join("permissions.json");
        let intact = std::fs::read_to_string(&path).expect("the sibling reads");
        let cut = intact.len().saturating_sub(3);
        std::fs::write(&path, intact.get(..cut).unwrap_or("")).expect("the damaged sibling writes");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root.clone(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        // A configured allow would cover the asking posture the
        // fallback moves the cell to; a deny must survive it.
        *context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = vec![
            dch_permissions::Rule {
                id: "allow-bash".to_string(),
                outcome: dch_permissions::PermissionOutcome::Allow,
                matcher: dch_permissions::Matcher::Tool("Bash".to_string()),
            },
            dch_permissions::Rule {
                id: "deny-env".to_string(),
                outcome: dch_permissions::PermissionOutcome::Block,
                matcher: dch_permissions::Matcher::Tool("Write".to_string()),
            },
        ];
        let parked_saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, _worker) = TranscriptWorker::spawn(parked_saver);
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: b.to_string() },
            &mut context,
        )
        .await;
        assert!(
            context.lanes.memory.allow_rules().is_empty()
                && context.lanes.memory.deny_rules().is_empty(),
            "a damaged sibling adopts nothing — the deny is not silently gone"
        );
        assert_eq!(
            *context
                .lanes
                .mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Interactive,
            "the auto the session stood on fails closed to the asking posture"
        );
        let mut damage_notice = None;
        let mut resumed_mode = None;
        // The dispatch publishes on the lane before it returns, so the
        // queue is read without waiting: a missing update fails here
        // instead of parking on a channel whose sender never closes.
        while let Ok(update) = session_rx.try_recv() {
            match update {
                SessionUpdate::Notice { text } => damage_notice = Some(text),
                SessionUpdate::Resumed {
                    permission_mode, ..
                } => resumed_mode = Some(permission_mode),
                _ => {}
            }
        }
        let notice = damage_notice.expect("the damage is said aloud on the notice lane");
        assert!(
            notice.contains("damaged"),
            "the notice names the damage: {notice}"
        );
        assert_eq!(
            resumed_mode,
            Some(dch_config::PermissionMode::Interactive),
            "the resumed display publishes the enforced posture, never the standing auto"
        );
        assert_eq!(
            context.config.runner.permission_mode,
            dch_config::PermissionMode::Interactive,
            "the retained config names the enforced mode — a later failed /mode \
             rebuild rolls back to it, never to the standing auto"
        );
        let remaining = context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            remaining
                .iter()
                .all(|rule| rule.outcome != dch_permissions::PermissionOutcome::Allow),
            "a damaged restore drops the configured allows — they would cover the ask: {remaining:?}"
        );
        assert!(
            remaining
                .iter()
                .any(|rule| rule.outcome == dch_permissions::PermissionOutcome::Block),
            "the configured tightening half stays"
        );
    }

    #[tokio::test]
    async fn a_healthy_resume_after_damage_returns_the_config_rules_with_the_verdict() {
        // The round-4 lifecycle case on the resume lane: the damaged
        // session's restore tightened the config-rules cell, and the
        // healthy session adopted after it replaces the memory —
        // verdict included. The healthy adoption re-seeds the cell
        // from the configuration's own join, so the damage posture
        // cannot outlive the verdict that keyed it: the config's
        // allows are in force for the healthy session, and the
        // repair door the posture left standing now answers
        // "nothing to repair".
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let root = dir.keep();
        let b = uuid::Uuid::new_v4();
        let damaged_saver =
            crate::session::SessionSaver::with_base_dir(b, "test-model".to_string(), root.clone());
        damaged_saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the damaged session's turn".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .expect("the damaged session lands");
        damaged_saver
            .save_permissions(&[], &[], false)
            .expect("the intact sibling saves");
        let damaged_path = root.join(b.to_string()).join("permissions.json");
        let intact = std::fs::read_to_string(&damaged_path).expect("the sibling reads");
        let cut = intact.len().saturating_sub(3);
        std::fs::write(&damaged_path, intact.get(..cut).unwrap_or(""))
            .expect("the damaged sibling writes");
        let c = uuid::Uuid::new_v4();
        let healthy_saver =
            crate::session::SessionSaver::with_base_dir(c, "test-model".to_string(), root.clone());
        healthy_saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the healthy session's turn".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .expect("the healthy session lands");
        healthy_saver
            .save_permissions(
                &[dch_tui::PermissionRule::ShellPrefix {
                    prefix: "cargo build".to_string(),
                }],
                &[],
                false,
            )
            .expect("the healthy sibling saves");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut config = unreachable_config();
        config.permissions.rules = vec![dch_config::PermissionRuleConfig {
            id: Some("allow-bash".to_string()),
            outcome: dch_config::PermissionRuleOutcome::Allow,
            tool: Some("Bash".to_string()),
            shell_prefix: None,
            path_glob: None,
        }];
        let mut context = DriverContext {
            config: config.clone(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root.clone(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        // The startup join seeds the cell before the damaged resume
        // projects it onto its tightening half.
        *context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dch_loop::config_rules(&config);
        let parked_saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, _worker) = TranscriptWorker::spawn(parked_saver);
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: b.to_string() },
            &mut context,
        )
        .await;
        assert!(
            context.lanes.memory.damage_standing(),
            "the damaged resume arms the posture"
        );
        assert!(
            context
                .lanes
                .config_rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .all(|rule| rule.outcome != dch_permissions::PermissionOutcome::Allow),
            "the damaged restore withdraws the configured allow"
        );

        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: c.to_string() },
            &mut context,
        )
        .await;

        assert!(
            !context.lanes.memory.damage_standing(),
            "the healthy adoption replaces the verdict wholesale"
        );
        assert_eq!(
            context
                .lanes
                .config_rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            dch_loop::config_rules(&config),
            "the healthy resume returns the cell to the configuration's own join — \
             the earlier damage cannot withdraw the config layer from this session"
        );
    }

    #[tokio::test]
    async fn a_preset_selection_survives_a_healthy_live_resume() {
        // The round-4 regression on the resume lane: the boot config
        // selected p1, a mid-session `/preset p2` recorded the swap
        // on the lanes, and a healthy live resume adopts its fresh
        // memory — the re-seed must compile the config layer at the
        // selection in effect, or the live preset's deny silently
        // leaves the gate while the boot preset's allow returns.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let root = dir.keep();
        let c = uuid::Uuid::new_v4();
        let saver =
            crate::session::SessionSaver::with_base_dir(c, "test-model".to_string(), root.clone());
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the healthy session's turn".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .expect("the healthy session lands");
        saver
            .save_permissions(
                &[dch_tui::PermissionRule::ShellPrefix {
                    prefix: "cargo build".to_string(),
                }],
                &[],
                false,
            )
            .expect("the healthy sibling saves");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut config = unreachable_config();
        config.permissions.preset = Some("p1".to_string());
        config.permissions.presets.insert(
            "p1".to_string(),
            dch_permissions::Preset {
                rules: vec![dch_config::PermissionRuleConfig {
                    id: None,
                    outcome: dch_config::PermissionRuleOutcome::Allow,
                    tool: Some("Bash".to_string()),
                    shell_prefix: None,
                    path_glob: None,
                }],
            },
        );
        config.permissions.presets.insert(
            "p2".to_string(),
            dch_permissions::Preset {
                rules: vec![dch_config::PermissionRuleConfig {
                    id: Some("no-push".to_string()),
                    outcome: dch_config::PermissionRuleOutcome::Deny,
                    tool: None,
                    shell_prefix: Some("git push".to_string()),
                    path_glob: None,
                }],
            },
        );
        let mut context = DriverContext {
            config: config.clone(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root.clone(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        *context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dch_loop::config_rules(&config);
        *context
            .lanes
            .preset_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("p2".to_string());
        let parked_saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, _worker) = TranscriptWorker::spawn(parked_saver);
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: c.to_string() },
            &mut context,
        )
        .await;
        assert_eq!(
            context
                .lanes
                .config_rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            dch_loop::config_rules_at(&config, Some("p2")),
            "the healthy resume compiles the config layer at the live \
             selection — the boot preset does not come back with the adoption"
        );
    }

    #[tokio::test]
    async fn a_standing_damage_marker_re_arms_the_resume_and_rides_the_saves() {
        // The relaunch laundering hole, closed end to end: the saved
        // file carries the marker beside its re-answers, the resume
        // restores the re-answers *and* re-arms the posture — the mode
        // cell off auto, the configured allows tightened, the retained
        // config named, the marker standing on the memory — and the
        // very next remembered answer saves a file that still says
        // damaged. The restriction cannot evaporate across a
        // save/relaunch; only `/permissions repair` clears it.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let root = dir.keep();
        let b = uuid::Uuid::new_v4();
        let saver =
            crate::session::SessionSaver::with_base_dir(b, "test-model".to_string(), root.clone());
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the saved turn".to_string(),
                    timestamp: chrono::Utc::now(),
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                chrono::Utc::now(),
            )
            .expect("the saved session lands");
        // The file a damaged-then-reanswered session writes: real
        // re-answers, marker standing.
        saver
            .save_permissions(
                &[dch_tui::PermissionRule::ShellPrefix {
                    prefix: "cargo test".to_string(),
                }],
                &[],
                true,
            )
            .expect("the marked pair saves");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: root.clone(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides::default(),
        };
        *context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = vec![
            dch_permissions::Rule {
                id: "allow-bash".to_string(),
                outcome: dch_permissions::PermissionOutcome::Allow,
                matcher: dch_permissions::Matcher::Tool("Bash".to_string()),
            },
            dch_permissions::Rule {
                id: "deny-env".to_string(),
                outcome: dch_permissions::PermissionOutcome::Block,
                matcher: dch_permissions::Matcher::Tool("Write".to_string()),
            },
        ];
        // The write-through a real session's wiring installs, so the
        // anti-laundering half runs against the file on disk.
        let hook_saver =
            crate::session::SessionSaver::with_base_dir(b, "test-model".to_string(), root.clone());
        context.lanes.memory.persist_through(Arc::new(
            move |allow: &[dch_tui::PermissionRule],
                  deny: &[dch_tui::PermissionRule],
                  damaged: bool| {
                let _ = hook_saver.save_permissions(allow, deny, damaged);
            },
        ));
        let parked_saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "m".to_string(),
            root.clone(),
        ));
        let (transcript, _worker) = TranscriptWorker::spawn(parked_saver);
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: b.to_string() },
            &mut context,
        )
        .await;
        assert_eq!(
            context.lanes.memory.allow_rules(),
            vec![dch_tui::PermissionRule::ShellPrefix {
                prefix: "cargo test".to_string()
            }],
            "the re-answers restore — they are real choices the session keeps"
        );
        assert!(
            context.lanes.memory.damage_standing(),
            "the marker re-arms with the resume"
        );
        assert_eq!(
            *context
                .lanes
                .mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Interactive,
            "the standing auto still fails closed to the asking posture"
        );
        assert_eq!(
            context.config.runner.permission_mode,
            dch_config::PermissionMode::Interactive,
            "the retained config names the enforced mode"
        );
        let remaining = context
            .lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            remaining
                .iter()
                .all(|rule| rule.outcome != dch_permissions::PermissionOutcome::Allow),
            "the configured allows stay tightened under the standing marker: {remaining:?}"
        );
        // The laundering attempt itself: the next remembered answer
        // saves, and the file it writes still says damaged.
        context
            .lanes
            .memory
            .remember_rules(&[dch_tui::PermissionRule::ShellPrefix {
                prefix: "cargo fmt".to_string(),
            }]);
        let raw = std::fs::read_to_string(root.join(b.to_string()).join("permissions.json"))
            .expect("the write-through fired");
        assert!(
            raw.contains("\"damaged\":true"),
            "the save cannot launder the marker away: {raw}"
        );
        assert!(
            raw.contains("cargo fmt"),
            "the re-answer itself did land: {raw}"
        );
    }

    #[tokio::test]
    async fn a_resume_rebuild_that_fails_keeps_the_live_config() {
        // The adoption is provisional: a config that cannot be built —
        // here through the blank fallback the construction breaker
        // refuses — leaves the live config, and the loop it stands for,
        // exactly as it stood, with the refusal said on the notice lane.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = crate::session::SessionSaver::with_base_dir(
            id,
            "saved-model".to_string(),
            dir.path().to_path_buf(),
        );
        let now = chrono::Utc::now();
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the saved turn".to_string(),
                    timestamp: now,
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                now,
            )
            .expect("the saved session lands");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        // No entries: the adoption takes the base's own api wholesale,
        // blank fallback included — the one the rebuild refuses.
        let mut base = unreachable_config();
        base.api.fallback_model = Some(String::new());
        let mut context = DriverContext {
            config: unreachable_config(),
            config_base: std::sync::Arc::new(base),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
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
            SessionCommand::Resume { id: id.to_string() },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.model, "test-model",
            "the live config stands — the adoption never landed"
        );
        let update = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the refused adoption reports")
            .expect("the session channel stays open");
        let SessionUpdate::Notice { text } = update else {
            panic!("a refused adoption posts a notice, got {update:?}");
        };
        assert!(
            text.contains("could not be adopted"),
            "the notice names the refusal: {text}"
        );
    }

    #[tokio::test]
    async fn a_live_resume_keeps_the_flag_model_over_the_saved_one() {
        // The startup adoption's own bow, made on the live path too: a
        // run launched with `--model` keeps it through a resume — the
        // saved session's model is provenance, never a replacement for
        // the user's named run.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = crate::session::SessionSaver::with_base_dir(
            id,
            "saved-model".to_string(),
            dir.path().to_path_buf(),
        );
        let now = chrono::Utc::now();
        saver
            .save_at(
                &[dch_tui::TuiMessage::User {
                    text: "the saved turn".to_string(),
                    timestamp: now,
                    steered: false,
                    unsent: false,
                }],
                crate::session::SessionTokens::default(),
                now,
            )
            .expect("the saved session lands");
        let mut runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let mut live = unreachable_config();
        live.api.model = "flag-model".to_string();
        let mut context = DriverContext {
            config: live,
            config_base: std::sync::Arc::new(unreachable_config()),
            workdir: std::path::PathBuf::from("."),
            sessions_root: dir.path().to_path_buf(),
            state,
            shutting_down: shutdown_flag(false),
            lanes: InteractiveLanes::detached(),
            overrides: RunOverrides {
                model: Some("flag-model".to_string()),
                reasoning_effort: None,
                permission_named: false,
                ..RunOverrides::default()
            },
        };
        let transcript = parked_transcript();
        apply_session_command(
            &mut runner,
            &transcript,
            &session_tx,
            SessionCommand::Resume { id: id.to_string() },
            &mut context,
        )
        .await;
        assert_eq!(
            context.config.api.model, "flag-model",
            "the flag's model stands through the resume"
        );
    }

    #[tokio::test]
    async fn a_model_switch_then_clear_files_the_fresh_session_under_the_live_model() {
        // Two commands in sequence: the rename the loop accepts updates the
        // driver's live configuration, and the clear's rotated saver records
        // that model — not the launch-time one — so the fresh session's
        // envelope names the loop that actually writes it.
        let dir = tempfile::tempdir().expect("transcript tempdir");
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            uuid::Uuid::new_v4(),
            "test-model".to_string(),
            dir.keep(),
        ));
        let (handle, _worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        let runner = unreachable_runner().await;
        let (_, state) = TuiObserverState::new().into_observer();
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Model {
                    name: "renamed-model".to_string(),
                },
            })
            .expect("driver channel open");
        submit_tx
            .send(Submission::Command {
                command: SessionCommand::Clear,
            })
            .expect("driver channel open");
        drop(submit_tx);
        let driver = tokio::spawn(Box::pin(run_driver_loop(
            runner,
            submit_rx,
            DriverContext {
                config: unreachable_config(),
                config_base: std::sync::Arc::new(unreachable_config()),
                workdir: std::path::PathBuf::from("."),
                sessions_root: crate::session::sessions_dir(),
                state,
                shutting_down: shutdown_flag(false),
                lanes: InteractiveLanes::detached(),
                overrides: RunOverrides::default(),
            },
            handle.clone(),
            session_tx,
        )));
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the rename is published")
            .expect("the session channel is open");
        let first = match first {
            SessionUpdate::Notice { .. } => {
                tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
                    .await
                    .expect("the verdict follows the receipt")
                    .expect("the session channel is open")
            }
            verdict => verdict,
        };
        assert_eq!(
            first,
            SessionUpdate::ModelName {
                name: "renamed-model".to_string()
            }
        );
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), session_rx.recv())
            .await
            .expect("the fresh id is published")
            .expect("the session channel is open");
        let SessionUpdate::SessionId { id } = second else {
            panic!("a clear publishes a fresh session id, got {second:?}");
        };
        assert_ne!(id, saver.session_id().to_string());
        let _ = driver.await;
        let saving = handle
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            saving.model, "renamed-model",
            "the rotated saver records the live loop's model"
        );
    }

    #[test]
    fn a_model_switch_relables_the_saver_without_moving_the_session() {
        // `/model` changes what the session runs, and the envelope
        // must say so: the saver's model field follows the switch so a
        // later resume defaults to it, while the session id — the
        // file's identity — never moves.
        let dir = tempfile::tempdir().expect("sessions tempdir");
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "first-model".to_string(),
            dir.path().to_path_buf(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        handle.relabel_model("switched-model");
        worker.join();
        let saving = handle
            .saver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            saving.model, "switched-model",
            "the envelope's model follows the switch the next save writes"
        );
        assert_eq!(
            saving.session_id(),
            id,
            "the switch moves the model, never the session's identity"
        );
    }

    #[test]
    fn a_refused_rewind_keeps_the_session_identity() {
        // The clear path's one gate: only a rewind that emptied the history
        // earns the fresh identity, so a refused rewind keeps the session —
        // its id, its file — exactly as it stood.
        let accepted: Result<usize, String> = Ok(3);
        assert!(
            rewind_earned_fresh_session(&accepted),
            "a rewind that emptied the history earns the fresh session"
        );
        let refused: Result<usize, String> = Err("a turn is in flight".to_string());
        assert!(
            !rewind_earned_fresh_session(&refused),
            "a refused rewind keeps the ended session standing"
        );
    }
}
