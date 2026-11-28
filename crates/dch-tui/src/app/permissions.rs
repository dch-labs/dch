//! The permission sheet's application path: the `/permissions`
//! command, the panel's key handling, and the remembered-rule
//! plumbing the overlay's listing drives.

use super::TuiApp;

use super::keys::key_survives_the_sheet;
use super::notices::wrap_prompt_rows;
use crate::cells::{for_display, for_review};
use crate::message::TuiMessage;
use crate::permission::PermissionRequest;
use crate::queue::{PermissionRule, SessionCommand};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use loopctl::middleware::PermissionResolution;
use ratatui::Frame;
use ratatui::layout::Rect;

impl TuiApp {
    /// `/permissions` — the rules in force, the way to drop one or
    /// all, and the damage marker's one door out.
    ///
    /// Bare lists configured and remembered rules — config and
    /// preset rows first, remembered allows then denies — numbered
    /// for forget. `forget <n>` takes the numbered
    /// remembered rule out of the memory and out of the session's
    /// permissions file in one motion, so that work asks again the next
    /// time; `forget all` is the same motion over every remembered rule
    /// at once. A configured rule is the config file's own text and says
    /// so instead of pretending it can be dropped here. `repair` clears
    /// a standing damage verdict — the fail-closed posture a damaged
    /// restore armed — and re-seeds the config rules as configured.
    pub(crate) fn permissions_command(&mut self, forget: Option<&str>) {
        let Some(requested) = forget else {
            self.post_permissions_listing();
            return;
        };
        if requested.trim() == "repair" {
            self.repair_permission_damage();
            return;
        }
        let Some(number) = requested.trim().strip_prefix("forget ") else {
            self.post_notice(
                "usage: /permissions [forget <n> | forget all | repair] — n is a remembered \
                 rule's number from the listing"
                    .to_string(),
            );
            return;
        };
        if number.trim() == "all" {
            let remembered = self
                .permission
                .permission_memory
                .allow_rules()
                .len()
                .saturating_add(self.permission.permission_memory.deny_rules().len());
            if remembered == 0 {
                self.post_notice(
                    "nothing remembered to forget — /permissions lists what is recorded"
                        .to_string(),
                );
                return;
            }
            self.permission.permission_memory.forget_all();
            self.push_message(TuiMessage::System {
                text: format!(
                    "Forgot all {remembered} remembered rules — that work asks again \
                     the next time."
                ),
                timestamp: chrono::Utc::now(),
            });
            return;
        }
        let Ok(shown) = number.trim().parse::<usize>() else {
            self.post_notice(format!(
                "{number:?} is not a rule number — /permissions lists what is recorded"
            ));
            return;
        };
        if shown == 0 {
            self.post_notice(format!(
                "no remembered rule {shown} — /permissions lists what is recorded"
            ));
            return;
        }
        let configured = self.permission.permission_listing_configured;
        if shown <= configured {
            self.post_notice(format!(
                "rule {shown} is configured — change it in the config file, not here"
            ));
            return;
        }
        let Some((was_allow, rendered)) = self
            .permission
            .rendered_permission_rules
            .get(shown.saturating_sub(configured.saturating_add(1)))
            .cloned()
        else {
            self.post_notice(format!(
                "no remembered rule {shown} — /permissions lists what is recorded"
            ));
            return;
        };
        let Some(removed) = self
            .permission
            .permission_memory
            .forget_matching(was_allow, &rendered)
        else {
            self.post_notice(format!(
                "rule {shown} is no longer remembered as rendered — the remembered \
                 list moved; run /permissions to list what is recorded now"
            ));
            return;
        };
        self.push_message(TuiMessage::System {
            text: format!(
                "Forgot remembered permission — {} asks again the next time.",
                removed.remember_phrase()
            ),
            timestamp: chrono::Utc::now(),
        });
    }

    /// The damage row a `/permissions` listing leads with while the
    /// verdict stands.
    ///
    /// The listing's own restatement of the posture — inspection
    /// output, shown on demand and never pushed by the session's own
    /// motion: the launch and adoption announcements are transient
    /// notices (a status, not conversation history), the durable
    /// record is the marker inside the permissions file, and this row
    /// names both the posture and the one command that clears it.
    fn push_permission_damage_row(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.push_message(TuiMessage::System {
            text: "the saved permissions are damaged — remembered rules were dropped; \
                   side effects ask, and the configured rules stay tightened, until \
                   /permissions repair (the damage marker rides inside the permissions \
                   file — hand-fixing the file leaves it standing)"
                .to_string(),
            timestamp: now,
        });
    }

