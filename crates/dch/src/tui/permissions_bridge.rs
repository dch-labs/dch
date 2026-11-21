//! Permission plumbing between the interactive panel and the
//! agent loop: the lanes a session wires once, the bridge that
//! resolves asks on the panel, and the memory and rule cells
//! the gate consults.

use std::path::Path;
use std::sync::Arc;

use dch_tui::PermissionMemory;
use dch_tui::PermissionRequest;
use dch_tui::PermissionRule;
use dch_tui::QuestionPrompt;
use dch_tui::SessionUpdate;
use tokio::sync::mpsc;

use super::transcript::TranscriptHandle;

/// The app's steering seam over the runner's slot.
///
/// The interface asks in text and answers in text; the slot speaks engine
/// messages, and this is that translation.
pub(crate) struct LoopGuidance {
    /// The session's slot, shared with the loop that drains it.
    ///
    /// An `Arc` rather than a borrow because the run holds the slot for as
    /// long as its future lives, and a key press arriving on another task
    /// cannot ask for it back for the duration. Both ends reach the same
    /// allocation: the loop reads it between turns, a steer writes into it.
    pub(crate) steering: Arc<loopctl::engine::Steering>,
}

impl dch_tui::Guidance for LoopGuidance {
    fn steer(&self, text: String) -> bool {
        // A user-role message, so the guidance enters the conversation as
        // something the user said — which is what it is. `false` when no
        // run is in flight, or ended that instant; the app then submits
        // the text as an ordinary prompt instead.
        self.steering.push(loopctl::message::Message::user(text))
    }

    fn take_returned(&self) -> Vec<String> {
        // `take_all`, not `take_leftovers`: a run that ends leaves its
        // undrained guidance in the live queue, and the sealed pocket is
        // only what an *earlier* run left behind. Asking for the pocket
        // alone would report a clean slot while the words the user sent
        // sat one shelf over, undelivered and unmentioned.
        self.steering
            .take_all()
            .iter()
            .map(loopctl::message::Message::text_content)
            .collect()
    }
}

/// The startup half of the "allow always" lane: a resumed session
/// adopts its own remembered choices at open (a fresh one finds
/// nothing to adopt), and every later remember writes through the
/// live saver cell — a rotation — `/clear`, a live resume — swaps the
/// target, and the hook reads the cell, so a choice always lands in
/// the session file that is current when it is made. The restore's
/// verdict travels back to the caller: damage is the caller's to
/// fail closed on, not this wiring's to bury.
pub(crate) fn wire_permission_memory(
    memory: &PermissionMemory,
    transcript: &TranscriptHandle,
) -> crate::session::PermissionsRestore {
    let launch_saver = transcript
        .saver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let restored = launch_saver.load_permissions();
    let damaged = restored.damage_stands();
    let (allow, deny) = restored.clone().into_lists();
    memory.adopt(allow, deny, damaged);
    let persist_cell = Arc::clone(&transcript.saver);
    memory.persist_through(Arc::new(move |allow, deny, damaged| {
        let saver = persist_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Err(why) = saver.save_permissions(allow, deny, damaged) {
            tracing::warn!(error = %why, "remembered permissions could not be saved");
        }
    }));
    restored
}

/// Fail a damaged permissions restore closed, on the mode cell.
///
/// The damage already emptied what the caller adopted — no list to
/// restore — so the one lever left is the posture: where the session
/// would run `auto`, the posture that never asks, the cell moves to
/// `interactive`, and every side-effecting call prompts until the
/// session's answers re-establish a memory (the explicit recovery).
/// A stricter posture stays as it stands — read-only and interactive
/// already gate everything the damage could have hidden — and the
/// answer says whether the move happened, for the notice that names
/// it. The move alone is half the fallback: the caller must also
/// project the config rules ([`tighten_config_rules_cell`]) and name
/// the enforced mode in the retained config, or a configured `allow`
/// would cover the ask and a failed rebuild would restore auto.
pub(crate) fn fail_permissions_restore_closed(cell: &dch_config::SharedPermissionMode) -> bool {
    let mut guard = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *guard == dch_config::PermissionMode::Auto {
        *guard = dch_config::PermissionMode::Interactive;
        return true;
    }
    false
}

/// Project the config-rules cell onto its tightening half.
///
/// The damaged-restore fallback's companion to the mode move: the
/// engine consults the config layer ahead of the mode matrix, and a
/// configured `allow` would cover the asking posture the cell moved
/// to — a damaged session under a config that allows a verb would
/// run that verb unasked, with the remembered refusals that might
/// have balanced it gone. The projection drops the `allow` rules
/// (the loosening half) and keeps `deny` and `ask` (the tightening
/// half, which never made anything run), so every side-effecting
/// call asks until the verdict the projection was keyed to clears:
/// `/permissions repair` naming the recovery — a `/preset` keyed
/// while the verdict stands having recorded its selection without
/// joining, so the repair seeds the layer at the preset the session
/// chose — or a fresh adoption undoing the projection
/// ([`reseed_config_rules_cell`]). The cell is the live view the
/// policy re-reads per evaluation, so the projection reaches the very
/// next dispatch and survives every runner rebuild.
pub(crate) fn tighten_config_rules_cell(
    cell: &std::sync::Arc<std::sync::Mutex<Vec<dch_permissions::Rule>>>,
) {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|rule| rule.outcome != dch_permissions::PermissionOutcome::Allow);
}

/// Re-seed the config-rules cell from the configuration's own join at
/// the selection in effect.
///
/// The recovery half of the damage posture and the exact call
/// `/permissions repair` makes: the cell returns to the rules the
/// configuration compiles today at the live preset selection — the
/// same join that seeded it at startup, carried on the lanes so a
/// mid-session `/preset` survives the re-seed instead of regressing
/// to the boot selection. Every fresh adoption clears the damage
/// verdict [`tighten_config_rules_cell`] was keyed to, so each
/// re-seeds the cell beside the adoption: the earned-fresh arm of a
/// `/clear` and the healthy arm of a resume both leave a posture that
/// belongs to no standing session, the configuration in force
/// exactly as written and as selected.
pub(crate) fn reseed_config_rules_cell(
    cell: &std::sync::Arc<std::sync::Mutex<Vec<dch_permissions::Rule>>>,
    config: &dch_config::DchConfig,
    preset: Option<&str>,
) {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone_from(&dch_loop::config_rules_at(config, preset));
}