    /// `/permissions repair` — the standing damage verdict's one
    /// door out of the session that earned it.
    ///
    /// A damaged restore arms a posture — the asking mode, the
    /// tightened config rules, the marker that re-arms both on every
    /// launch and rides every save — and nothing the run does clears
    /// it: not the re-answers, which record real choices without
    /// resurrecting the refusals the damage lost. Only the wholesale
    /// endings do — a `/clear` rotation and a resume into a healthy
    /// session adopt a fresh memory, and the driver re-seeds the
    /// config-rules cell beside each, because a posture belongs to
    /// the session that earned it. Within the damaged session, the
    /// repair is the explicit recovery the notices name: the marker
    /// clears, and the config-rules cell re-seeds from the
    /// configuration's own join, so the configured allows return
    /// exactly as written. The mode stays where it stands — the
    /// user's to move with `/mode` as ever, because a posture is not
    /// a config value.
    ///
    /// The whole repair runs under the driver's adoption guard — the
    /// same lock a live resume or `/clear` adoption holds across its
    /// whole motion — and every check inside reads the memory the
    /// guard holds stable: the damage verdict, the listed state, and
    /// the session identity the listing rendered under. A repair owes
    /// its writes to a listing the user actually read: without one,
    /// or with a memory the listing did not show — rules moved, an
    /// adoption landed — or with a session the listing did not
    /// render, it refuses and names the re-read, so a concurrent
    /// adoption can never borrow a review that belonged to another
    /// session to clear a posture its own session never earned.
    fn repair_permission_damage(&mut self) {
        let adoption = self.permission.permission_adoption_guard.clone();
        let _held = adoption.as_ref().map(|guard| {
            guard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if !self.permission.permission_memory.damage_standing() {
            self.post_notice("nothing to repair — the saved permissions read whole".to_string());
            return;
        }
        let Some(listed) = self.permission.permission_listed_state.clone() else {
            self.post_notice(
                "run /permissions to review the standing damage before repairing".to_string(),
            );
            return;
        };
        let session_listed = self.permission.permission_listing_session.clone();
        if listed != Self::listed_permission_state(&self.permission.permission_memory)
            || session_listed != self.host.session_id
        {
            self.post_notice(
                "the permissions changed since the listing — run /permissions again".to_string(),
            );
            return;
        }
        self.permission.permission_memory.clear_damage();
        let rules = self.config.permissions.engine_rules();
        if let Some(cell) = &self.permission.permission_config_rules_cell {
            cell.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone_from(&rules);
        }
        self.post_notice(
            "repaired — the configured permission rules are in force again".to_string(),
        );
    }

    /// Post the `/permissions` listing: configured rules, then the
    /// session's remembered ones, numbered for `forget`.
    ///
    /// The config file's rules render first, the selected preset's
    /// rules follow — each labeled with its preset — and the
    /// remembered allows and denies come last as their remember
    /// phrases, numbered in one continuing sequence so
    /// `/permissions forget <n>` addresses any remembered row. Each
    /// section posts as its own system row under a header that names
    /// where its rules live. With nothing in force anywhere the
    /// listing says so instead, and a standing damage verdict leads
    /// the rows either way, so the tightened posture they are read
    /// under is never silent.
    fn post_permissions_listing(&mut self) {
        self.permission.permission_listed_state = Some(Self::listed_permission_state(
            &self.permission.permission_memory,
        ));
        self.permission.permission_listing_session = self.host.session_id.clone();
        let now = chrono::Utc::now();
        let configured: Vec<String> = self
            .config
            .permissions
            .rules
            .iter()
            .map(dch_config::PermissionRuleConfig::summary_line)
            .collect();
        let selected_preset = self.config.permissions.preset.clone();
        let mut preset_lines: Vec<String> = Vec::new();
        if let Some(name) = &selected_preset
            && let Some(preset) = self.config.permissions.presets.get(name)
        {
            for rule in &preset.rules {
                preset_lines.push(format!("{} — from preset {name}", rule.summary_line()));
            }
        }
        let allow_snapshot = self.permission.permission_memory.allow_rules();
        let deny_snapshot = self.permission.permission_memory.deny_rules();
        self.permission.rendered_permission_rules = allow_snapshot
            .iter()
            .cloned()
            .map(|rule| (true, rule))
            .chain(deny_snapshot.iter().cloned().map(|rule| (false, rule)))
            .collect();
        self.permission.permission_listing_configured =
            configured.len().saturating_add(preset_lines.len());
        let allowed: Vec<String> = allow_snapshot
            .iter()
            .map(PermissionRule::remember_phrase)
            .collect();
        let denied: Vec<String> = deny_snapshot
            .iter()
            .map(PermissionRule::remember_phrase)
            .collect();
        if configured.is_empty()
            && preset_lines.is_empty()
            && allowed.is_empty()
            && denied.is_empty()
        {
            if self.permission.permission_memory.damage_standing() {
                self.push_permission_damage_row(now);
            } else {
                self.push_message(TuiMessage::System {
                    text: "no permission rules in force — an allow-always or deny-always \
                           answer records one, and [permissions] in the config pre-seeds more"
                        .to_string(),
                    timestamp: now,
                });
            }
            return;
        }
        if self.permission.permission_memory.damage_standing() {
            self.push_permission_damage_row(now);
        }
        let configured_lines: Vec<String> = configured
            .iter()
            .cloned()
            .chain(preset_lines.iter().cloned())
            .collect();
        if !configured_lines.is_empty() {
            let header = match &selected_preset {
                Some(name) => format!(
                    "Permission rules configured (the config file's rules and the \
                     selected preset \"{name}\" — change them there):"
                ),
                None => "Permission rules configured (in the config file — change them \
                       there):"
                    .to_string(),
            };
            self.push_message(TuiMessage::System {
                text: header,
                timestamp: now,
            });
            for (position, line) in (1..=configured_lines.len()).zip(&configured_lines) {
                self.push_message(TuiMessage::System {
                    text: format!("  {position}. {line}"),
                    timestamp: now,
                });
            }
        }
        let allow_base = configured_lines.len();
        let list_section = |app: &mut Self, header: &str, items: &[String], offset: usize| {
            if items.is_empty() {
                return;
            }
            app.push_message(TuiMessage::System {
                text: header.to_string(),
                timestamp: now,
            });
            for (position, phrase) in (offset.checked_add(1).unwrap_or(0)
                ..=offset.checked_add(items.len()).unwrap_or(0))
                .zip(items)
            {
                app.push_message(TuiMessage::System {
                    text: format!("  {position}. {phrase}"),
                    timestamp: now,
                });
            }
        };
        list_section(
            self,
            "Always allowed this session — /permissions forget <n> drops one:",
            &allowed,
            allow_base,
        );
        let deny_base = allow_base.checked_add(allowed.len()).unwrap_or(0);
        list_section(
            self,
            "Always denied this session — /permissions forget <n> drops one:",
            &denied,
            deny_base,
        );
    }

    /// The permission state a review sees: the remembered rules and
    /// the damage verdict together.
    ///
    /// The listing records it and the repair re-reads it under the
    /// adoption guard; equality is what binds the repair to the
    /// session the user reviewed.
    fn listed_permission_state(
        memory: &crate::queue::PermissionMemory,
    ) -> (Vec<PermissionRule>, Vec<PermissionRule>, bool) {
        (
            memory.allow_rules(),
            memory.deny_rules(),
            memory.damage_standing(),
        )
    }

    /// Move the live permission mode: the shared cell and the
    /// driver's rebuild, from the config the panel holds.
    ///
    /// `/mode` and `/unset permission_mode` share the whole live path:
    /// the cell the gate re-reads per dispatch flips on the spot — the
    /// flip buys the in-flight turn's next dispatch the new mode — and
    /// the driver rebuilds the loop under the mode the config now
    /// names, a rebuild whose failure is the driver's to undo, from
    /// the loop's own config. Skipping any half of it leaves the
    /// display naming one mode while the gate enforces another.
    pub(crate) fn move_permission_mode_live(&mut self) {
        if let Some(cell) = self.permission.permission_mode_cell.as_ref() {
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                self.config.runner.permission_mode;
        }
        self.send_command(SessionCommand::Mode {
            mode: self.config.runner.permission_mode,
        });
    }

    /// Install the session's remembered-permission memory.
    ///
    /// The host's wiring: the same object the permission resolver
    /// consults, so an "allow always" answer the panel records is
    /// honored by the very next ask — before any file round-trip.
    pub fn set_permission_memory(&mut self, memory: crate::queue::PermissionMemory) {
        self.permission.permission_memory = memory;
    }

    /// Install the session's live permission-mode cell.
    ///
    /// The host's wiring: the same cell the runner's gate re-reads on
    /// every dispatch, so `/mode` flips the mode the loop enforces on
    /// the spot — mid-run included.
    pub fn set_permission_mode_cell(&mut self, mode: Option<dch_config::SharedPermissionMode>) {
        self.permission.permission_mode_cell = mode;
    }

    /// Install the live config-rules cell the loop's policy re-reads
    /// per dispatch.
    ///
    /// The host's wiring: `/preset` swaps the cell's contents, and the
    /// gate evaluates the new join on the very next dispatch.
    pub fn set_permission_config_rules_cell(
        &mut self,
        cell: Option<std::sync::Arc<std::sync::Mutex<Vec<dch_permissions::Rule>>>>,
    ) {
        self.permission.permission_config_rules_cell = cell;
    }

    /// Install the live preset-selection cell the driver's fresh
    /// adoptions re-seed from.
    ///
    /// The host's wiring: `/preset` records the selection, and the
    /// next `/clear` or healthy resume compiles the config layer at
    /// it — a live swap cannot be reverted by an adoption that only
    /// knows the boot config.
    pub fn set_permission_preset_cell(
        &mut self,
        cell: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    ) {
        self.permission.permission_preset_cell = cell;
    }

    /// Install the adoption guard the driver's permission adoptions
    /// hold, so a `/preset` swap serializes against them.
    ///
    /// The host's wiring: the swap's damage check and rules write run
    /// under the same lock a resume or `/clear` adoption holds across
    /// its whole motion — verdict, mode, rules — closing the window
    /// where a damaged adoption lands between the check and the write
    /// and the swap's configured allows overwrite the tightening it
    /// installed. The `/preset` swap takes the guard before the preset
    /// and rules locks, matching the driver's order, so the two
    /// cannot deadlock either.
    pub fn set_permission_adoption_guard(
        &mut self,
        guard: Option<std::sync::Arc<std::sync::Mutex<()>>>,
    ) {
        self.permission.permission_adoption_guard = guard;
    }

    /// Install the channel permission requests arrive on.
    ///
    /// The receiver half of the bridge the host builds around the
    /// runner's permission resolver; once set, pending asks render as
    /// an overlay and the run loop claims the channel into its select.
    pub fn set_permission_requests(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.permission.permission_rx = Some(rx);
    }

    /// Land one delivered request and any others already waiting
    /// behind it.
    ///
    /// The request enters the queue and the receiver drains, so a
    /// burst of ready asks lands together and the first frame's
    /// overlay reports the true waiting count instead of admitting
    /// them one loop iteration at a time. The integration suites and
    /// the in-crate tests stage their bursts through here; the run
    /// loop lands its own the same way, through the permission state
    /// and the receiver it keeps in a local.
    pub fn land_permission_requests(
        &mut self,
        first: PermissionRequest,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.permission.land_permission_requests(first, rx);
    }

    /// Refresh the permission queue without the run loop.
    ///
    /// The test seam the integration suites drive to stage asks; the
    /// pass itself is the permission state's own
    /// (`PermissionState::poll_permission_requests`). The run loop
    /// lands its bursts through the permission state instead, from the
    /// receiver it keeps in a local — once `run` has claimed the
    /// channel, this is a no-op.
    pub fn poll_permission_requests(&mut self) {
        self.permission.poll_permission_requests();
    }
}

/// The permission sheet's state: the asks waiting, the reason or
/// preview a sheet carries mid-answer, and the remembered-rule
/// plumbing the listing drives.
///
/// Everything the interactive permission flow keeps between frames,
/// in one place — the queue of pending asks with the channel they
/// arrive on, the memory "allow always" answers record into, the
/// rendered listing the `forget` keys read back, and the live cells
/// a mode or preset swap moves. The host-facing installers stay on
/// [`TuiApp`]; this is the state they install into.
#[derive(Default)]
pub(crate) struct PermissionState {
    /// Permission asks waiting for the user's answer, oldest first.
    ///
    /// The gate resolves `Ask` cells by handing the UI one request per
    /// pending tool call; the front of the queue is what the overlay
    /// shows and what a keypress answers. A finished or cancelled run
    /// empties it — the gate has already denied anything unanswered
    /// against the cancel signal.
    pub(crate) pending_permissions: std::collections::VecDeque<PermissionRequest>,

    /// The reason being typed for the front permission ask, when the
    /// sheet's `r` press opened the reason line.
    ///
    /// `None` is the sheet's ordinary posture — `y`/`a`/`d`/`n` answer
    /// directly. `Some(buffer)` is the reason-entry mode: the sheet
    /// owns every keystroke, Enter resolves the ask with
    /// [`DenyWithFeedback`](loopctl::middleware::PermissionResolution::DenyWithFeedback)
    /// (a trimmed-empty buffer plain-denies), and Esc returns to the
    /// ordinary sheet with nothing resolved.
    pub(crate) permission_reason: Option<String>,

    /// The channel permission requests arrive on, until `run` claims
    /// it.
    ///
    /// Installed by the host before the run loop starts; the loop's
    /// select arm moves requests into the queue, so between runs the
    /// slot simply holds the receiver.
    pub(crate) permission_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>>,

    /// The session's remembered permission prompts.
    ///
    /// The shared "allow always" memory: the same object the
    /// permission resolver consults, so an answer the panel records
    /// is honored by the very next ask. `Default` until the host
    /// installs the session's own — asking still works, remembering
    /// just goes nowhere the resolver can see.
    pub(crate) permission_memory: crate::queue::PermissionMemory,

    /// The remembered rules as the last `/permissions` listing
    /// rendered them, in listing order — allow rules first, deny
    /// after.
    ///
    /// `forget <n>` is keyed to what the user read, not to whatever
    /// occupies the numbered position when the press lands: the
    /// memory can move under a rendered listing — a queued
    /// `/resume`'s adopt replaces the pair wholesale — so the press
    /// matches the rendered rule's identity and, when nothing
    /// matches, says the listing moved instead of removing a
    /// stranger.
    pub(crate) rendered_permission_rules: Vec<(bool, crate::queue::PermissionRule)>,

    /// The configured-rule count the last `/permissions` listing
    /// numbered its remembered rules past.
    ///
    /// `forget <n>` keys to what the user read, so the boundary
    /// between configured and remembered rules is the listing's own
    /// count, snapshotted here — a `/preset` that changes the
    /// configured set between the listing and the press must not
    /// re-number the rows under the number the user is about to name,
    /// which would hand the press a stranger to remove.
    pub(crate) permission_listing_configured: usize,

    /// The permission mode and preset selection this session started
    /// under.
    ///
    /// The stand-in a config save writes without a wired file
    /// restores these — the boot-time pair, flag- or file-stated — so
    /// a `/mode` or `/preset` swap rides no save into a file it
    /// promised never to touch, while an initial `--permission-mode`
    /// or `--permissions-preset` also rides no save *out* of one.
    pub(crate) permission_session_start: (dch_config::PermissionMode, Option<String>),

    /// The permission state the last `/permissions` listing rendered.
    ///
    /// The review record the repair binds to: the remembered rules
    /// and the damage verdict as the user read them. A repair that
    /// finds the live state moved — an adoption landed, an answer
    /// remembered a rule — refuses rather than act on a session the
    /// user never reviewed. `None` until a listing runs in this
    /// session.
    pub(crate) permission_listed_state: Option<(Vec<PermissionRule>, Vec<PermissionRule>, bool)>,

    /// The session whose permissions the last `/permissions` listing
    /// rendered.
    ///
    /// The identity half of the review record: adoptions can land a
    /// memory whose rules read exactly as the listing did while
    /// belonging to a session the user never reviewed, so the repair
    /// binds to the session id beside the state — an id that moved
    /// with an adoption refuses exactly as moved rules do. `None`
    /// until a listing runs in this session.
    pub(crate) permission_listing_session: Option<String>,

    /// Whether this render's visibility has been spent by a resolve.
    ///
    /// An approval must answer a request the user has seen. The
    /// event loop drains a burst of keypresses before the next
    /// redraw, so the first resolving press of a burst spends the
    /// visibility the render granted and every later press is
    /// swallowed until a frame draws the ask that stepped forward —
    /// a double-press can never resolve two requests when only the
    /// first was on screen. The render clears the flag when it draws
    /// the front ask; every resolution sets it.
    pub(crate) permission_spent: std::cell::Cell<bool>,

    /// Whether the permission sheet shows the command preview
    /// expanded.
    ///
    /// `e` on the sheet toggles it: the collapsed sheet shows the
    /// command's first rows, the expanded sheet its last — together
    /// the two windows cover the command, and the destructive suffix
    /// a head-only preview could hide is always one press away. The
    /// toggle resets with the ask it read.
    pub(crate) permission_command_expanded: bool,

    /// The session's live permission mode, when the host wired one.
    ///
    /// The same cell the runner's gate re-reads on every dispatch;
    /// `/mode` flips it in place, so a run in flight picks the new
    /// mode up on its very next tool call. `None` in tests and
    /// headless constructors — the loop command still carries the
    /// move between turns.
    pub(crate) permission_mode_cell: Option<dch_config::SharedPermissionMode>,

    /// The live config-rules cell the loop's policy re-reads per
    /// dispatch, mirroring
    /// [`permission_mode_cell`](Self::permission_mode_cell).
    ///
    /// `/preset` swaps the contents on the spot; a host without a
    /// lane (headless constructors) leaves it `None` and the loop
    /// command still carries the selection between turns.
    pub(crate) permission_config_rules_cell:
        Option<std::sync::Arc<std::sync::Mutex<Vec<dch_permissions::Rule>>>>,

    /// The live preset-selection cell the driver's fresh adoptions
    /// re-seed from, mirroring
    /// [`permission_config_rules_cell`](Self::permission_config_rules_cell).
    ///
    /// `/preset` records the selection here beside swapping the rules
    /// cell, so a `/clear` rotation or a healthy resume compiles the
    /// config layer at the selection in effect rather than the boot
    /// config's own. A host without a lane leaves it `None` and the
    /// selection rides the session config alone.
    pub(crate) permission_preset_cell: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,

    /// The adoption guard the driver holds across its whole permission
    /// adoptions, mirroring
    /// [`permission_preset_cell`](Self::permission_preset_cell).
    ///
    /// `/preset` takes the same lock across its damage check and
    /// rules write, so a live adoption — a resume landing damaged, a
    /// `/clear` rotation — can never interleave with the swap: the
    /// tightening an adoption installs cannot be overwritten by a
    /// swap whose healthy check ran before it. A host without a lane
    /// leaves it `None` and the swap stands alone, as it always did.
    pub(crate) permission_adoption_guard: Option<std::sync::Arc<std::sync::Mutex<()>>>,
}

impl PermissionState {
    /// Spend the visibility a landing front inherits.
    ///
    /// A request stepping into an empty queue has never been drawn:
    /// the visibility a render grants belongs to the ask the user
    /// last saw, and an ask landing — first of its burst — must earn
    /// its own frame before any press may answer it. Nothing to do
    /// for a queue that already has a front: the landing changes
    /// nothing the presses read.
    pub(crate) fn spend_unrendered_front(&mut self) {
        if self.pending_permissions.is_empty() {
            self.permission_spent.set(true);
        }
    }

    /// Answer the front permission request, if one is pending.
    ///
    /// Sends `resolution` on its reply channel and drops it from the
    /// queue, clearing any half-typed reason and the command preview's
    /// expansion beside it; a send that finds no receiver (a resolver
    /// already cancelled away) is silently fine — the gate denied
    /// that call on its own.
    fn resolve_pending_permission(&mut self, resolution: PermissionResolution) {
        self.permission_reason = None;
        self.permission_command_expanded = false;
        self.permission_spent.set(true);
        if let Some(front) = self.pending_permissions.pop_front()
            && front.reply.send(resolution).is_err()
        {
            tracing::trace!("permission reply channel already closed");
        }
    }

    /// Apply a key press to a pending permission ask.
    ///
    /// `Some(redraw)` when a request is pending — the overlay owns the
    /// keyboard: plain `y` allows, plain `a` allows and remembers the
    /// ask for the rest of the session, plain `n`/Esc deny, `r` opens
    /// a reason line, `e` expands the command preview to its whole
    /// wrap (and back), and every other key is swallowed — Enter among
    /// them, since a press
    /// queued to submit the draft must never land as an approval — so
    /// nothing reaches the composer, whose control chords would
    /// otherwise submit or edit the hidden draft (Ctrl-Enter and
    /// Ctrl-M submit, Ctrl-W deletes a word).
    /// `None` only for the keys that survive the sheet — Ctrl+Shift+C
    /// (copy) among them. Plain `Ctrl+C` never survives: it is
    /// intercepted below and answers the sheet, denying the ask (the
    /// ask is cancelled, not the session), which the sheet's own pins
    /// hold.
    /// Modifier-decorated answers (Alt+y, Alt+Enter) do not answer:
    /// the documented keys are the plain ones. A resolving press
    /// spends the visibility the last render granted: presses that
    /// trail it inside the same burst — drained before the redraw —
    /// are swallowed until a frame draws the ask that stepped
    /// forward, so a double-press can never resolve a request the
    /// user has not seen. The gate holds the sheet's offers — `y`,
    /// `a`, `d`, `n`, the reason line — and never the interrupt:
    /// Ctrl+C denies the front ask on any frame, seen or not,
    /// because a denial is fail-closed and a user pressing the
    /// interrupt under an ask means one action either way.
    pub(crate) fn permission_key(&mut self, key: KeyEvent) -> Option<bool> {
        self.pending_permissions.front()?;
        let spent = self.permission_spent.get();
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.resolve_pending_permission(PermissionResolution::DenyOnce);
            return Some(true);
        }
        if self.permission_reason.is_some() {
            return Some(self.permission_reason_key(key));
        }
        if key_survives_the_sheet(key) {
            return None;
        }
        let bare = key.modifiers == KeyModifiers::NONE;
        let shift = key.modifiers == KeyModifiers::SHIFT;
        if spent {
            return Some(false);
        }
        match key.code {
            KeyCode::Char('y' | 'Y') if bare || shift => {
                self.resolve_pending_permission(PermissionResolution::AllowOnce);
                Some(true)
            }
            KeyCode::Char('a' | 'A') if bare || shift => {
                if let Some(front) = self.pending_permissions.front() {
                    let always = front.always.clone();
                    let adoption = self.permission_adoption_guard.clone();
                    let _held = adoption.as_ref().map(|guard| {
                        guard
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    });
                    self.permission_memory.remember_rules(&always);
                }
                self.resolve_pending_permission(PermissionResolution::AllowOnce);
                Some(true)
            }
            KeyCode::Char('d' | 'D') if bare || shift => {
                if let Some(front) = self.pending_permissions.front() {
                    let never = front.never.clone();
                    let adoption = self.permission_adoption_guard.clone();
                    let _held = adoption.as_ref().map(|guard| {
                        guard
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    });
                    self.permission_memory.remember_deny(&never);
                }
                self.resolve_pending_permission(PermissionResolution::DenyOnce);
                Some(true)
            }
            KeyCode::Char('n' | 'N') if bare || shift => {
                self.resolve_pending_permission(PermissionResolution::DenyOnce);
                Some(true)
            }
            KeyCode::Char('r' | 'R') if bare || shift => {
                self.permission_reason = Some(String::new());
                Some(true)
            }
            KeyCode::Char('e' | 'E') if bare || shift => {
                self.permission_command_expanded = !self.permission_command_expanded;
                Some(true)
            }
            KeyCode::Esc if bare => {
                self.resolve_pending_permission(PermissionResolution::DenyOnce);
                Some(true)
            }
            _ => Some(false),
        }
    }

    /// One key press in the reason-entry mode of the permission sheet.
    ///
    /// The buffer is the typed text; Enter resolves the ask — the
    /// user's words ride back to the model as the denial reason (a
    /// trimmed-empty buffer is a deliberate plain deny) — and Esc
    /// returns to the ordinary sheet with the ask still pending. Every
    /// other press types or is swallowed: the modal sheet owns the
    /// keyboard, as it does outside the reason line. Every key here is
    /// taken, so the redraw answer is unconditional.
    fn permission_reason_key(&mut self, key: KeyEvent) -> bool {
        let bare = key.modifiers == KeyModifiers::NONE;
        match key.code {
            KeyCode::Esc if bare => {
                self.permission_reason = None;
                true
            }
            KeyCode::Enter if bare => {
                if self.permission_spent.get() {
                    return true;
                }
                let reason = self
                    .permission_reason
                    .take()
                    .map_or_default(|buffer| buffer.trim().to_string());
                let resolution = if reason.is_empty() {
                    PermissionResolution::DenyOnce
                } else {
                    PermissionResolution::DenyWithFeedback(reason)
                };
                self.resolve_pending_permission(resolution);
                true
            }
            KeyCode::Backspace if bare => {
                if let Some(reason) = self.permission_reason.as_mut() {
                    reason.pop();
                }
                true
            }
            KeyCode::Char(ch) if bare || key.modifiers == KeyModifiers::SHIFT => {
                if let Some(reason) = self.permission_reason.as_mut() {
                    reason.push(ch);
                }
                true
            }
            _ => false,
        }
    }

    /// The pending permission ask, as the overlay renders it.
    ///
    /// The sheet carries the ask's prompt, the phrases naming what each
    /// press would actually record — post-skip, the same filter the
    /// record arms apply — the command the ask would run (read from the
    /// raw input, never from the prompt), any reason line being typed,
    /// and how many more asks wait behind it.
    pub(crate) fn shown_permission(&self) -> Option<PermissionSheet> {
        self.permission_spent.set(false);
        self.pending_permissions.front().map(|request| {
            let phrases = |rules: &[PermissionRule]| {
                rules
                    .iter()
                    .map(PermissionRule::remember_phrase)
                    .collect::<Vec<String>>()
            };
            PermissionSheet {
                prompt: for_display(&request.prompt).into_owned(),
                always: phrases(&self.permission_memory.would_record(&request.always, false))
                    .iter()
                    .map(|phrase| for_review(phrase))
                    .collect(),
                never: phrases(&self.permission_memory.would_record(&request.never, true))
                    .iter()
                    .map(|phrase| for_review(phrase))
                    .collect(),
                command: request.command_detail().map(|command| for_review(&command)),
                reason: self
                    .permission_reason
                    .as_deref()
                    .map(|reason| for_display(reason).into_owned()),
                more: self.pending_permissions.len().saturating_sub(1),
            }
        })
    }

    /// Move any waiting permission requests into the queue.
    ///
    /// The test seam for staging asks without the run loop:
    /// production keeps its receiver in a local and lands bursts
    /// through [`land_permission_requests`](Self::land_permission_requests)
    /// instead — once `run` claims the channel, this slot is empty and
    /// the method is a no-op.
    pub(crate) fn poll_permission_requests(&mut self) {
        while let Some(request) = self
            .permission_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.spend_unrendered_front();
            self.pending_permissions.push_back(request);
        }
    }

    /// Land one delivered request and any others already waiting
    /// behind it.
    ///
    /// The run loop's select arm hands the request its wakeup
    /// delivered plus the receiver it owns: the request enters the
    /// queue and the receiver drains, so a burst of ready asks lands
    /// together and the first frame's overlay reports the true
    /// waiting count instead of admitting them one loop iteration at
    /// a time.
    pub(crate) fn land_permission_requests(
        &mut self,
        first: PermissionRequest,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        self.spend_unrendered_front();
        self.pending_permissions.push_back(first);
        while let Ok(request) = rx.try_recv() {
            self.spend_unrendered_front();
            self.pending_permissions.push_back(request);
        }
    }
}
/// The popup's window over its candidates, following the highlight.
///
/// The first rows show until the highlight passes the last of them;
/// from there the highlight pins to the window's last row and the
/// rows above it scroll away — a dropdown's scroll, in a list that
/// floats above the composer. Returns the window's start and the
/// highlighted row's place inside it.
pub(crate) fn popup_window(selected: usize, total: usize, shown_cap: usize) -> (usize, usize) {
    let cap = shown_cap.min(total.max(1));
    let start = selected
        .saturating_sub(cap.saturating_sub(1))
        .min(total.saturating_sub(cap));
    (start, selected.saturating_sub(start))
}