/// Fail a damaged permissions restore closed at startup, on every
/// lever the session owns.
///
/// The whole of the fallback, in the order the guarantee needs: the
/// mode cell moves off the never-asking posture
/// ([`fail_permissions_restore_closed`]), the retained driver config
/// names the enforced mode — a later failed `/mode` rebuild rolls
/// back to this value, never to the standing auto — the config-rules
/// cell is projected onto its tightening half so no configured allow
/// covers the ask, the display is told the enforced mode, and the
/// damage is said as a transient notice: the announcement is a
/// status, not conversation history — the durable record is the
/// marker inside the permissions file, and `/permissions` restates
/// the posture on demand. Each lever matters alone: the move without
/// the projection would let a config allow run what the notice says
/// asks, and the move without the retained config would let one
/// failed rebuild restore auto.
pub(crate) fn fail_permission_restore_closed_at_startup(
    app: &mut dch_tui::TuiApp,
    driver_config: &mut dch_config::DchConfig,
    lanes: &InteractiveLanes,
    session_tx: &mpsc::UnboundedSender<SessionUpdate>,
) {
    if fail_permissions_restore_closed(&lanes.mode_cell) {
        driver_config.runner.permission_mode = dch_config::PermissionMode::Interactive;
    }
    tighten_config_rules_cell(&lanes.config_rules_cell);
    let enforced = *lanes
        .mode_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if session_tx
        .send(SessionUpdate::PermissionMode { mode: enforced })
        .is_err()
    {
        tracing::warn!("damage posture published but the display is gone");
    }
    app.post_notice(
        "the saved permissions are damaged — remembered rules were dropped; \
         side effects ask, and the configured rules stay tightened, until \
         /permissions repair (the damage marker rides inside the permissions \
         file — hand-fixing the file leaves it standing)"
            .to_string(),
    );
}

/// Post a fresh session's boot warning, when one was earned, as the
/// transcript's opening system row.
///
/// Lifted from the launch path so
/// [`launch_identity`](crate::tui::session::launch_identity) reads as one
/// shape: the resumed half seeds messages wholesale, the fresh half
/// opens with its warning, if any, and nothing else.
pub(crate) fn report_fresh_warn(app: &mut dch_tui::TuiApp, warn: Option<String>) {
    if let Some(warn) = warn {
        app.push_message(dch_tui::TuiMessage::System {
            text: warn,
            timestamp: chrono::Utc::now(),
        });
    }
}

/// Bridge the runner's permission gate to the app's overlay.
///
/// Returns the typed resolver the runner's `Ask` cells consult and the
/// receiver end of the channel the resolver hands requests to. Every
/// ask that reaches here is fresh: the gate consults the session's
/// remembered approvals itself, per dispatch, and answers a covered ask
/// on its own — so the resolver re-derives from the ask's raw input the
/// rules an "always" answer would record (the same pure derivation the
/// gate computes, deterministic, so nothing needs to travel beside the
/// ask), ships the request to the UI, and awaits the typed answer. A
/// dropped request or a closed channel reads as a denial, and the gate
/// itself races the run's cancel signal so a cancelled prompt never
/// waits on the UI.
fn permission_bridge() -> (
    std::sync::Arc<dyn loopctl::middleware::PermissionAskResolver>,
    mpsc::UnboundedReceiver<PermissionRequest>,
) {
    let (requests_tx, requests_rx) = mpsc::unbounded_channel();
    let resolver: std::sync::Arc<dyn loopctl::middleware::PermissionAskResolver> =
        Arc::new(PanelBridge { requests_tx });
    (resolver, requests_rx)
}

/// The typed permission resolver: loopctl's ask in, the panel's
/// request out.
///
/// The rules each press would record are derived from the ask's raw
/// input at answer time — [`dch_permissions::remember_rules`] and
/// [`dch_permissions::deny_rules`] are pure and deterministic, so
/// these derivations are exactly the ones the gate's evaluation would
/// have computed — grounded on the ask's own working directory, the
/// base the gate spelled the call's write targets on. Both sides mint
/// every *writer* segment — readers mint nothing on either side of
/// the sheet, the carve-out runs them free on their own — and the
/// deny side is the narrow derivation, so a refusal bans the work,
/// not the pipe beside it.
struct PanelBridge {
    /// The channel the panel's requests travel on.
    ///
    /// Each ask becomes a [`PermissionRequest`] carrying a oneshot
    /// reply, and the resolver awaits that reply: a send that fails
    /// (the panel is gone) and a dropped reply alike resolve as
    /// [`loopctl::middleware::PermissionResolution::DenyOnce`], so a
    /// dead panel can only ever deny.
    requests_tx: mpsc::UnboundedSender<PermissionRequest>,
}

impl loopctl::middleware::PermissionAskResolver for PanelBridge {
    fn resolve<'a>(
        &'a self,
        ask: &'a loopctl::middleware::PermissionAsk,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = loopctl::middleware::PermissionResolution> + Send + 'a,
        >,
    > {
        let always =
            dch_permissions::remember_rules(&ask.tool_name, &ask.input, Path::new(&ask.cwd));
        let never = dch_permissions::deny_rules(&ask.tool_name, &ask.input, Path::new(&ask.cwd));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let sent = self.requests_tx.send(PermissionRequest {
            tool_name: ask.tool_name.clone(),
            prompt: ask.prompt.clone(),
            input: ask.input.clone(),
            always: tui_rules(&always),
            never: tui_rules(&never),
            reply: reply_tx,
        });
        Box::pin(async move {
            if sent.is_ok() {
                reply_rx
                    .await
                    .unwrap_or(loopctl::middleware::PermissionResolution::DenyOnce)
            } else {
                loopctl::middleware::PermissionResolution::DenyOnce
            }
        })
    }
}