/// The popup's rectangle: the composer's full width, floated beside it
/// as high or as low as the content needs.
///
/// `above` places the popup's bottom against the anchor's top — the
/// docked composer's popup floats into the conversation above it —
/// while `!above` places its top against the anchor's bottom, the
/// welcome's choice: there the field sits under the art, and a popup
/// above would cover the image the session opens on.
pub(crate) fn popup_rect(input_area: Rect, height: u16, above: bool) -> Rect {
    Rect {
        x: input_area.x,
        y: if above {
            input_area.y.saturating_sub(height)
        } else {
            input_area.y.saturating_add(input_area.height)
        },
        width: input_area.width.max(1),
        height,
    }
}

/// The rows a popup may take on its chosen side of the anchor.
///
/// Above-placement clamps to the frame's top, below to its bottom,
/// so a field near either edge sheds popup rows rather than painting
/// past the buffer.
pub(crate) fn popup_bound(frame: &Frame, input_area: Rect, above: bool) -> u16 {
    if above {
        input_area.y
    } else {
        frame
            .area()
            .height
            .saturating_sub(input_area.y.saturating_add(input_area.height))
    }
}

/// The most body rows any one block of the permission overlay shows
/// before ellipsizing.
///
/// The cap keeps the box short enough to stay inside the border on
/// every terminal tall enough to matter; the prompt is a question and
/// the hints are one line, not a document.
pub(crate) const MAX_OVERLAY_ROWS: usize = 3;

/// The most rows the expanded command preview keeps from its wrap's
/// tail — the ceiling for panes tall enough to spend it.
///
/// Expansion exists so the *end* of a command — where a destructive
/// suffix hides — is never invisible at the approval press: the kept
/// rows are the wrap's last, one leading ellipsis row naming the cut,
/// and the cap keeps a pathological command from filling the whole
/// sheet on a terminal that could not scroll it anyway. The collapsed
/// head and the expanded tail together cover any command that fits.
/// The budget a sheet actually spends is derived from its own height
/// ([`expanded_command_budget`]) and clamped to this ceiling; the
/// collapsed path is unchanged.
pub(crate) const MAX_EXPANDED_OVERLAY_ROWS: usize = 40;

/// The sheet's inner height, minus the borders.
///
/// The block draws a rounded border on every side, so two of the
/// sheet's rows are chrome; the content rows live between them.
pub(crate) fn sheet_inner_height(sheet_height: u16) -> usize {
    usize::from(sheet_height.saturating_sub(2))
}

/// The rows the sheet draws besides the prompt and the preview.
///
/// `blocks` are the conditional rows the sheet may draw below the
/// preview — the always-remember scope, the deny-remember scope, the
/// reason draft — handed in as the strings the renderer will wrap, so
/// the budget measures the rows those strings actually occupy, never
/// a one-row guess per block. Each block is counted as the renderer
/// appends it, wrapped to the pane and capped at the overlay's row
/// ceiling, because a long scope or reason on a narrow pane occupies
/// up to three rows; the blank spacer ahead of the hints and the
/// hints themselves are counted the same measured way rather than
/// guessed, because a narrow pane wraps a long hint across several
/// rows too.
pub(crate) fn expanded_fixed_rows(blocks: &[&str], hints: &str, wrap_at: usize) -> usize {
    let rows = blocks
        .iter()
        .map(|block| wrap_prompt_rows(block, wrap_at, MAX_OVERLAY_ROWS).len())
        .fold(0usize, usize::saturating_add);
    let hint_rows = wrap_prompt_rows(hints, wrap_at, MAX_OVERLAY_ROWS).len();
    rows.saturating_add(1).saturating_add(hint_rows)
}