/// Translate the gate's remember-rules into the panel's rule records.
///
/// The two types say the same thing on either side of the crate
/// boundary: the gate's rules are the wire format, the panel's are
/// the session's own state — and this is the one place they meet.
fn tui_rules(rules: &dch_loop::permission::RememberRules) -> Vec<PermissionRule> {
    match rules {
        dch_loop::permission::RememberRules::Tool(tool) => {
            vec![PermissionRule::Tool { tool: tool.clone() }]
        }
        dch_loop::permission::RememberRules::Segments { prefixes, texts } => {
            let mut out: Vec<PermissionRule> = prefixes
                .iter()
                .map(|prefix| PermissionRule::ShellPrefix {
                    prefix: prefix.clone(),
                })
                .collect();
            out.extend(
                texts
                    .iter()
                    .map(|text| PermissionRule::CommandText { text: text.clone() }),
            );
            out
        }
        dch_loop::permission::RememberRules::PathGlobs(patterns) => patterns
            .iter()
            .map(|pattern| PermissionRule::PathGlob {
                pattern: pattern.clone(),
            })
            .collect(),
    }
}

/// The session's interactive lanes: the permission gate and the ask
/// tool's channel.
///
/// Both lanes must outlive any single runner. A `/resume` or a
/// cross-provider `/model` rebuilds the runner on a fresh
/// [`RunnerContext`](dch_loop::Runner::context), and the two host
/// axes a rebuild cannot carry through the builder are exactly these:
/// the panel holds their receivers — handed over once, at startup —
/// and a rebuild that built its own bridge would strand the resolver
/// on a closed channel (every ask silently denying) and leave the new
/// context with no question channel at all. Carrying the pair here
/// keeps one bridge per session: the resolver the panel answers and
/// the prompt channel the sheet renders are the same objects every
/// rebuild hands to the new runner.
#[derive(Clone)]
pub(crate) struct InteractiveLanes {
    /// The typed resolver the permission middleware asks through.
    ///
    /// The same clone the first runner was built with; every rebuild
    /// receives it again, so the panel's receiver — held since
    /// startup — stays the live one.
    pub(crate) permission: std::sync::Arc<dyn loopctl::middleware::PermissionAskResolver>,

    /// The channel ask-tool prompts arrive on, overlay-shaped.
    ///
    /// The sender half of the pair the panel's question sheet owns;
    /// each rebuild's supervision task forwards the new context's
    /// prompts through the same [`overlay_prompt`] rewrite before
    /// they travel here.
    prompt_tx: mpsc::UnboundedSender<QuestionPrompt>,

    /// The rewrite of a tool prompt into the display's overlay type.
    ///
    /// The same [`overlay_prompt`] the startup supervision installs,
    /// so a rebuilt runner's prompts render in the identical shape.
    overlay: fn(dch_loop::PromptedQuestion) -> QuestionPrompt,

    /// The session's live permission mode.
    ///
    /// Every runner build shares this one cell — the gate re-reads it
    /// per dispatch — so the mode moves with `/mode` even while a run
    /// is in flight, and every rebuild (a `/model` switch, a resume)
    /// carries the session's current mode forward instead of
    /// resurrecting the boot config's.
    pub(crate) mode_cell: dch_config::SharedPermissionMode,

    /// The session's remembered approvals: the memory the sheet's
    /// "allow always" records into, the resumed file adopts, and the
    /// gate's snapshot consults per dispatch.
    ///
    /// Held on the lanes so every runner build shares one memory —
    /// the snapshot closure the builder receives reads it live, so a
    /// remember made one call ago covers the next, across rebuilds as
    /// across turns.
    pub(crate) memory: PermissionMemory,

    /// The session's live config-rules cell: `/preset` swaps the
    /// contents, and the gate's snapshot re-reads per dispatch.
    pub(crate) config_rules_cell: std::sync::Arc<std::sync::Mutex<Vec<dch_permissions::Rule>>>,

    /// The session's live preset selection: `/preset` records it, and
    /// the fresh adoptions re-seed the config-rules cell from it.
    ///
    /// Held on the lanes because the selection is process state the
    /// app owns and the driver reads back: a `/clear` rotation and a
    /// healthy resume both re-seed the cell from the configuration
    /// compiled at this selection, so a mid-session swap cannot be
    /// reverted by an adoption that only knows the boot config's.
    pub(crate) preset_cell: std::sync::Arc<std::sync::Mutex<Option<String>>>,

    /// The adoption guard: one lock the driver's permission adoptions
    /// and the panel's `/preset` swap both hold across their whole
    /// motions, so the two can never interleave.
    ///
    /// The race it closes: the swap's damage check and rules write
    /// shared no lock with a live adoption, so a damaged resume
    /// landing between them installed its tightened rules after the
    /// swap's healthy check — and the swap's configured allows then
    /// overwrote the tightening while the verdict stood. Every path
    /// that carries the guard takes it before the preset and rules
    /// locks, so it cannot deadlock against them.
    pub(crate) adoption_guard: std::sync::Arc<std::sync::Mutex<()>>,
}

impl InteractiveLanes {
    /// The session's bridges, before any runner exists to supervise.
    ///
    /// Both channels, the resolver, and the session's permission
    /// memory in one call: the panel's receivers and the memory come
    /// back beside the lanes that will carry the sender halves
    /// through every rebuild — the wiring a session does exactly once,
    /// at startup.
    pub(crate) fn bridges(
        config: &dch_config::DchConfig,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<PermissionRequest>,
        mpsc::UnboundedReceiver<QuestionPrompt>,
        PermissionMemory,
    ) {
        let memory = PermissionMemory::default();
        let config_rules_cell =
            std::sync::Arc::new(std::sync::Mutex::new(dch_loop::config_rules(config)));
        let preset_cell =
            std::sync::Arc::new(std::sync::Mutex::new(config.permissions.preset.clone()));
        let (permission, permission_rx) = permission_bridge();
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        (
            Self {
                permission,
                prompt_tx,
                overlay: overlay_prompt,
                mode_cell: std::sync::Arc::new(std::sync::Mutex::new(
                    config.runner.permission_mode,
                )),
                memory: memory.clone(),
                config_rules_cell,
                preset_cell,
                adoption_guard: std::sync::Arc::new(std::sync::Mutex::new(())),
            },
            permission_rx,
            prompt_rx,
            memory,
        )
    }