/// The row budget an expanded command preview may spend on this sheet.
///
/// The sheet's inner height, less the prompt rows already drawn and
/// the fixed rows it must still draw, floored at one row so a cramped
/// pane still shows the preview's first line — and capped at the
/// standing ceiling so a tall pane does not hand an enormous command
/// the whole sheet.
pub(crate) fn expanded_command_budget(
    sheet_height: u16,
    prompt_rows: usize,
    fixed_rows: usize,
) -> usize {
    let remaining = sheet_inner_height(sheet_height)
        .saturating_sub(prompt_rows)
        .saturating_sub(fixed_rows);
    remaining.clamp(1, MAX_EXPANDED_OVERLAY_ROWS)
}

/// A full-width rectangle anchored at the bottom of `area`.
///
/// The permission sheet's geometry: it grows upward from just above
/// the status bar over the rows the composer occupies, so the
/// conversation above and the session line below both stay in view.
pub(crate) fn bottom_sheet_rect(height: u16, area: Rect) -> Rect {
    let height = height.min(area.height);
    Rect {
        x: area.x,
        y: area.y.saturating_add(area.height).saturating_sub(height),
        width: area.width,
        height,
    }
}

/// The permission overlay's render inputs, gathered in one pass.
///
/// [`TuiApp::shown_permission`] builds it off the front pending
/// request; [`TuiApp::render_permission_overlay`] draws it.
pub(crate) struct PermissionSheet {
    /// The gate's question — the tool, its category, the mode.
    ///
    /// Display text built from the ask, never an identity: the typed
    /// resolver receives the raw call beside it, so nothing here is
    /// load-bearing for a later decision.
    pub(crate) prompt: String,

    /// What an "allow always" answer would actually record, in the
    /// rules' own phrases — post-skip against the deny list.
    ///
    /// Rendered on the sheet so the user approves exactly the rules
    /// the press writes, not a paraphrase of them.
    pub(crate) always: Vec<String>,

    /// What a "deny always" answer would actually record, in the
    /// rules' own phrases — post-skip against the allow list.
    ///
    /// Same contract as [`Self::always`] for the deny direction: the
    /// sheet shows the words, the press records the words.
    pub(crate) never: Vec<String>,

    /// The command the ask would run, read from the raw call input —
    /// never from the prompt string.
    ///
    /// Some only for shell calls; the overlay shows it under the
    /// display policy — the letters on the sheet are the command
    /// line's own — so the approval names exactly what would run.
    pub(crate) command: Option<String>,

    /// The reason being typed, when the sheet's `r` press opened the
    /// reason line.
    ///
    /// Held here between keystrokes and carried into the denial the
    /// model reads when the reason line submits.
    pub(crate) reason: Option<String>,