    /// The config rules as a live view the gate consults per dispatch.
    ///
    /// The closure re-reads the shared cell the panel swaps on a
    /// `/preset`, so the very next dispatch evaluates under the new
    /// join — nothing is captured at build time, and caching the
    /// result would break that promise.
    pub(crate) fn config_rules_view(&self) -> dch_loop::permission::ConfigRulesView {
        let cell = Arc::clone(&self.config_rules_cell);
        Arc::new(move || {
            cell.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
    }

    /// The remembered-approvals snapshot the gate consults per dispatch.
    ///
    /// The closure reads the memory live — the same object the sheet's
    /// "allow always" records into and the resumed file adopts — and
    /// projects it into the engine's vocabulary: a path-glob rule the
    /// file carries that no longer compiles is skipped with a warning,
    /// so one unreadable rule must not silence the rest of the memory.
    pub(crate) fn memory_snapshot(&self) -> dch_loop::MemorySnapshot {
        // The memory shares its rule lists through internal `Arc`s, so the
        // clone is the cheap handle the closure captures; the projection
        // runs per evaluation on the live cell.
        let memory = self.memory.clone();
        Arc::new(move || memory.snapshot())
    }

    /// Install the first ask supervision on the session's first
    /// runner.
    ///
    /// The startup half of the ask lane; every rebuild installs its
    /// own generation through [`attach`](Self::attach). The handle
    /// comes back for the exit path's abort.
    pub(crate) fn open(&self, runner: &dch_loop::Runner) -> tokio::task::JoinHandle<()> {
        runner
            .question_path()
            .supervise(self.prompt_tx.clone(), self.overlay)
    }

    /// Attach both lanes to a freshly built runner.
    ///
    /// The permission side needs no attach — the resolver rides the
    /// builder itself — so this installs only the question
    /// supervision: a new generation on the new runner's context,
    /// forwarding its prompts to the panel through the session's one
    /// channel. The previous generation parks on the context it still
    /// holds an `Arc` to — nothing dispatches through it after the swap,
    /// so no prompt can be stolen — and settles when the session's
    /// channels close at exit; one parked generation accrues per
    /// user-triggered rebuild, bounded and cleared by the session's
    /// end.
    pub(crate) fn attach(&self, runner: &dch_loop::Runner) {
        let _generation = runner
            .question_path()
            .supervise(self.prompt_tx.clone(), self.overlay);
    }

    /// Lanes whose panel halves are gone.
    ///
    /// A test seam for driver contexts that never exercise the
    /// interactive paths: the receivers are dropped on the spot, so a
    /// permission ask would deny and a question prompt would end the
    /// supervision — exactly the behaviors a test that stays off the
    /// lanes wants, and nothing a real session may build.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        let (permission, _rx) = permission_bridge();
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
        Self {
            permission,
            prompt_tx,
            overlay: overlay_prompt,
            mode_cell: std::sync::Arc::new(std::sync::Mutex::new(
                dch_config::PermissionMode::default(),
            )),
            memory: PermissionMemory::default(),
            config_rules_cell: std::sync::Arc::new(std::sync::Mutex::new(dch_loop::config_rules(
                &dch_config::DchConfig::default(),
            ))),
            preset_cell: std::sync::Arc::new(std::sync::Mutex::new(None)),
            adoption_guard: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }
}

/// Rewrite one asking-path prompt into the app's overlay type.
///
/// The mechanical field-for-field mapping between the composition
/// layer's prompt and the display's own type; the reply one-shot
/// rides through untouched, so an answer reaches the asking tool
/// without this mapping holding anything open.
fn overlay_prompt(prompted: dch_loop::PromptedQuestion) -> QuestionPrompt {
    QuestionPrompt {
        question: prompted.question,
        header: prompted.header,
        options: prompted
            .options
            .into_iter()
            .map(|option| dch_tui::QuestionChoice {
                label: option.label,
                description: option.description,
            })
            .collect(),
        multi_select: prompted.multi_select,
        reply: prompted.reply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use crate::headless::resolve_session_config_pair;
    use crate::tui::test_support::*;
    use crate::tui::transcript::MachineSnapshot;
    use crate::tui::transcript::TranscriptWorker;
    use clap::Parser as _;
    use dch_tui::PermissionMemory;
    use dch_tui::PermissionRule;
    use dch_tui::SessionUpdate;
    use dch_tui::TuiApp;
    use dch_tui::TuiMessage;
    use dch_tui::TuiObserverState;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    #[test]
    fn a_flag_preset_selection_reaches_the_interactive_rules_cell() {
        // The startup chain the interactive mode stands on: the flag
        // is a per-run statement, so the boot resolution folds it
        // into the live config — the same fold the headless mode
        // makes — and the lanes the session builds read that folded
        // config, so the gate's rules cell carries the preset's
        // rules from the first dispatch. A wiring that re-read the
        // file instead would silently run the file's own selection.
        let tmp = tempfile::TempDir::new().expect("config tempdir");
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "[permissions]\n",
                "rules = [{ id = \"base-deny\", outcome = \"deny\", tool = \"WebFetch\" }]\n",
                "[permissions.presets.rust-dev]\n",
                "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
            ),
        )
        .expect("the config file lands");
        let args = Args::try_parse_from([
            "dch",
            "--config",
            path.to_str().expect("a utf-8 tempdir path"),
            "--permissions-preset",
            "rust-dev",
        ])
        .expect("the flags parse");
        let control = crate::resume::ResumeControl::Fresh {
            session_id: None,
            warn: None,
        };
        let (config, _base, _file) =
            resolve_session_config_pair(&args, &control).expect("the selection resolves");
        assert_eq!(
            config.permissions.preset.as_deref(),
            Some("rust-dev"),
            "the flag's selection is the boot config's own"
        );
        let (lanes, _permission_rx, _prompt_rx, _memory) = InteractiveLanes::bridges(&config);
        let rules = lanes
            .config_rules_cell
            .lock()
            .expect("rules cell lock")
            .clone();
        assert!(
            rules.iter().any(|rule| rule.id == "preset.rust-dev.rule-0"),
            "the preset's rules join the cell the gate consults: {rules:?}"
        );
        assert!(
            rules.iter().any(|rule| rule.id == "base-deny"),
            "the top-level rules stand ahead of the preset's"
        );
    }

    #[test]
    fn a_crash_between_the_pair_writes_degrades_to_derive_not_half_adoption() {
        let dir = tempfile::tempdir().expect("session tempdir");
        let root = dir.keep();
        let id = uuid::Uuid::new_v4();
        let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
            id,
            "m".to_string(),
            root.clone(),
        ));
        let (handle, worker) = TranscriptWorker::spawn(Arc::clone(&saver));
        handle.send(
            vec![TuiMessage::User {
                text: "turn one".to_string(),
                timestamp: chrono::Utc::now(),
                steered: false,
                unsent: false,
            }],
            crate::session::SessionTokens::default(),
        );
        worker.join();
        let session_at = crate::session::load_with_meta_in(id, &root)
            .unwrap()
            .saved_at;
        assert!(
            crate::session::load_machine_in(id, &root, session_at).is_none(),
            "a display-only save never adopts a machine half it did not write"
        );