    /// How many more asks wait behind this one.
    ///
    /// Zero when this is the only pending request; the overlay shows
    /// the count so a burst of asks reads as a queue, not a loop.
    pub(crate) more: usize,
}

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::commands::{self};

    use crate::queue::PermissionRule;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use loopctl::middleware::PermissionResolution;

    #[test]
    fn retiring_pending_permissions_clears_the_sheet_entry_state() {
        // The sheet's own drafting state retires with the sheet: a
        // half-typed reason or a left-expanded preview would greet the
        // next ask as if the retired sheet's keys had carried over.
        // The ask itself is dropped unanswered — the resolver reads a
        // dropped reply as denial — so only the entry state needs
        // proving here.
        let mut subject = app();
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        subject
            .permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "run it?".to_string(),
                input: serde_json::json!({"command": "cargo test"}),
                always: Vec::new(),
                never: Vec::new(),
                reply: reply_tx,
            });
        subject.permission.permission_reason = Some("because".to_string());
        subject.permission.permission_command_expanded = true;

        let _ = subject.tick_wake(std::time::Instant::now());

        assert!(
            subject.permission.pending_permissions.is_empty(),
            "the run at rest retires the pending ask"
        );
        assert!(
            subject.permission.permission_reason.is_none(),
            "the reason draft retires with the sheet"
        );
        assert!(
            !subject.permission.permission_command_expanded,
            "the preview's expansion retires with the sheet"
        );
    }

    #[test]
    fn a_burst_press_cannot_answer_an_unrendered_ask() {
        // The drain consumes a burst of keypresses before the next
        // redraw: the first `y` resolves the ask on screen, and a
        // second `y` in the same burst arrives before the frame has
        // drawn the ask behind it. That press is swallowed — the
        // model sees no answer for a request the user never saw — and
        // the ask becomes answerable again only once a draw renders
        // it.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let mut replies = Vec::new();
        for command in ["cargo test", "cargo build"] {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            app.permission
                .pending_permissions
                .push_back(crate::permission::PermissionRequest {
                    tool_name: "Bash".to_string(),
                    prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
                    input: serde_json::json!({ "command": command }),
                    always: Vec::new(),
                    never: Vec::new(),
                    reply: reply_tx,
                });
            replies.push(reply_rx);
        }
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(true),
            "the first press answers the rendered ask"
        );
        assert_eq!(
            replies.remove(0).try_recv(),
            Ok(PermissionResolution::AllowOnce),
            "the rendered ask was allowed"
        );
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(false),
            "the burst's second press is swallowed and redraws nothing"
        );
        assert!(
            replies[0].try_recv().is_err(),
            "the unrendered ask carries no answer"
        );
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(true),
            "after its render the ask answers again"
        );
        assert_eq!(
            replies.remove(0).try_recv(),
            Ok(PermissionResolution::AllowOnce),
            "the now-rendered ask was allowed"
        );
    }

    #[test]
    fn a_repair_refuses_when_the_state_moved_since_the_listing() {
        // The repair binds to the review: an adoption that replaced
        // the memory after the listing leaves the user having read a
        // session that no longer stands, and its fail-closed posture
        // is not theirs to clear. The notice names the re-read
        // instead of acting.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let memory = crate::queue::PermissionMemory::default();
        app.set_permission_memory(memory.clone());
        app.permissions_command(None);
        memory.adopt(Vec::new(), Vec::new(), true);
        app.permissions_command(Some("repair"));
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the moved state answers with a notice")
            .clone();
        assert!(
            notice.contains("changed since the listing"),
            "the refusal names the re-read: {notice}"
        );
        assert!(
            memory.damage_standing(),
            "the adopted session's posture stands untouched"
        );
    }

    #[test]
    fn a_repair_before_any_listing_asks_for_the_listing_first() {
        // Damage stands, but no listing has rendered it this session:
        // the repair owes its writes to a review the user actually
        // read, and a launch announcement is not that review. The
        // refusal names the listing, and the posture stands.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let memory = crate::queue::PermissionMemory::default();
        app.set_permission_memory(memory.clone());
        memory.adopt(Vec::new(), Vec::new(), true);
        app.permissions_command(Some("repair"));
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the unreviewed repair answers with a notice")
            .clone();
        assert!(
            notice.contains("review the standing damage"),
            "the refusal names the listing: {notice}"
        );
        assert!(
            memory.damage_standing(),
            "the posture stands until the user has seen it"
        );
    }

    #[test]
    fn a_repair_after_an_adoption_changed_the_session_refuses() {
        // The adoption landed a memory whose state reads exactly as
        // the listing rendered — same rules, same standing damage —
        // under a new session id. The state check alone would bless
        // the repair; the identity check refuses it: the listing the
        // user read was another session's posture, and clearing this
        // one on that review would clear a verdict nobody looked at.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_session_id("session-a".to_string());
        let memory = crate::queue::PermissionMemory::default();
        app.set_permission_memory(memory.clone());
        memory.adopt(Vec::new(), Vec::new(), true);
        app.permissions_command(None);
        app.set_session_id("session-b".to_string());
        app.permissions_command(Some("repair"));
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the refused repair answers with a notice")
            .clone();
        assert!(
            notice.contains("changed since the listing"),
            "the refusal names the re-read: {notice}"
        );
        assert!(
            memory.damage_standing(),
            "the adopted session's posture stands untouched"
        );
    }

    #[test]
    fn a_repair_on_the_listed_session_clears_the_posture() {
        // The door out, in the one case it opens: damage standing, a
        // listing that rendered it under this very session, and no
        // rule moved since. The marker clears and the notice says so.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_session_id("session-a".to_string());
        let memory = crate::queue::PermissionMemory::default();
        app.set_permission_memory(memory.clone());
        memory.adopt(Vec::new(), Vec::new(), true);
        app.permissions_command(None);
        app.permissions_command(Some("repair"));
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the repair answers with a notice")
            .clone();
        assert!(notice.contains("repaired"), "the repair lands: {notice}");
        assert!(!memory.damage_standing(), "the marker cleared");
    }

    #[test]
    fn an_ask_landing_in_a_burst_waits_for_its_frame() {
        // The one hole the render gate left: a request landing into an
        // empty queue inherited whatever visibility the last render
        // granted — on a fresh session, none spent — so a press drained
        // in the same burst could answer an ask no frame had drawn. The
        // landing itself spends the visibility: the ask waits for the
        // frame that draws it before any press may answer, and answers
        // normally once drawn.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let request = crate::permission::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
            input: serde_json::json!({ "command": "cargo test" }),
            always: Vec::new(),
            never: Vec::new(),
            reply: reply_tx,
        };
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.land_permission_requests(request, &mut rx);
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(false),
            "the sheet still owns the press, and nothing it changed asks for a redraw"
        );
        assert!(
            reply_rx.try_recv().is_err(),
            "an ask no frame drew carries no answer"
        );
        assert_eq!(
            app.permission.pending_permissions.len(),
            1,
            "the ask waits for its frame"
        );
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(true),
            "the drawn ask answers"
        );
        assert_eq!(
            reply_rx.try_recv(),
            Ok(PermissionResolution::AllowOnce),
            "the frame's ask was allowed"
        );
    }

    #[test]
    fn a_write_ask_shows_what_always_would_remember() {
        // The `a` press's scope is read before it is pressed: the sheet
        // carries a dim line naming the rules an allow-always would
        // record — for a write ask, the directory scope — and a
        // scopeless ask (nothing to record) shows no such line.
        let mut subject = app();
        subject
            .permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Write".to_string(),
                prompt: "Allow 'Write' (FileWrite) in Interactive mode?".to_string(),
                input: serde_json::json!({ "file_path": "crates/loop/src/lib.rs" }),
                always: vec![PermissionRule::PathGlob {
                    pattern: "crates/**".to_string(),
                }],
                never: vec![PermissionRule::PathGlob {
                    pattern: "crates/**".to_string(),
                }],
                reply: tokio::sync::oneshot::channel().0,
            });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| subject.render(frame)).expect("draw");
        let frame = buffer_text(terminal.backend().buffer());
        assert!(
            frame.contains("a remembers: writes under `crates/**`"),
            "the sheet names the scope the a press records: {frame}"
        );
        let mut bare = app();
        bare.permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
                input: serde_json::json!({ "command": "cargo build" }),
                always: Vec::new(),
                never: Vec::new(),
                reply: tokio::sync::oneshot::channel().0,
            });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| bare.render(frame)).expect("draw");
        let frame = buffer_text(terminal.backend().buffer());
        assert!(
            frame.contains("runs: cargo build"),
            "the sheet renders the command from the raw input: {frame}"
        );
        assert!(
            !frame.contains("a remembers"),
            "nothing to record, no scope line: {frame}"
        );
    }

    #[test]
    fn deny_always_records_the_asks_rules_into_the_deny_list() {
        // The `d` press is the refusal-side memory: the same typed rules
        // the `a` press would record land in the deny list, the call is
        // refused, and plain `n` still records nothing — one denial is
        // one denial, not a standing "never".
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let memory = crate::queue::PermissionMemory::default();
        app.set_permission_memory(memory.clone());
        app.permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
                input: serde_json::json!({ "command": "make ci" }),
                always: vec![PermissionRule::ShellPrefix {
                    prefix: "make ci".to_string(),
                }],
                never: vec![PermissionRule::ShellPrefix {
                    prefix: "make ci".to_string(),
                }],
                reply: tokio::sync::oneshot::channel().0,
            });
        // An approval answers a rendered ask: the pin draws the sheet
        // once so the press finds it answerable.
        let _ = app.permission.shown_permission();
        app.permission
            .permission_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(
            memory.deny_rules(),
            vec![PermissionRule::ShellPrefix {
                prefix: "make ci".to_string()
            }],
            "the deny list carries the verb prefix"
        );
        assert!(
            memory.allow_rules().is_empty(),
            "a deny-always never leaks into the allow list"
        );

        // A plain `n` denies once and records nothing.
        app.permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
                input: serde_json::json!({ "command": "make test" }),
                always: vec![PermissionRule::ShellPrefix {
                    prefix: "make test".to_string(),
                }],
                never: vec![PermissionRule::ShellPrefix {
                    prefix: "make test".to_string(),
                }],
                reply: tokio::sync::oneshot::channel().0,
            });
        // The next ask needs its own render before its own press.
        let _ = app.permission.shown_permission();
        app.permission
            .permission_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(
            memory.deny_rules().len() == 1 && memory.allow_rules().is_empty(),
            "a plain denial records nothing"
        );
    }

    #[test]
    fn the_sheet_renders_both_remember_lines_post_skip() {
        // The sheet tells the truth before the press: both remember
        // lines render the post-skip set — what the press will
        // actually write — not the raw derivation.
        let mut subject = app();
        let memory = crate::queue::PermissionMemory::default();
        memory.remember_rules(&[PermissionRule::ShellPrefix {
            prefix: "tail".to_string(),
        }]);
        subject.set_permission_memory(memory);
        subject
            .permission
            .pending_permissions
            .push_back(crate::permission::PermissionRequest {
                tool_name: "Bash".to_string(),
                prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
                input: serde_json::json!({
                    "command": "cargo build --workspace 2>&1 | tail -30"
                }),
                always: vec![
                    PermissionRule::ShellPrefix {
                        prefix: "cargo build".to_string(),
                    },
                    PermissionRule::ShellPrefix {
                        prefix: "tail".to_string(),
                    },
                ],
                never: vec![
                    PermissionRule::ShellPrefix {
                        prefix: "tail".to_string(),
                    },
                    PermissionRule::ShellPrefix {
                        prefix: "cargo build".to_string(),
                    },
                ],
                reply: tokio::sync::oneshot::channel().0,
            });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| subject.render(frame)).expect("draw");
        let frame = buffer_text(terminal.backend().buffer());
        assert!(
            frame.contains("a remembers: every future `cargo build` run"),
            "the allow line previews both verbs — neither is denied: {frame}"
        );
        assert!(
            frame.contains("d remembers: every future `cargo build` run"),
            "the deny line skips the blessed tail — only the build previews: {frame}"
        );
        assert!(
            !frame.contains("d remembers: every future `cargo build` run ·"),
            "the blessed tail is gone from the deny preview: {frame}"
        );
    }

    #[test]
    fn permissions_lists_and_forgets_the_remembered_rules() {
        type Ledger = Option<(
            Vec<crate::queue::PermissionRule>,
            Vec<crate::queue::PermissionRule>,
        )>;

        // The listing names the config's rules and the session's
        // remembered ones, numbered; `forget <n>` drops the numbered
        // remembered rule through the memory and its persist hook in one
        // motion, so what the listing forgets the file forgets with it.
        // A configured rule is listed read-only — its home is the file.
        let mut app = TuiApp::new(
            dch_config::DchConfig::from_toml_str(
                "[[permissions.rules]]\nid = \"no-env-writes\"\noutcome = \"deny\"\npath_glob = \"**/.env\"\n",
            )
            .expect("the config parses"),
        );
        let memory = crate::queue::PermissionMemory::default();
        let written: std::sync::Arc<std::sync::Mutex<Ledger>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let ledger = std::sync::Arc::clone(&written);
        memory.persist_through(std::sync::Arc::new(move |allow, deny, _damaged: bool| {
            *ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((allow.to_vec(), deny.to_vec()));
        }));
        app.set_permission_memory(memory.clone());
        memory.remember_rules(&[
            crate::queue::PermissionRule::Tool {
                tool: "WebFetch".to_string(),
            },
            crate::queue::PermissionRule::PathGlob {
                pattern: "crates/**".to_string(),
            },
        ]);
        let out = command_output(&mut app, "/permissions");
        assert!(
            out.contains("deny path_glob **/.env"),
            "the config rule lists with its verdict and matcher: {out}"
        );
        assert!(
            out.contains("2. every future WebFetch call"),
            "the allow rules number after the configured ones: {out}"
        );
        assert!(out.contains("3. writes under `crates/**`"), "{out}");
        let out = command_output(&mut app, "/permissions forget 2");
        assert!(
            out.contains("Forgot remembered permission"),
            "the forget says what lapsed: {out}"
        );
        assert_eq!(
            memory.allow_rules(),
            vec![crate::queue::PermissionRule::PathGlob {
                pattern: "crates/**".to_string()
            }],
            "the glob rule the forget did not name stays in the allow list"
        );
        assert_eq!(
            memory.deny_rules(),
            Vec::new(),
            "the forget records no denial"
        );
        let persisted = written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("the forget fired the write-through");
        assert_eq!(
            persisted.0,
            vec![crate::queue::PermissionRule::PathGlob {
                pattern: "crates/**".to_string()
            }],
            "the allow half mirrors the memory"
        );
        assert_eq!(persisted.1, Vec::new(), "the deny half mirrors the memory");
        let out = command_output(&mut app, "/permissions forget 9");
        assert!(
            out.contains("no remembered rule 9"),
            "an out-of-range forget says so: {out}"
        );
    }

    #[test]
    fn permissions_command_parses_bare_and_forget() {
        assert_eq!(
            commands::parse("/permissions"),
            commands::Parsed::Command(commands::Command::Permissions { forget: None })
        );
        assert_eq!(
            commands::parse("/permissions forget 2"),
            commands::Parsed::Command(commands::Command::Permissions {
                forget: Some("forget 2".to_string())
            })
        );
    }

    #[test]
    fn forget_zero_answers_no_such_rule_and_spares_the_first_remembered() {
        // Zero is nobody's number in a list numbered from one: with a
        // remembered rule sitting at position 1, `forget 0` must answer
        // the no-such-rule notice — never reach the remembered index,
        // and never masquerade as a configured rule when none exist.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let memory = crate::queue::PermissionMemory::default();
        memory.remember_rules(&[crate::queue::PermissionRule::ShellPrefix {
            prefix: "cargo test".to_string(),
        }]);
        app.set_permission_memory(memory.clone());
        assert_eq!(
            app.config.permissions.rules,
            Vec::new(),
            "the fresh app carries no configured rules"
        );

        app.permissions_command(Some("forget 0"));

        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the zero ask answers with a notice");
        assert!(
            notice.contains("no remembered rule 0"),
            "the notice says no such rule, not the configured-file wording: {notice}"
        );
        assert_eq!(
            app.permission.permission_memory.allow_rules().len(),
            1,
            "the remembered rule at position 1 is untouched"
        );
        assert_eq!(
            memory.allow_rules(),
            app.permission.permission_memory.allow_rules(),
            "the app's memory and the seeded copy still agree"
        );
    }

    #[test]
    fn allow_always_remembers_the_asks_rules_and_only_the_asks() {
        // The overlay's third answer: `a` allows and records the ask's
        // typed rules — for Bash, one verb prefix per non-reader
        // segment, so every later variant of the same work runs free —
        // while `y` allows without remembering and `n` denies without
        // remembering. The memory is the same object the resolver
        // consults, so the next ask the rules cover never prompts.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let memory = crate::queue::PermissionMemory::default();
        app.set_permission_memory(memory.clone());
        let asks = [
            (
                "Bash",
                "Allow 'Bash' (ShellExecute) in Interactive mode?",
                serde_json::json!({ "command": "cargo test -p dch-tui" }),
                vec![crate::queue::PermissionRule::ShellPrefix {
                    prefix: "cargo test".to_string(),
                }],
                vec![crate::queue::PermissionRule::ShellPrefix {
                    prefix: "cargo test".to_string(),
                }],
            ),
            (
                "Bash",
                "Allow 'Bash' (ShellExecute) in Interactive mode?",
                serde_json::json!({ "command": "rm -rf /tmp/x" }),
                vec![crate::queue::PermissionRule::ShellPrefix {
                    prefix: "rm".to_string(),
                }],
                vec![crate::queue::PermissionRule::ShellPrefix {
                    prefix: "rm".to_string(),
                }],
            ),
            (
                "Write",
                "Allow 'Write' (FileWrite) in Interactive mode?",
                serde_json::json!({ "file_path": "README.md" }),
                vec![crate::queue::PermissionRule::Tool {
                    tool: "Write".to_string(),
                }],
                vec![crate::queue::PermissionRule::Tool {
                    tool: "Write".to_string(),
                }],
            ),
        ];
        let mut replies = Vec::new();
        for (tool, prompt, input, always, never) in asks {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            app.permission
                .pending_permissions
                .push_back(crate::permission::PermissionRequest {
                    tool_name: tool.to_string(),
                    prompt: prompt.to_string(),
                    input,
                    always,
                    never,
                    reply: reply_tx,
                });
            replies.push(reply_rx);
        }
        // Each press answers a rendered ask: the loop staged all three
        // at once, so every press here is preceded by its own draw.
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(true),
            "`a` answers the ask"
        );
        assert_eq!(
            replies.remove(0).try_recv(),
            Ok(PermissionResolution::AllowOnce),
            "`a` allows the call"
        );
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(true)
        );
        assert_eq!(
            replies.remove(0).try_recv(),
            Ok(PermissionResolution::AllowOnce),
            "`y` allows too"
        );
        let _ = app.permission.shown_permission();
        assert_eq!(
            app.permission
                .permission_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(true)
        );
        assert_eq!(
            replies.remove(0).try_recv(),
            Ok(PermissionResolution::DenyOnce),
            "`n` denies"
        );
        let remembered = "cargo test";
        assert_eq!(
            memory.allow_rules(),
            vec![crate::queue::PermissionRule::ShellPrefix {
                prefix: remembered.to_string()
            }],
            "only the `a` answer is remembered — its verb prefix"
        );
        assert!(
            memory.covers_prefixes(&["cargo test".to_string()]),
            "the resolver answers every cargo-test variant on sight"
        );
        assert!(
            !memory.covers_prefixes(&["cargo build".to_string()]),
            "a different verb is a different ask"
        );
    }
}