        let (handle, worker) = TranscriptWorker::spawn(saver);
        handle.send_machine(MachineSnapshot {
            messages: Arc::new(Vec::new()),
            saved_at: chrono::Utc::now(),
            compactions: 0,
            saver: None,
        });
        worker.join();
        let session_at = crate::session::load_with_meta_in(id, &root)
            .unwrap()
            .saved_at;
        assert!(
            crate::session::load_machine_in(id, &root, session_at).is_some(),
            "the machine half landing after restores the pair"
        );
    }

    #[tokio::test]
    async fn a_fresh_ask_crosses_carrying_what_always_would_remember() {
        // The resolver's contract after coverage moved into the gate:
        // every ask that arrives is fresh — the gate answered the covered
        // ones itself — so the resolver always ships a request, deriving
        // from the ask's raw input what an "always" answer would record.
        // Nothing is stashed anywhere: the derivation is pure, and the
        // typed answer rides back as a PermissionResolution.
        let (resolver, mut requests_rx) = permission_bridge();
        let ask = loopctl::middleware::PermissionAsk::new(
            "Bash",
            "Allow 'Bash' (ShellExecute) in Interactive mode?",
            serde_json::json!({ "command": "cargo build" }),
            "call_1",
            3,
            ".",
        );
        let pending = resolver.resolve(&ask);
        let request = requests_rx.recv().await.expect("a fresh ask crosses");
        assert_eq!(
            request.prompt, "Allow 'Bash' (ShellExecute) in Interactive mode?",
            "the request carries the ask's prompt"
        );
        assert_eq!(
            request.input,
            serde_json::json!({ "command": "cargo build" }),
            "the request carries the raw call input"
        );
        assert_eq!(
            request.always,
            vec![PermissionRule::ShellPrefix {
                prefix: "cargo build".to_string()
            }],
            "the request carries what an always-answer would remember"
        );
        assert_eq!(
            request.never,
            vec![PermissionRule::ShellPrefix {
                prefix: "cargo build".to_string()
            }],
            "a clean non-reader ask denies by the same verb prefix"
        );
        let _replied = request
            .reply
            .send(loopctl::middleware::PermissionResolution::AllowOnce);
        assert_eq!(
            pending.await,
            loopctl::middleware::PermissionResolution::AllowOnce,
            "the panel's answer resolves the ask"
        );

        let ask = loopctl::middleware::PermissionAsk::new(
            "Write",
            "Allow 'Write' (FileWrite) in Interactive mode?",
            serde_json::json!({ "file_path": "README.md" }),
            "call_2",
            4,
            ".",
        );
        let pending = resolver.resolve(&ask);
        let request = requests_rx.recv().await.expect("the write ask crosses");
        assert_eq!(
            request.always,
            vec![PermissionRule::PathGlob {
                pattern: "README.md".to_string()
            }],
            "a write ask carries its directory scope"
        );
        assert_eq!(
            request.never, request.always,
            "a write refusal mints the same scope the approval would"
        );
        let _replied = request
            .reply
            .send(loopctl::middleware::PermissionResolution::DenyOnce);
        assert_eq!(
            pending.await,
            loopctl::middleware::PermissionResolution::DenyOnce,
            "a refusal resolves the ask as a denial"
        );

        let ask = loopctl::middleware::PermissionAsk::new(
            "Jobs",
            "Allow 'Jobs' (Meta) in Interactive mode?",
            serde_json::json!({ "operation": "cleanup_jobs" }),
            "call_3",
            5,
            ".",
        );
        let pending = resolver.resolve(&ask);
        let request = requests_rx.recv().await.expect("a tool ask still crosses");
        assert_eq!(
            request.always,
            vec![PermissionRule::Tool {
                tool: "Jobs".to_string()
            }],
            "a tool ask remembers the tool itself"
        );
        assert_eq!(
            request.never, request.always,
            "a tool refusal mints the tool"
        );
        let _replied = request
            .reply
            .send(loopctl::middleware::PermissionResolution::AllowOnce);
        assert_eq!(
            pending.await,
            loopctl::middleware::PermissionResolution::AllowOnce,
            "the panel's answer resolves the ask"
        );
    }

    #[tokio::test]
    async fn the_deny_derivation_is_narrow_at_the_seam() {
        // The narrow-deny pin on the bridge — a chain mixing reader
        // and writer segments: the same ask ships the writer verb as
        // what an `a` press records — readers mint nothing, on either
        // side of the sheet — and only the writer verb as what a `d`
        // press records. `tail` is never banned by association; the
        // redirect rides the scrub like any
        // allow derivation sees it.
        let (resolver, mut requests_rx) = permission_bridge();
        let ask = loopctl::middleware::PermissionAsk::new(
            "Bash",
            "Allow 'Bash' (ShellExecute) in Interactive mode?",
            serde_json::json!({
                "command": "cargo build --workspace --quiet 2>&1 | tail -30"
            }),
            "call_live",
            7,
            "/work",
        );
        let pending = resolver.resolve(&ask);
        let request = requests_rx.recv().await.expect("the live-case ask crosses");
        assert_eq!(
            request.always,
            vec![PermissionRule::ShellPrefix {
                prefix: "cargo build".to_string()
            },],
            "the allow derivation mints every *writer* segment — the reader \
             tail is part of no answer, on either side of the sheet"
        );
        assert_eq!(
            request.never,
            vec![PermissionRule::ShellPrefix {
                prefix: "cargo build".to_string()
            }],
            "the deny derivation mints the writer alone"
        );
        let _replied = request
            .reply
            .send(loopctl::middleware::PermissionResolution::DenyOnce);
        assert_eq!(
            pending.await,
            loopctl::middleware::PermissionResolution::DenyOnce,
            "the panel's refusal resolves the ask"
        );
    }

    #[tokio::test]
    async fn the_overlay_prompt_carries_every_prompt_field_and_the_reply_channel() {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let prompt = dch_loop::PromptedQuestion {
            question: "Ship it?".to_string(),
            header: Some("Ship".to_string()),
            options: vec![dch_loop::PromptOption {
                label: "yes".to_string(),
                description: Some("now, at 3am".to_string()),
            }],
            multi_select: true,
            reply: reply_tx,
        };

        let overlay = overlay_prompt(prompt);

        assert_eq!(overlay.question, "Ship it?");
        assert_eq!(overlay.header.as_deref(), Some("Ship"));
        assert_eq!(overlay.options.len(), 1);
        assert_eq!(overlay.options[0].label, "yes");
        assert_eq!(
            overlay.options[0].description.as_deref(),
            Some("now, at 3am")
        );
        assert!(overlay.multi_select);
        overlay
            .reply
            .send(vec!["yes".to_string()])
            .expect("the asking path awaits the reply");
        assert_eq!(
            reply_rx.await.expect("the reply channel rode through"),
            vec!["yes".to_string()]
        );
    }

    #[tokio::test]
    async fn the_bridge_delivers_requests_and_replies() {
        let (resolver, mut requests_rx) = permission_bridge();
        let ask = loopctl::middleware::PermissionAsk::new(
            "Bash",
            "Allow 'Bash' (ShellExecute) in Interactive mode?",
            serde_json::json!({ "command": "make ci" }),
            "call_probe",
            0,
            ".",
        );
        let pending = resolver.resolve(&ask);
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), requests_rx.recv())
            .await
            .expect("the request arrives")
            .expect("the channel is open");
        assert_eq!(request.tool_name, "Bash");
        assert!(
            request.prompt.contains("Bash") && request.prompt.contains("Interactive"),
            "the request carries the gate's prompt: {}",
            request.prompt
        );
        request
            .reply
            .send(loopctl::middleware::PermissionResolution::AllowOnce)
            .expect("the reply channel is live");
        let answered = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .expect("the resolver future settles once replied");
        assert_eq!(
            answered,
            loopctl::middleware::PermissionResolution::AllowOnce,
            "the panel's allow resolves the ask"
        );
    }

    #[tokio::test]
    async fn a_dropped_request_resolves_as_a_denial() {
        let (resolver, mut requests_rx) = permission_bridge();
        let ask = loopctl::middleware::PermissionAsk::new(
            "Bash",
            "Allow 'Bash' (ShellExecute) in Interactive mode?",
            serde_json::json!({ "command": "cargo build" }),
            "call_probe",
            0,
            ".",
        );
        let pending = resolver.resolve(&ask);
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), requests_rx.recv())
            .await
            .expect("the request arrives")
            .expect("the channel is open");
        drop(request);
        let answered = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .expect("a dropped request still settles the resolver");
        assert_eq!(
            answered,
            loopctl::middleware::PermissionResolution::DenyOnce,
            "an unanswered request reads as a denial"
        );
    }

    #[tokio::test]
    async fn a_cancelled_ask_leaves_no_state_behind() {
        // The stale-entry pin, now structural: the old stash kept an
        // entry per unanswered ask, bounded at 256 — there is no stash
        // to leak anymore. A dropped request (the cancel race winning)
        // settles the resolver as a denial, and nothing is recorded:
        // the memory the sheet shares is untouched by the cancelled ask.
        let (resolver, mut requests_rx) = permission_bridge();
        let memory = PermissionMemory::default();
        let ask = loopctl::middleware::PermissionAsk::new(
            "Bash",
            "Allow 'Bash' (ShellExecute) in Interactive mode?",
            serde_json::json!({ "command": "cargo build" }),
            "call_cancelled",
            0,
            ".",
        );
        let pending = resolver.resolve(&ask);
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), requests_rx.recv())
            .await
            .expect("the request arrives")
            .expect("the channel is open");
        drop(request); // the cancel race wins: the sheet never answers
        let answered = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .expect("the cancelled ask still settles");
        assert_eq!(
            answered,
            loopctl::middleware::PermissionResolution::DenyOnce,
            "a cancelled ask denies"
        );
        assert!(
            memory.allow_rules().is_empty() && memory.deny_rules().is_empty(),
            "a cancelled ask recorded nothing — and there is no stash to hold a stale entry"
        );
    }

    #[tokio::test]
    async fn a_rebuilt_runner_keeps_both_interactive_lanes() {
        // What a rebuild must never lose: the ask tool's channel and
        // the permission gate. The lanes a session carries attach a
        // fresh runner's question supervision to the panel's own
        // channel, and the resolver every rebuild builds with is the
        // one the panel has held since startup — so an ask through the
        // rebuilt context reaches the panel, not a closed channel's
        // silent denial.
        let (permission, mut permission_rx) = permission_bridge();
        let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel();
        let lanes = InteractiveLanes {
            permission: Arc::clone(&permission),
            prompt_tx,
            overlay: overlay_prompt,
            mode_cell: std::sync::Arc::new(std::sync::Mutex::new(
                dch_config::PermissionMode::default(),
            )),
            memory: PermissionMemory::default(),
            config_rules_cell: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            preset_cell: std::sync::Arc::new(std::sync::Mutex::new(None)),
            adoption_guard: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        let rebuilt = unreachable_runner().await;
        lanes.attach(&rebuilt);

        // The ask lane: a question through the rebuilt runner's
        // context lands on the panel's channel, overlay-shaped.
        let sender = rebuilt
            .context()
            .question_tx
            .lock()
            .expect("slot lock")
            .clone()
            .expect("attach installed a live generation");
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        sender
            .send(dch_tools::QuestionRequest {
                questions: vec![dch_tools::Question {
                    question: "still asking?".to_string(),
                    header: None,
                    options: Vec::new(),
                    multi_select: false,
                    response_tx,
                }],
            })
            .expect("the installed channel takes the question");
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), prompt_rx.recv())
            .await
            .expect("the panel's channel carries the rebuilt runner's prompt")
            .expect("the supervision stays alive");
        assert_eq!(prompt.question, "still asking?");

        // The permission lane: the resolver a rebuild reuses reaches
        // the panel's receiver and returns its verdict.
        let ask = loopctl::middleware::PermissionAsk::new(
            "Bash",
            "may I run this?",
            serde_json::json!({ "command": "cargo build" }),
            "call_rebuild",
            0,
            ".",
        );
        let verdict = permission.resolve(&ask);
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), permission_rx.recv())
            .await
            .expect("the panel's permission channel carries the ask")
            .expect("the bridge stays alive");
        assert_eq!(request.tool_name, "Bash");
        request
            .reply
            .send(loopctl::middleware::PermissionResolution::AllowOnce)
            .expect("the panel answers while the resolver waits");
        assert_eq!(
            verdict.await,
            loopctl::middleware::PermissionResolution::AllowOnce,
            "the verdict the panel sent is the verdict the gate reads"
        );
    }

    #[test]
    fn the_damage_projection_keeps_only_tightening_rules() {
        // The projection half of the damaged fallback: allow rules go
        // — they would cover the ask the mode move promises — and deny
        // and ask stay, because they never made anything run and
        // losing a deny would loosen.
        let cell = std::sync::Arc::new(std::sync::Mutex::new(vec![
            dch_permissions::Rule {
                id: "allow-bash".to_string(),
                outcome: dch_permissions::PermissionOutcome::Allow,
                matcher: dch_permissions::Matcher::Tool("Bash".to_string()),
            },
            dch_permissions::Rule {
                id: "ask-fetch".to_string(),
                outcome: dch_permissions::PermissionOutcome::Ask,
                matcher: dch_permissions::Matcher::Tool("WebFetch".to_string()),
            },
            dch_permissions::Rule {
                id: "deny-env".to_string(),
                outcome: dch_permissions::PermissionOutcome::Block,
                matcher: dch_permissions::Matcher::Tool("Write".to_string()),
            },
        ]));
        tighten_config_rules_cell(&cell);
        let remaining = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            remaining
                .iter()
                .map(|rule| rule.id.clone())
                .collect::<Vec<_>>(),
            vec!["ask-fetch".to_string(), "deny-env".to_string()],
            "the loosening half goes; the tightening half stays"
        );
    }

    #[test]
    fn the_startup_damage_fallback_moves_every_lever() {
        // The startup fallback, each lever pinned alone: the cell moves
        // off auto, the retained driver config names the enforced mode
        // (a later failed rebuild rolls back to it, never auto), the
        // config-rules cell loses its allows, and the display is told
        // the enforced mode over the session lane.
        let (observer, kept) = TuiObserverState::new().into_observer();
        drop(observer);
        let mut app = TuiApp::from_observer_state(unreachable_config(), kept);
        let mut driver_config = unreachable_config();
        let lanes = InteractiveLanes::detached();
        let mode_cell = Arc::clone(&lanes.mode_cell);
        *lanes
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
        let (session_tx, mut session_rx) = mpsc::unbounded_channel();
        fail_permission_restore_closed_at_startup(
            &mut app,
            &mut driver_config,
            &lanes,
            &session_tx,
        );
        assert_eq!(
            *mode_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Interactive,
            "the cell leaves the never-asking posture"
        );
        assert_eq!(
            driver_config.runner.permission_mode,
            dch_config::PermissionMode::Interactive,
            "the retained config names the enforced mode — a failed rebuild \
             cannot roll back to auto"
        );
        let remaining = lanes
            .config_rules_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            remaining
                .iter()
                .all(|rule| rule.outcome != dch_permissions::PermissionOutcome::Allow),
            "no configured allow survives to cover the ask: {remaining:?}"
        );
        assert_eq!(
            session_rx.try_recv(),
            Ok(SessionUpdate::PermissionMode {
                mode: dch_config::PermissionMode::Interactive
            }),
            "the display is told the enforced mode"
        );
        assert!(
            app.conversation()
                .iter()
                .all(|message| !matches!(message, dch_tui::TuiMessage::System { text, .. } if text.contains("damaged"))),
            "the announcement is a transient notice, not a transcript row — nothing \
             accumulates across reopens; the durable record is the file's marker"
        );
    }

    #[test]
    fn a_damaged_restore_moves_only_an_auto_cell_to_asking() {
        // The helper's whole contract: `auto` — the posture that never
        // asks — moves to the asking posture and says so; a stricter
        // posture already gates everything damage could have hidden and
        // stays exactly where it stands.
        let auto: dch_config::SharedPermissionMode =
            std::sync::Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::Auto));
        assert!(
            fail_permissions_restore_closed(&auto),
            "the auto cell moves"
        );
        assert_eq!(
            *auto
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::Interactive,
            "the move lands on the asking posture"
        );
        let read_only: dch_config::SharedPermissionMode =
            std::sync::Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::ReadOnly));
        assert!(
            !fail_permissions_restore_closed(&read_only),
            "a stricter cell does not move"
        );
        assert_eq!(
            *read_only
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::ReadOnly,
            "read-only stays read-only"
        );
    }

    /// Steers through the app's seam at a turn's start, from inside the run.
    ///
    /// The loop emits `on_turn_start` after it has drained its steering slot
    /// for that turn and before it sends that turn's request, so a steer
    /// offered here provably missed the boundary that could have read it.
    /// A poll from the test task cannot establish that while the run opens:
    /// a steer accepted that early is still ahead of the first drain and
    /// rides the first request.
    struct TurnStartSteer {
        /// The app's seam, attached once the built runner hands out its slot.
        guidance: std::sync::Mutex<Option<Arc<dyn dch_tui::Guidance>>>,
        /// The text the observer steers.
        text: &'static str,
        /// Whether the run in flight accepted the steer.
        accepted: AtomicBool,
    }

    impl TurnStartSteer {
        /// A steer holding `text`, with no seam attached yet.
        fn new(text: &'static str) -> Arc<Self> {
            Arc::new(Self {
                guidance: std::sync::Mutex::new(None),
                text,
                accepted: AtomicBool::new(false),
            })
        }

        /// Attach the seam the observer steers through.
        fn attach(&self, guidance: Arc<dyn dch_tui::Guidance>) {
            *self.guidance.lock().expect("the seam lock") = Some(guidance);
        }

        /// Whether the run in flight took the steer.
        fn accepted(&self) -> bool {
            self.accepted.load(Ordering::SeqCst)
        }
    }

    impl loopctl::observer::LoopObserver for TurnStartSteer {
        fn name(&self) -> &'static str {
            "turn-start-steer"
        }

        fn on_turn_start(&self, ctx: &loopctl::observer::TurnStartContext) {
            if ctx.turn > 0 || self.accepted() {
                return;
            }
            let offered = self
                .guidance
                .lock()
                .expect("the seam lock")
                .as_ref()
                .is_some_and(|guidance| guidance.steer(self.text.to_string()));
            if offered {
                self.accepted.store(true, Ordering::SeqCst);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_host_seam_reaches_a_running_loop_and_gives_back_what_it_missed() {
        // The response is held far longer than the test needs, so the run is
        // provably in flight around the steer. This is the only test that
        // crosses the whole host path — app trait object, adapter, engine
        // slot, live loop — and it is the one that earns the translation:
        // text in, engine message out, and what no run delivered back out as
        // text again. The steer is placed by the loop's own turn event, so
        // the run's never having read it is a fact of the run's shape rather
        // than of the machine's speed. A sentinel rides the text because the
        // request body carries the system prompt and the tool schemas, where
        // ordinary words appear by coincidence.
        let server = SseServer::start_with_delay(
            vec![sse_text_turn("too late")],
            std::time::Duration::from_secs(30),
        )
        .await;
        let dir = tempfile::tempdir().expect("temp workdir");
        let steer = TurnStartSteer::new("the report is in the other directory, host-3j8f");
        let runner = dch_loop::Runner::builder(&sse_config(server.port), dir.path())
            .with_observer(Arc::clone(&steer) as Arc<dyn loopctl::observer::LoopObserver>)
            .build()
            .await
            .expect("runner construction");
        let cancel = runner.cancel_signal();
        // Installed exactly as the app holds it: a shared trait object, which
        // is also the pin that the seam stays object-safe.
        let seam: Arc<dyn dch_tui::Guidance> = Arc::new(LoopGuidance {
            steering: runner.steering(),
        });
        steer.attach(Arc::clone(&seam));
        let task = tokio::spawn(async move {
            let mut runner = runner;
            drop(runner.run("go").await);
        });

        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !steer.accepted() || server.request_count() < 1 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the seam reached the loop while its request was in flight");

        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("the cancelled run settles promptly")
            .expect("the driving task does not panic");

        let bodies = server.bodies();
        assert_eq!(bodies.len(), 1, "one request was in flight: {bodies:?}");
        assert!(
            !bodies[0].contains("host-3j8f"),
            "a steer behind the drain cannot retroactively enter the request \
             its turn sent: {bodies:?}"
        );
        assert_eq!(
            seam.take_returned(),
            ["the report is in the other directory, host-3j8f"],
            "what the run never read comes back through the adapter as text — \
             including the guidance the ended run left in its live queue, not \
             only what an earlier run sealed away"
        );
        assert!(
            seam.take_returned().is_empty(),
            "taking it empties the slot, so a later sweep cannot show it twice"
        );
    }

    #[test]
    fn an_always_answer_for_a_quoted_verb_phrases_the_exact_command() {
        // The sheet's phrase is the user's record of what the approval
        // covers. A quoted first word mints no verb prefix — no
        // `every future run` wording exists for the placeholder — so
        // the ask's rules carry the exact command and the phrase says
        // so.
        let minted = dch_permissions::remember_rules(
            "Bash",
            &serde_json::json!({ "command": "'cat' notes.txt" }),
            std::path::Path::new("/work"),
        );
        let rules = tui_rules(&minted);
        assert_eq!(
            rules,
            vec![PermissionRule::CommandText {
                text: "'cat' notes.txt".to_string()
            }]
        );
        assert!(
            rules
                .iter()
                .all(|rule| rule.remember_phrase() == "the exact command"),
            "no phrase names a placeholder run"
        );
    }
}
