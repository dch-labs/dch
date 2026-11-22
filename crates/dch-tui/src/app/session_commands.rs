//! The slash-command application paths: `/help`, `/config`,
//! `/theme`, `/model`, `/resume`, and the rest of the command table's
//! handlers, with the session and model pickers they open.
use crate::cells::for_display;

use super::TuiApp;
use super::copies::copy_via_transports;

use super::keys::key_survives_the_sheet;
use crate::cells::{prefix_within, str_width};
use crate::commands;
use crate::commands::{Command, Parsed};
use crate::config_fields;
use crate::message::TuiMessage;
use crate::queue::{SessionCommand, SessionPick, Submission};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dch_config::DchConfig;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

impl TuiApp {
    /// Dispatch one locally-parsed command into its lane.
    ///
    /// A `/`-prefixed line never reaches the model: a recognised command is
    /// carried out here, an unrecognised one answers with a notice, and the
    /// composer clears either way. Only a line with no leading slash is left
    /// to the ordinary submit path, which the caller drives.
    pub(crate) fn handle_command(&mut self, parsed: Parsed) {
        match parsed {
            Parsed::Command(command) => self.run_command(command),
            Parsed::Unknown { name } => {
                self.post_notice(format!("unknown command /{name} — type /help for the list"));
            }
            Parsed::Misused { name, usage } => {
                self.post_notice(format!(
                    "/{name} is a command, but not with that shape — {usage} (see /help {name})"
                ));
            }
            Parsed::NotACommand => {}
        }
    }

    /// Route one recognised command to its handler.
    ///
    /// The dispatch table itself: every [`Command`] variant maps to
    /// exactly one handler here, so adding a command means adding the
    /// variant, the registration, and this arm — the compiler keeps
    /// the three in step.
    pub fn run_command(&mut self, command: Command) {
        match command {
            Command::Help { topic } => match topic {
                Some(topic) => self.show_command_detail(&topic),
                None => self.show_help(),
            },
            Command::Theme { name } => match name {
                Some(name) => self.set_theme_command(&name),
                None => self.begin_theme_preview(),
            },
            Command::Model { name } => self.switch_model_command(&name),
            Command::Clear => self.clear_command(),
            Command::Resume { target } => self.resume_command(target),
            Command::Models => self.list_models_command(),
            Command::Config => self.show_config_command(),
            Command::Set { key, value } => self.set_config_command(&key, &value),
            Command::Mode { name } => self.mode_command(name.as_deref()),
            Command::Permissions { forget } => self.permissions_command(forget.as_deref()),
            Command::Preset { name } => self.preset_command(name.as_deref()),
            Command::Unset { key } => self.unset_config_command(&key),
            Command::Compact => self.compact_command(),
            Command::Exit => self.exit_command(),
            Command::Stop => self.stop_command(),
            Command::Sessions => self.sessions_command(),
            Command::Themes => self.list_themes_command(),
            Command::Verbosity { level } => self.verbosity_command(level),
            Command::Cost => self.cost_command(),
            Command::Copy => self.copy_command(),
            Command::Version => self.version_command(),
        }
    }

    /// Print the command listing into the transcript, grouped by category.
    ///
    /// Reads the same registration table `parse` matches against, so the
    /// listing cannot advertise a command the parser would reject or omit one
    /// it would run — the rows and the vocabulary stay one thing. Commands are
    /// gathered under their category headings in the palette's own order, each
    /// row showing the invocation and its one-line summary.
    fn show_help(&mut self) {
        let now = chrono::Utc::now();
        self.push_message(TuiMessage::System {
            text: "In-session commands — a line starting with / runs here, it is not sent:"
                .to_string(),
            timestamp: now,
        });
        let specs = commands::specs();
        let mut current: Option<&str> = None;
        for spec in specs {
            if current != Some(spec.category) {
                current = Some(spec.category);
                self.push_message(TuiMessage::System {
                    text: format!("{}:", spec.category),
                    timestamp: now,
                });
            }
            let shape = if spec.args.is_empty() {
                format!("/{}", spec.name)
            } else {
                format!("/{} {}", spec.name, spec.args)
            };
            let mut row = format!("  {shape:24} {}", spec.summary);
            if !spec.aliases.is_empty() {
                let alias: Vec<String> = spec.aliases.iter().map(|a| format!("/{a}")).collect();
                row.push_str("  (also ");
                row.push_str(&alias.join(", "));
                row.push(')');
            }
            self.push_message(TuiMessage::System {
                text: row,
                timestamp: now,
            });
        }
        self.push_message(TuiMessage::System {
            text: "Type /help <command> for detail on one; Tab completes a word.".to_string(),
            timestamp: now,
        });
        self.scroll.scroll_to_bottom();
    }

    /// Explain one command in depth, resolving a name or an alias.
    ///
    /// Reads the same table as the palette via [`commands::detail_for`], so an
    /// alias (`/help new`) explains the command it names (`/clear`). An
    /// unknown topic says so and points back at the listing rather than
    /// inventing an explanation.
    fn show_command_detail(&mut self, topic: &str) {
        let now = chrono::Utc::now();
        let Some((view, _matched)) = commands::detail_for(topic) else {
            self.post_notice(format!("no command /{topic} — type /help for the list"));
            return;
        };
        self.push_message(TuiMessage::System {
            text: format!("/{} — {}", view.name, view.summary),
            timestamp: now,
        });
        if !view.usage.is_empty() {
            self.push_message(TuiMessage::System {
                text: format!("  usage: {}", view.usage),
                timestamp: now,
            });
        }
        if !view.aliases.is_empty() {
            let alias: Vec<String> = view.aliases.iter().map(|a| format!("/{a}")).collect();
            self.push_message(TuiMessage::System {
                text: format!("  aliases: {}", alias.join(", ")),
                timestamp: now,
            });
        }
        if !view.detail.is_empty() {
            self.push_message(TuiMessage::System {
                text: format!("  {}", view.detail),
                timestamp: now,
            });
        }
        self.scroll.scroll_to_bottom();
    }

    /// List the configured models, marking the one the loop is running.
    ///
    /// Reads the `[[models]]` profiles so there is a set to point `/model` at.
    /// With none configured the notice says so plainly rather than printing an
    /// empty table, because there is genuinely nothing to list.
    fn list_models_command(&mut self) {
        if self.config.models.is_empty() {
            self.post_notice(
                "no [[models]] configured — /model renames the live model directly".to_string(),
            );
            return;
        }
        let names: Vec<String> = self
            .config
            .models
            .iter()
            .map(|profile| profile.model.clone())
            .collect();
        let running = self
            .config
            .models
            .iter()
            .position(|profile| profile.model == self.config.api.model)
            .unwrap_or(0);
        self.pickers.model_picker = Some(ModelPicker {
            rows: names,
            cursor: running,
        });
    }

    /// List the settable config keys and their values in effect.
    ///
    /// Every row is `key = value` read through [`config_fields`], secret keys
    /// masked, so `/config`, `/set`, and `Tab` all describe the same surface.
    /// Keys overridden this session are tagged so the temporary ones stand out.
    fn show_config_command(&mut self) {
        let now = chrono::Utc::now();
        self.push_message(TuiMessage::System {
            text: "Configuration in effect (* overridden this session):".to_string(),
            timestamp: now,
        });
        for key in config_fields::KEYS {
            let shown = match config_fields::get(&self.config, key) {
                Some(value)
                    if config_fields::is_secret(key) && !value.is_empty() && value != "unset" =>
                {
                    "••••".to_string()
                }
                Some(value) => value,
                None => String::new(),
            };
            let mark = if self.config_writes.config_overrides.contains_key(*key) {
                "*"
            } else {
                " "
            };
            self.push_message(TuiMessage::System {
                text: format!("  {mark} {key} = {shown}"),
                timestamp: now,
            });
        }
        self.push_message(TuiMessage::System {
            text:
                "The theme, verbosity, caret_display, diff_layout, diff_palette, strip_position, \
                   strip_style, path_style, and copy_command rows change live, and permission_mode \
                   moves live too — the session remembers it; the rest \
                   are read at startup. /set <key> <value> · /unset <key> · /config shows the \
                   value in effect."
                    .to_string(),
            timestamp: now,
        });
        self.scroll.scroll_to_bottom();
    }

    /// Override a config value for this session, or say why it cannot be.
    ///
    /// Every accepted write lands in two places at once — the session's
    /// own config and the config file, via
    /// [`persist_config`](Self::persist_config) — but only a value the
    /// running panel re-reads (the theme, the verbosity, the caret
    /// display, the diff layout) changes the session already running:
    /// those apply on the spot and are remembered so `/unset` can undo
    /// them. A value the loop, the HTTP client, or a spawned process
    /// reads once at startup cannot be moved for this session; the
    /// write is still taken, and the notice says when it lands — "read
    /// when dch starts" — rather than implying a liveness nothing
    /// delivers. `/config` shows the value actually in effect.
    ///
    /// The model is one exception worth naming: the live conversation's model
    /// *can* be moved between turns, but through `/model`, which drives the
    /// running client — so `/set model X` is sugar for `/model X` rather than a
    /// silent config edit that would not reach the loop. `permission_mode` is
    /// the other: it lands in the session's own config only — the base and
    /// the file never hear of it — because the session file carries the mode
    /// a resume re-seats.
    pub(crate) fn set_config_command(&mut self, key: &str, value: &str) {
        if key == "model" {
            self.switch_model_command(value);
            return;
        }
        if !config_fields::is_key(key) {
            let keys: Vec<&str> = config_fields::KEYS.iter().copied().take(8).collect();
            self.post_notice(format!(
                "no such setting: {key} — /config lists them; try one of: {}",
                keys.join(", ")
            ));
            return;
        }
        let persisted = if key == "permission_mode" {
            &self.config
        } else {
            self.config_writes
                .config_file
                .as_ref()
                .unwrap_or(&self.config)
        };
        if !self.config_writes.config_overrides.contains_key(key) {
            let original = if config_fields::is_absent(persisted, key) {
                None
            } else {
                Some(config_fields::get(persisted, key).unwrap_or_default())
            };
            self.config_writes
                .config_overrides
                .insert(key.to_string(), original);
        }
        if let Err(why) = config_fields::set(&mut self.config, key, value) {
            self.reconcile_override(key);
            self.post_notice(format!("could not set {key}: {why}"));
            return;
        }
        let session_scoped = key == "permission_mode";
        if !session_scoped {
            if let Some(base) = self.config_writes.config_base.as_mut() {
                let _mirror = config_fields::set(base, key, value);
            }
            if let Some(file) = self.config_writes.config_file.as_mut() {
                let _mirror = config_fields::set(file, key, value);
            }
        }
        self.apply_live_config(key);
        let shown = if config_fields::is_secret(key) && !value.is_empty() && value != "unset" {
            "••••".to_string()
        } else {
            value.to_string()
        };
        if matches!(config_fields::live(key), Some(config_fields::Live::Display)) {
            self.post_notice(format!("{key} = {shown} — applied now"));
        } else if key == "permission_mode" {
            // The echo names the canonical spelling in force: a
            // retired alias (`plan`) lands as its replacement
            // (`read-only`), and the notice must not teach the alias
            // as if it stuck.
            self.post_notice(format!(
                "{key} = {} — applies live, the next tool call runs under it; \
                 this session remembers it",
                self.config.runner.permission_mode.as_str()
            ));
        } else {
            self.post_notice(format!(
                "{key} = {shown} — saved; read when dch starts, so it applies next session"
            ));
        }
        if key == "permission_mode" {
            self.move_permission_mode_live();
        }
        if !session_scoped {
            self.persist_config(key);
        }
    }

    /// `/preset` — the permission presets' own door.
    ///
    /// Bare lists the configured presets and marks the live selection; a
    /// name swaps the session's selection. The swap is process state,
    /// unlike `/mode`: it lives in the session's own config and the
    /// shared rules cell — the loop's policy re-reads the preset join
    /// per evaluation — and never reaches the configuration file, so a
    /// restart or a launch `--resume` starts under the configuration's
    /// own selection. The selection also rides the shared preset
    /// cell, so the driver's fresh adoptions — a `/clear`, a healthy
    /// resume — re-seed at the selection in effect, never the boot
    /// one. While a damage verdict stands the swap records the
    /// selection and holds: the config layer stays tightened until
    /// `/permissions repair` re-seeds it as chosen. The swap runs
    /// under the driver's adoption guard — the same lock a live
    /// adoption holds across its whole motion — so the two can never
    /// interleave: a damaged adoption cannot land between the swap's
    /// damage check and its rules write.
    pub(crate) fn preset_command(&mut self, name: Option<&str>) {
        let Some(selected) = name else {
            let names: Vec<&str> = self
                .config
                .permissions
                .presets
                .keys()
                .map(String::as_str)
                .collect();
            if names.is_empty() {
                self.post_notice(
                    "no [permissions.presets] configured — add one to the config file".to_string(),
                );
            } else {
                let live = self
                    .config
                    .permissions
                    .preset
                    .as_deref()
                    .unwrap_or("(none)");
                self.post_notice(format!("presets: {} — selected: {live}", names.join(", ")));
            }
            return;
        };
        if !self.config.permissions.presets.contains_key(selected) {
            let names: Vec<&str> = self
                .config
                .permissions
                .presets
                .keys()
                .map(String::as_str)
                .collect();
            let available = if names.is_empty() {
                "(none configured)".to_string()
            } else {
                names.join(", ")
            };
            self.post_notice(format!("no preset {selected:?} — configured: {available}"));
            return;
        }
        self.config.permissions.preset = Some(selected.to_string());
        let adoption = self.permission.permission_adoption_guard.clone();
        let _held = adoption.as_ref().map(|guard| {
            guard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if let Some(cell) = &self.permission.permission_preset_cell {
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(selected.to_string());
        }
        if self.permission.permission_memory.damage_standing() {
            self.post_notice(format!(
                "preset {selected:?} selected — its rules wait for /permissions repair"
            ));
            return;
        }
        let rules = self.config.permissions.engine_rules();
        if let Some(cell) = &self.permission.permission_config_rules_cell {
            cell.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone_from(&rules);
        } else {
            self.post_notice("the loop adopts the selection on its next rebuild".to_string());
        }
        self.post_notice(format!(
            "preset {selected:?} selected — its rules join the config layer"
        ));
    }

    /// `/mode` — the permission mode's own short door.
    ///
    /// Bare names the mode in effect and the spellings it takes; a
    /// value rides [`Self::set_config_command`] — validation and the
    /// override ledger — and then moves live: the shared cell the gate
    /// re-reads per dispatch flips on the spot, so even mid-run the
    /// next tool call runs under the new mode. The file mirror and the
    /// persist are skipped for this key — the session file carries the
    /// mode, and the config file never hears of it. A rebuild that
    /// cannot stand hands the old mode back to the cell — the driver
    /// writes that direction — and the notice names the mode kept.
    fn mode_command(&mut self, name: Option<&str>) {
        match name {
            Some(value) => self.set_config_command("permission_mode", value),
            None => self.post_notice(format!(
                "mode is {} — /mode <auto|read_only|interactive>",
                self.config.runner.permission_mode.as_str()
            )),
        }
    }

    /// Point live config changes at a directory to persist into.
    ///
    /// The host's wiring: the real session hands the user's config
    /// directory, and every later live change writes the whole
    /// configuration through
    /// [`save_to_dir`](dch_config::DchConfig::save_to_dir) — atomically,
    /// into the file a next load prefers.
    pub fn set_config_persist_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.config_writes.config_persist_dir = dir;
    }

    /// Install the file-stated config write-through persists.
    ///
    /// The host's wiring: the file as loaded, before any CLI override
    /// or model selection ran. Every user-authored change lands on it
    /// beside the live config, and `persist_config` serializes it
    /// rather than the session-effective copy.
    pub fn set_config_file(&mut self, file: DchConfig) {
        self.config_writes.config_file = Some(file);
    }

    /// Install the user-stated base a model selection layers onto.
    ///
    /// The host's wiring for the no-accretion rule: the base is the
    /// boot-time config before any model selection ran — the file plus
    /// every CLI override except the boot model — and every later
    /// selection the panel mirrors (a `/model` verdict, a provider
    /// switch's verdict) layers onto it, so the second switch of a
    /// session equals a restart naming it. A host that never calls this
    /// snapshots the live config at the panel's first selection
    /// verdict, which is exact only for configs nobody boot-selected.
    pub fn set_config_base(&mut self, base: DchConfig) {
        self.config_writes.config_base = Some(base);
    }

    /// Write the configuration in effect to the persist directory.
    ///
    /// The one throat every live change swallows: on success the
    /// notice names the class of the change that landed; on failure the
    /// wording branches on that same class, because the two classes
    /// fail differently — a display value the panel re-reads has
    /// already applied and only the save is lost, while a restart-class
    /// value applied nothing this session and, unsaved, applies nothing
    /// next start either. The model is the restart-class exception: a
    /// switch has already applied live by the time the save runs, and
    /// its failure wording says so rather than claiming nothing took.
    /// The failure never rolls the session's own value back.
    ///
    /// When no config file is wired — an embedder host — the save
    /// serializes the live config through a stand-in that first
    /// restores the session-scoped fields, the permission mode and
    /// the preset selection, to the pair this session started under:
    /// a `/mode` or `/preset` swap must not ride the save into a file
    /// it promised never to touch, and an initial flag-stated pair
    /// must not ride the save *out* of one either.
    pub(crate) fn persist_config(&mut self, key: &str) {
        let Some(dir) = self.config_writes.config_persist_dir.clone() else {
            return;
        };
        let to_save = if let Some(file) = self.config_writes.config_file.as_ref() {
            file.clone()
        } else {
            let mut stand_in = self.config.clone();
            stand_in.runner.permission_mode = self.permission.permission_session_start.0;
            stand_in
                .permissions
                .preset
                .clone_from(&self.permission.permission_session_start.1);
            stand_in
        };
        if let Err(why) = to_save.save_to_dir(&dir) {
            match key {
                "model" => self.post_notice(format!(
                    "the switch is live but not saved — the config save failed: {why}"
                )),
                _ if matches!(config_fields::live(key), Some(config_fields::Live::Display)) => {
                    self.post_notice(format!(
                        "applied for this session only — the config save failed: {why}"
                    ));
                }
                _ => self.post_notice(format!(
                    "not saved — the config save failed: {why}. {key} was never applied this \
                     session, and without the save it will not apply next start either"
                )),
            }
        }
    }

    /// Drop the session override on `key`, restoring its original value.
    ///
    /// Restores the value the config had before the first `/set` this session,
    /// not a compiled-in default — so an override is truly undone, in the
    /// session and in the file alike: the restore writes through, or a
    /// restart would resurrect the override the user just removed. A key
    /// that was never overridden says so rather than quietly rewriting it.
    /// `permission_mode` is the exception, exactly as on the set side: the
    /// key is session-scoped, so its unset restores the live value alone —
    /// the base and file mirrors and the save stay untouched, or the
    /// session's mode would overwrite the file's configured one for every
    /// later launch.
    fn unset_config_command(&mut self, key: &str) {
        let Some(original) = self.config_writes.config_overrides.remove(key) else {
            self.post_notice(format!("{key} has no session override to undo"));
            return;
        };
        let restored = match original.as_deref() {
            None => config_fields::clear(&mut self.config, key),
            Some(value) => config_fields::set(&mut self.config, key, value),
        };
        if let Err(why) = restored {
            self.config_writes
                .config_overrides
                .insert(key.to_string(), original);
            self.post_notice(format!("could not restore {key}: {why}"));
            return;
        }
        let session_scoped = key == "permission_mode";
        if !session_scoped {
            if let Some(base) = self.config_writes.config_base.as_mut() {
                match original.as_deref() {
                    None => {
                        let _mirror = config_fields::clear(base, key);
                    }
                    Some(value) => {
                        let _mirror = config_fields::set(base, key, value);
                    }
                }
            }
            if let Some(file) = self.config_writes.config_file.as_mut() {
                match original.as_deref() {
                    None => {
                        let _mirror = config_fields::clear(file, key);
                    }
                    Some(value) => {
                        let _mirror = config_fields::set(file, key, value);
                    }
                }
            }
        }
        if key == "permission_mode" {
            self.move_permission_mode_live();
        }
        self.apply_live_config(key);
        if !session_scoped {
            self.persist_config(key);
        }
        let shown = if original.is_none() {
            "unset — an environment credential takes over again".to_string()
        } else if config_fields::is_secret(key) {
            "••••".to_string()
        } else {
            original.unwrap_or_default()
        };
        self.post_notice(format!("{key} restored to {shown}"));
    }

    /// Push a just-changed value into whatever panel state reads it live.
    ///
    /// Only display-classed values have panel state to refresh — the
    /// theme, the verbosity, the caret display, and the diff layout —
    /// and their handlers re-bake the caches or re-render the rendered
    /// exactly as the interactive controls would; every other key falls
    /// through untouched, its effect waiting on the next start.
    fn apply_live_config(&mut self, key: &str) {
        match key {
            "theme" => {
                if let Some(theme) = Theme::by_name(&self.config.display.theme) {
                    self.theme = theme;
                    self.land_theme_change();
                }
            }
            "verbosity" => {
                let verbosity = self.config.display.verbosity;
                self.set_verbosity(verbosity);
            }
            "diff_layout" | "diff_palette" => {
                self.invalidate_conversation_renders();
            }
            "strip_position" => {
                self.display.strip_position = self.config.display.strip_position;
            }
            "strip_style" => {
                self.display.strip_style = self.config.display.strip_style;
            }
            "copy_command" => {
                let copy_command = self.config.display.copy_command.clone();
                let outcomes = Arc::clone(&self.copy.copy_outcomes);
                let wake = Arc::clone(&self.state.render_notify);
                self.set_selection_copier(Box::new(move |attempt, text| {
                    copy_via_transports(copy_command.as_deref(), attempt, text, &outcomes, &wake)
                }));
            }
            _ => {}
        }
    }

    /// Drop a ledger entry whose recorded original equals the current value.
    ///
    /// When a `/set` was refused nothing actually changed, so an entry added
    /// just before the attempt would make `/unset` claim to undo something that
    /// never happened; if the stored original still matches the value, the key
    /// is not overridden and the entry is removed.
    fn reconcile_override(&mut self, key: &str) {
        let still_same = self
            .config_writes
            .config_overrides
            .get(key)
            .is_some_and(|original| match original {
                Some(original) => {
                    config_fields::get(&self.config, key).as_deref() == Some(original.as_str())
                }
                None => config_fields::is_absent(&self.config, key),
            });
        if still_same {
            self.config_writes.config_overrides.remove(key);
        }
    }

    /// Send the driver a live resume: swap the conversation to a saved
    /// session.
    ///
    /// The display's part is the shape check and the send: a target that
    /// is not a session id is refused here — the driver's load keys on
    /// the id alone — and a well-formed one travels to the driver, which
    /// loads the session, rebuilds the loop on its history, and moves
    /// the save target onto its file. What the screen shows comes back
    /// over the session channel, seeded from the loaded transcript.
    /// The send is refused while a run holds the agent, like every
    /// conversation-moving command, and the waiting prompts drop before
    /// it leaves: they belong to the conversation being left, and a
    /// claim the driver already took must not race the resumed
    /// session's first turn.
    fn resume_command(&mut self, target: Option<String>) {
        let Some(id) = target else {
            self.post_notice("which session? /resume <id> — the id is printed on exit".to_string());
            return;
        };
        if uuid::Uuid::parse_str(&id).is_err() {
            self.post_notice(format!(
                "{id} is not a session id — /resume takes the id dch --resume prints"
            ));
            return;
        }
        if self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "commands run between turns — wait for the current run to finish".to_string(),
            );
            return;
        }
        self.queue.handed = None;
        self.state
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.send_command(SessionCommand::Resume { id });
        self.post_notice("resuming the session…".to_string());
    }

    /// Send the driver a manual compaction pass, between turns.
    ///
    /// The engine's own context manager folds the committed history the
    /// same way its automatic passes do; the next turn continues from
    /// the folded context, and the driver's notice carries the token
    /// figures the pass measured. The folded history is also published
    /// to the machine snapshot, so a session that ends before its next
    /// run resumes from the compacted context. Nothing is sent to the
    /// model — an unknown slash line is never a prompt.
    fn compact_command(&mut self) {
        if self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "commands run between turns — wait for the current run to finish".to_string(),
            );
            return;
        }
        let compactions = u32::try_from(
            self.conversation
                .iter()
                .filter(|message| matches!(message, TuiMessage::Compacted { .. }))
                .count(),
        )
        .unwrap_or(u32::MAX);
        self.send_command(SessionCommand::Compact { compactions });
    }

    /// Quit the session and leave the terminal, cleanly.
    ///
    /// The explicit-intent path through the exit contract: the same sequence
    /// the confirmed `Ctrl+C` press runs — raise the shared shutdown flag so
    /// the driver refuses anything still queued, cancel a run in flight if one
    /// is going, then ask the frame loop to stop. There is no second press to
    /// wait for: typing the command is itself the confirmation.
    fn exit_command(&mut self) {
        self.exit
            .shutdown_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel_run_in_flight();
        self.quit.requested = true;
        self.post_notice("exiting — the session is saved".to_string());
    }

    /// Stop the run in flight without leaving the session.
    ///
    /// Cancels the current run through the same canceller `Ctrl+C` uses, so the
    /// turn ends as an ordinary cancellation (its hook persists what landed) —
    /// but unlike `/exit` the shutdown flag stays down, so the session and its
    /// queue keep going. A plain notice when no run is in flight.
    fn stop_command(&mut self) {
        if self.cancel_run_in_flight() {
            self.post_notice("stopped the current run — the session continues".to_string());
        } else {
            self.post_notice("nothing is running to stop".to_string());
        }
    }

    /// Open the saved-session picker on a catalog the driver listed.
    ///
    /// The `/sessions` answer's display half: an empty catalog says so
    /// on the notice row — the user asked and deserves a reply, not a
    /// silently blank screen — while anything else seats the cursor on
    /// the newest conversation.
    pub(crate) fn open_session_picker(&mut self, rows: Vec<SessionPick>) {
        if rows.is_empty() {
            self.post_notice("no saved sessions yet — one is saved on exit".to_string());
            return;
        }
        self.pickers.session_picker = Some(SessionPicker { rows, cursor: 0 });
    }

    /// Ask the driver for the saved-session catalog.
    ///
    /// The display's `/sessions` half is the ask: the sessions root
    /// lives on the driver's side of the crate boundary, so the list
    /// travels as a command and the catalog comes back as an update
    /// that opens the picker. The ask itself posts nothing — the
    /// picker's arrival is the answer. Refused while a run holds the
    /// agent, like every conversation-moving command.
    fn sessions_command(&mut self) {
        if self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "commands run between turns — wait for the current run to finish".to_string(),
            );
            return;
        }
        self.send_command(SessionCommand::ListSessions);
    }

    /// List every compiled-in theme, marking the live one.
    ///
    /// Reads the same constructor table `/theme` steps and `/theme <name>` sets
    /// through, so the list is exactly the set that resolves. The running
    /// palette is starred; the closing line says how to move.
    fn list_themes_command(&mut self) {
        let now = chrono::Utc::now();
        let current = self.theme.name;
        let keys = Theme::command_names();
        self.push_message(TuiMessage::System {
            text: format!("Themes (* marks the live one, {current}):"),
            timestamp: now,
        });
        for key in keys {
            let live = Theme::by_name(key).is_some_and(|theme| theme.name == current);
            let mark = if live { "*" } else { " " };
            self.push_message(TuiMessage::System {
                text: format!("  {mark} {key}"),
                timestamp: now,
            });
        }
        self.push_message(TuiMessage::System {
            text: "Step through them live with /theme; set one with /theme <name>.".to_string(),
            timestamp: now,
        });
        self.scroll.scroll_to_bottom();
    }

    /// Set or report the display verbosity.
    ///
    /// With a level, routes through the same value-setting path as
    /// `/set verbosity` — validating, remembering the original so `/unset` can
    /// undo it, and applying to the display at once. Without one, it just
    /// reports the current level so the user knows the word to pass.
    fn verbosity_command(&mut self, level: Option<String>) {
        match level {
            None => self.post_notice(format!(
                "verbosity is {:?} — /verbosity quiet|normal|verbose",
                self.display.verbosity
            )),
            Some(level) => self.set_config_command("verbosity", &level),
        }
    }

    /// Report the context fill and this session's token totals.
    ///
    /// Reads the same snapshot the context bar draws from (how full the model's
    /// window is) plus the token tallies the loop keeps, so the number on the
    /// bar and the number here agree. Read-only; nothing is sent.
    pub(crate) fn cost_command(&mut self) {
        let now = chrono::Utc::now();
        let (used, window) = self.context_snapshot();
        let percent = if window == 0 {
            0
        } else {
            used.saturating_mul(100)
                .saturating_add(window.saturating_div(2))
                .checked_div(window)
                .unwrap_or(0)
        };
        let (input, output, total_in, total_out) = {
            let counts = self
                .state
                .tokens
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (
                counts.input,
                counts.output,
                counts.cumulative_input,
                counts.cumulative_output,
            )
        };
        self.push_message(TuiMessage::System {
            text: format!("Context: {used} of {window} tokens used ({percent}% full)."),
            timestamp: now,
        });
        self.push_message(TuiMessage::System {
            text: format!(
                "  last turn: {input} in / {output} out · session total: {total_in} in / {total_out} out"
            ),
            timestamp: now,
        });
        self.scroll.scroll_to_bottom();
    }

    /// Print the running build's version.
    ///
    /// The compiled-in `CARGO_PKG_VERSION` beside the binary's name —
    /// the figure a bug report names, with nothing else riding along.
    fn version_command(&mut self) {
        let now = chrono::Utc::now();
        self.push_message(TuiMessage::System {
            text: format!("dch {}", env!("CARGO_PKG_VERSION")),
            timestamp: now,
        });
        self.scroll.scroll_to_bottom();
    }

    /// Swap the palette for preview, without landing it on the terminal.
    ///
    /// The browsing half of a theme switch: the palette field swaps and
    /// the render caches invalidate — the next frame paints in the new
    /// colors — but no OSC escape is written, no terminal query runs,
    /// no cursor tint changes. The terminal's own background and cursor
    /// keep their previous state until the user commits (Enter submits
    /// the command, or the bare `/theme` settles), at which point
    /// [`land_theme_change`](Self::land_theme_change) performs the
    /// full landing. Browsing stays instant this way: stepping through
    /// ten themes costs ten cache invalidations, not ten terminal
    /// round-trips.
    pub(crate) fn preview_theme(&mut self, theme: Theme) {
        if self.browse.theme_before_browse.is_none() {
            self.browse.theme_before_browse = Some(std::mem::replace(&mut self.theme, theme));
        } else {
            self.theme = theme;
        }
        self.browse.pending_theme_osc = Some((self.theme.ui.background, self.theme.ui.primary));
        self.invalidate_conversation_renders();
    }

    /// Pin the canvas mode the app renders under.
    ///
    /// Construction reads the environment — a multiplexed terminal
    /// settles local painting — and this override stands the app in
    /// either mode regardless of the machine it runs on, so a harness
    /// or test that renders frames and reads cells pins the shape it
    /// asserts. A switch that lands after the pin follows it: local
    /// painting never queries, never writes an OSC byte, and never
    /// defers the palette.
    pub fn set_canvas_painted_locally(&mut self, local: bool) {
        self.canvas_local = local;
    }

    /// Land a theme change on every surface the session already painted.
    ///
    /// The startup sequence, replayed on every switch. The background
    /// override lands in two tiers: a terminal the session has already
    /// captured takes a plain write — no query, no reader to park —
    /// and a terminal the session has never asked is asked exactly
    /// once, with the event reader parked through the quiesce
    /// handshake so the reply cannot be stolen into the composer as
    /// keystrokes. A terminal that was asked and stayed silent is
    /// never asked again; its palettes defer to the terminal's own
    /// scheme for the session's life, exactly as they did at startup.
    /// The cursor rides the canvas: tinted to the theme's accent where
    /// the override lands, returned to the terminal's own where the
    /// palette goes paintless. Both writes are deferred to the render
    /// flush — the same burst that carries the re-styled cells — so a
    /// committed switch lands in one visible step, never a canvas that
    /// flips a beat ahead of the text under it. The invalidation
    /// regenerates every styled line — the settled bake, the merged
    /// cache the frames serve, and the live lane's streaming rows —
    /// under the new palette rather than serving the old colors from
    /// cache. Inside a multiplexer the landing is all local: the
    /// canvas is painted on the frame's own cells
    /// ([`TuiApp::canvas_local`]), so no query runs, no OSC byte is
    /// written, and no deferral applies — the palette keeps every
    /// color it was built with, and the invalidation alone carries
    /// the switch onto the screen.
    fn land_theme_change(&mut self) {
        if !self.canvas_local {
            let background = self.theme.ui.background;
            let landed = if crate::terminal::write_only_warranted(background) {
                true
            } else {
                self.ask_background_once(background)
            };
            if !landed {
                self.theme = self.theme.deferred_to_terminal();
            }
        }
        self.browse.pending_theme_osc = Some((self.theme.ui.background, self.theme.ui.primary));
        self.invalidate_conversation_renders();
    }

    /// Change the live theme by name, keeping the current one on a miss.
    ///
    /// Resolves through [`Theme::by_name`], so the display spelling and the
    /// lowercase key reach the same palette however they are typed; rendering
    /// reads the theme field every frame, so a swap shows on the next draw.
    fn set_theme_command(&mut self, name: &str) {
        self.browse.theme_before_browse = None;
        if let Some(theme) = Theme::by_name(name) {
            self.theme = theme;
            self.land_theme_change();
            let key = Theme::key_for_name(name).unwrap_or_default();
            self.config.display.theme = key.to_string();
            if let Some(file) = self.config_writes.config_file.as_mut() {
                file.display.theme = key.to_string();
            }
            self.persist_config("theme");
        } else {
            self.post_notice(format!("unknown theme '{name}'"));
        }
    }

    /// Step to the next theme as a live preview.
    ///
    /// A bare `/theme` advances the panel to the next palette in registry
    /// order; the change shows on the next draw because rendering reads the
    /// theme field each frame. Repeating the command walks the whole set and
    /// wraps, so `/theme` acts as its own cycle — whichever theme is showing
    /// when you stop is the one this session keeps; the next launch opens
    /// the file's theme again, so a palette meant to stay is set with
    /// `/theme <name>` or `/set theme`. This is deliberately stateless:
    /// there is no half-applied preview to cancel with an unrelated
    /// keystroke, and no committed palette to fall back to.
    fn begin_theme_preview(&mut self) {
        self.browse.theme_before_browse = None;
        let names = Theme::names();
        let Some(current) = names.iter().position(|name| *name == self.theme.name) else {
            self.post_notice(format!(
                "theme '{}' is not in the built-in set to step through; /theme <name> to set one",
                self.theme.name
            ));
            return;
        };
        let next = current
            .checked_add(1)
            .filter(|next| *next < names.len())
            .unwrap_or(0);
        if let Some(name) = names
            .get(next)
            .and_then(|candidate| Theme::by_name(candidate))
        {
            self.theme = name;
            self.land_theme_change();
        }
    }

    /// Switch the live loop's model, resolved against the configured models.
    ///
    /// The lookup mirrors `dch --model`: a case-insensitive match among the
    /// `[[models]]` entries, sent as that entry's own spelling. A
    /// same-provider entry renames the live client; an entry on another
    /// endpoint or protocol travels as a provider rebuild carrying the
    /// conversation. With no entries configured any name is a plain
    /// rename, again matching `--model`. The panel's configuration and
    /// the file change only on the loop's verdict, published back over
    /// the session channel — a refused switch leaves both exactly as
    /// they were. A command is a between-turns act, so a run in flight
    /// is refused.
    fn switch_model_command(&mut self, name: &str) {
        if self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "commands run between turns — wait for the current run to finish".to_string(),
            );
            return;
        }
        let profile = self
            .config
            .models
            .iter()
            .find(|profile| profile.model.eq_ignore_ascii_case(name));
        if let Some(profile) = profile
            && self.model_is_cross_provider(profile)
        {
            let (model, base_url, api_type) = (
                profile.model.clone(),
                profile.base_url.clone().unwrap_or_default(),
                profile
                    .api_type
                    .and_then(|kind| {
                        serde_json::to_value(kind)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_string))
                    })
                    .unwrap_or_default(),
            );
            self.retire_live_context_window();
            self.send_command(SessionCommand::SwitchProvider {
                model,
                base_url,
                api_type,
            });
            return;
        }
        let model = match profile {
            Some(profile) => profile.model.clone(),
            None if self.config.models.is_empty() => name.to_string(),
            None => {
                let options: Vec<&str> = self
                    .config
                    .models
                    .iter()
                    .map(|profile| profile.model.as_str())
                    .take(6)
                    .collect();
                self.post_notice(format!(
                    "no model named '{name}'; /model accepts: {}",
                    options.join(", ")
                ));
                return;
            }
        };
        self.retire_live_context_window();
        self.send_command(SessionCommand::Model { name: model });
    }

    /// Whether `profile` would move the conversation to another client.
    ///
    /// A same-provider entry leaves both `base_url` and `api_type` unset,
    /// inheriting the running config; a cross-provider entry replaces one or
    /// both, which names a different client rather than a model on this loop.
    fn model_is_cross_provider(&self, profile: &dch_config::ModelProfile) -> bool {
        profile
            .base_url
            .as_deref()
            .is_some_and(|url| url != self.config.api.base_url)
            || profile
                .api_type
                .as_ref()
                .is_some_and(|kind| *kind != self.config.api.api_type)
    }

    /// Empty the session at rest: the display here, the loop in the driver.
    ///
    /// The transcript, the waiting queue, the selection, and the summary
    /// debts the compacted rows still owe are the app's half and clear on
    /// the spot; the loop rewind and the fresh save target are the
    /// driver's, asked for by the command sent beside it. Refused while a
    /// run is in flight.
    pub(crate) fn clear_command(&mut self) {
        if self.state.agent_running.load(Ordering::SeqCst) {
            self.post_notice(
                "commands run between turns — wait for the current run to finish".to_string(),
            );
            return;
        }
        self.permission
            .permission_memory
            .adopt(Vec::new(), Vec::new(), false);
        self.config_writes
            .config_overrides
            .remove("permission_mode");
        self.compaction_summary_owed.clear();
        self.plan.latest_todos.clear();
        self.conversation.clear();
        self.bake.conversation_generation = self.bake.conversation_generation.saturating_add(1);
        self.pointer.selection = None;
        self.queue.handed = None;
        self.state
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.scroll.scroll_to_bottom();
        self.send_command(SessionCommand::Clear);
        self.post_notice("session cleared — starting a fresh one".to_string());
    }

    /// Hand a session command to the driver over the submit channel.
    ///
    /// The driver is the loop's only holder; the display names the command
    /// rather than reaching for it. A channel nobody listens to, and one that
    /// was never installed, each report themselves on the notice row.
    pub(crate) fn send_command(&mut self, command: SessionCommand) {
        let sent = match self.host.submit_tx.as_ref() {
            Some(tx) => tx.send(Submission::Command { command }).is_ok(),
            None => false,
        };
        if sent {
            return;
        }
        if self.host.submit_tx.is_some() {
            self.post_notice("the driver has gone — nothing more can be sent".to_string());
        } else {
            self.post_notice("this session has no agent loop to command".to_string());
        }
    }

    /// The keys the saved-session picker takes, when it stands.
    ///
    /// The picker owns the keyboard while it stands: the arrows walk
    /// the rows (wrapping at the ends), Enter resumes the highlighted
    /// session through the very lane `/resume` takes — so every guard
    /// there applies — and Esc closes. The sheet-surviving chords pass
    /// through untouched; everything else is swallowed, because a
    /// keystroke behind a picker's back would build a draft the picker
    /// then hides.
    pub(crate) fn sessions_key(&mut self, key: KeyEvent) -> Option<bool> {
        if self.pickers.session_picker.is_none() || key_survives_the_sheet(key) {
            return None;
        }
        let handled = match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.step_session_picker(true);
                true
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.step_session_picker(false);
                true
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let chosen = self
                    .pickers
                    .session_picker
                    .as_ref()
                    .and_then(|picker| picker.rows.get(picker.cursor).cloned());
                match chosen {
                    Some(pick) => {
                        self.pickers.session_picker = None;
                        self.resume_command(Some(pick.id));
                        true
                    }
                    None => true,
                }
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.pickers.session_picker = None;
                true
            }
            _ => true,
        };
        Some(handled)
    }

    /// The keys the configured-models picker takes, when it stands.
    ///
    /// The same contract as the session picker: the arrows walk the
    /// rows (wrapping), Enter switches to the highlighted model
    /// through the very `/model` lane, Esc closes. Everything else
    /// is swallowed.
    pub(crate) fn models_key(&mut self, key: KeyEvent) -> Option<bool> {
        if self.pickers.model_picker.is_none() || key_survives_the_sheet(key) {
            return None;
        }
        let handled = match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.step_model_picker(true);
                true
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.step_model_picker(false);
                true
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let chosen = self
                    .pickers
                    .model_picker
                    .as_ref()
                    .and_then(|picker| picker.rows.get(picker.cursor).cloned());
                match chosen {
                    Some(model) => {
                        self.pickers.model_picker = None;
                        self.switch_model_command(&model);
                        true
                    }
                    None => true,
                }
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.pickers.model_picker = None;
                true
            }
            _ => true,
        };
        Some(handled)
    }

    /// Step the model picker's cursor one row, wrapping at the ends.
    ///
    /// The picker's arrow semantics: Up and Down move a single row and
    /// wrap past either end onto the opposite row, so both directions
    /// reach every entry without a second key. An empty sheet (no
    /// declared models) never reaches here — the picker does not open —
    /// and the guard keeps the step a no-op rather than a panic if one
    /// ever stands.
    fn step_model_picker(&mut self, up: bool) {
        let Some(picker) = self.pickers.model_picker.as_mut() else {
            return;
        };
        if picker.rows.is_empty() {
            return;
        }
        let last = picker.rows.len().saturating_sub(1);
        picker.cursor = if up {
            if picker.cursor == 0 {
                last
            } else {
                picker.cursor.saturating_sub(1)
            }
        } else if picker.cursor >= last {
            0
        } else {
            picker.cursor.saturating_add(1)
        };
    }

    /// Step the picker's cursor one row, wrapping at the ends.
    ///
    /// The same walk the completion popup's arrows take: a list short
    /// enough to show whole never moves, and the ends wrap so both
    /// directions always have somewhere to go. An empty sheet never
    /// reaches here — the picker does not open — and the guard keeps
    /// the step a no-op rather than a panic if one ever stands.
    fn step_session_picker(&mut self, up: bool) {
        let Some(picker) = self.pickers.session_picker.as_mut() else {
            return;
        };
        if picker.rows.is_empty() {
            return;
        }
        let last = picker.rows.len().saturating_sub(1);
        picker.cursor = if up {
            if picker.cursor == 0 {
                last
            } else {
                picker.cursor.saturating_sub(1)
            }
        } else if picker.cursor >= last {
            0
        } else {
            picker.cursor.saturating_add(1)
        };
    }

    /// Invalidate every cached rendering of the conversation: the
    /// baked prefix, the merged cache the frames serve, and the live
    /// lane's styled rows.
    ///
    /// The shapes that make this necessary differ — a verbosity switch
    /// re-shapes every tool body, a theme switch re-tints every styled
    /// line — but the cache discipline is one: the generation bump
    /// pierces the render guard (or the merged cache would keep serving
    /// the old rendering until an unrelated event rebaked), the rebake
    /// empties the bake, and the live-stamp reset forces the streaming
    /// rows to re-render (or they would keep the old rendering until
    /// their next delta).
    pub(crate) fn invalidate_conversation_renders(&mut self) {
        self.bake.conversation_generation = self.bake.conversation_generation.saturating_add(1);
        self.bake.rebake_whole();
        self.freeze.live_stamp = 0;
    }
}

/// The floating pickers: the saved-session catalog and the
/// configured-model list, whichever stands.
///
/// At most one picker owns the keyboard at a time; both take the
/// same arrow/Enter/Esc contract and close on the act they commit.
/// `None` in each slot means no picker of that kind stands.
#[derive(Default)]
pub(crate) struct Pickers {
    /// The saved-session picker `/sessions` opened, when it stands.
    ///
    /// Holds the catalog the driver listed — newest first — and the
    /// row the cursor is on. `None` when no picker stands; the picker
    /// owns the keyboard while it does, and Enter hands its row to
    /// the resume lane exactly as `/resume <id>` would.
    pub(crate) session_picker: Option<SessionPicker>,

    /// The configured-models picker `/models` opened, when it stands.
    ///
    /// Same shape as the session picker: the model names with the
    /// cursor, Enter switching through the `/model` lane. `None` when
    /// no picker stands.
    pub(crate) model_picker: Option<ModelPicker>,
}

/// The theme browse's state: the palette the session stood on and
/// the terminal escapes a preview owes.
///
/// A browse exists only while the completion popup walks `/theme`
/// candidates; the pair is what makes leaving the browse honest —
/// the remembered palette restores, and the deferred escapes keep
/// the terminal's own colors untouched until a commit lands whole.
#[derive(Default)]
pub(crate) struct ThemeBrowse {
    /// The theme the session was on before the completion popup
    /// started previewing.
    ///
    /// Browsing `/theme` candidates previews each highlighted palette;
    /// this holds what was showing before the browsing began, so
    /// dismissing the popup without committing restores it — a
    /// preview the user walked away from is not a switch. Cleared by
    /// every committed path (`/theme <name>`, `/theme` bare,
    /// `/set theme`); `None` when no browse stands.
    pub(crate) theme_before_browse: Option<Theme>,

    /// OSC escapes a pending theme preview owes the terminal.
    ///
    /// A preview swaps the palette and invalidates the caches at key
    /// time, but the terminal escapes (background, cursor tint) are
    /// deferred to the render: writing them at key time means the
    /// background changes one frame before the text catches up —
    /// the two-step flash. Holding them here lets the render loop
    /// write the escapes and draw the re-styled content in one
    /// back-to-back flush, so the palette lands whole.
    pub(crate) pending_theme_osc: Option<(ratatui::style::Color, ratatui::style::Color)>,
}

/// The configuration's write-through state: where live changes
/// persist, the file as loaded, the session's override ledger, and
/// the no-accretion base.
///
/// The four config surfaces a `/set`, `/unset`, `/theme`, or model
/// verdict read and write — kept together because every one of those
/// paths walks the same quartet in the same order.
#[derive(Default)]
pub(crate) struct ConfigWrites {
    /// Where live config changes persist, when a host wires it.
    ///
    /// Write-through's target: every command that changes a config
    /// value — `/set`, `/unset`, `/theme`, a `/model` switch — saves
    /// the whole configuration here, so the next launch opens where
    /// this session left off. `None` (a bare test app) changes
    /// nothing on any disk.
    pub(crate) config_persist_dir: Option<std::path::PathBuf>,

    /// The configuration as the file states it, for write-through.
    ///
    /// The session-effective config mixes in run-scoped CLI overrides
    /// and the selected profile's expanded `[api]`; persisting it would
    /// write both into the user's file (an `--unsafe-paths` run leaving
    /// `unsafe_paths = true` behind; a profile's endpoint and credential
    /// freezing into `[api]` for profiles that name none of their own).
    /// This copy is the file as loaded, before overrides and selections,
    /// and carries only what a user word actually changed — `/set`,
    /// `/unset`, `/theme`, a `/model` verdict's intended selection — so
    /// `persist_config` writes user settings,
    /// not the session. `None` (a host that installed none) falls back
    /// to persisting the live config, today's behavior.
    pub(crate) config_file: Option<DchConfig>,

    /// Config values overridden this session, each mapped to its original.
    ///
    /// `/set` records a key's value the first time it is changed so `/unset`
    /// can restore exactly what the file gave, and not merely some default.
    /// The map is the session's ledger of overrides — nothing is written back
    /// to the file, so a restart starts again from disk. An entry's `None`
    /// records that the file carried no value for the key at all, so the
    /// restore reaches the same absence instead of a value spelled like one.
    pub(crate) config_overrides: std::collections::HashMap<String, Option<String>>,

    /// The user-stated config a model selection layers onto, when the
    /// host installed one.
    ///
    /// Selections must not accrete: a second switch layers its entry
    /// over the base the session started from — the file plus the CLI's
    /// non-model overrides — never over the live config's
    /// already-selected bundle, so `/model b` mid-session equals a
    /// restart that named `b`. The host installs the boot-time base
    /// beside the live config; `/set` and `/unset` mirror their writes
    /// into it so a later switch cannot drop them. `None` until the
    /// first verdict needs it — a panel nobody boot-selected snapshots
    /// the live config at that moment, which for exactly those panels
    /// is the file's own `[api]`.
    pub(crate) config_base: Option<DchConfig>,
}

/// The saved-session picker `/sessions` stands on.
///
/// The catalog the driver listed with the cursor's row. Rows arrive
/// newest-first and never change while the picker stands — resuming
/// closes it — so the cursor indexes a frozen list, exactly the
/// completion cycle's discipline.
pub(crate) struct SessionPicker {
    /// The catalog, newest first.
    ///
    /// What the driver's listing read; the picker never edits it.
    pub(crate) rows: Vec<SessionPick>,

    /// The row the cursor stands on.
    ///
    /// Wraps at both ends when the arrows walk; Enter resumes this
    /// row's session.
    pub(crate) cursor: usize,
}

/// The configured-models picker `/models` stands on.
///
/// The same floating shape the session picker takes: the model names
/// with the cursor's row. Enter switches to the highlighted model
/// through the same `/model` lane; Esc closes. The running model is
/// marked beside its name.
pub(crate) struct ModelPicker {
    /// The configured model names, in `[[models]]` order.
    ///
    /// Read from the display's own config — no driver round-trip —
    /// and never edited while the picker stands.
    pub(crate) rows: Vec<String>,

    /// The row the cursor stands on.
    ///
    /// Wraps at both ends; Enter switches to this model.
    pub(crate) cursor: usize,
}
/// Fit a session's name to the picker's title column.
///
/// The name is the reason a row is readable, so it stands right of
/// the short id and before the model — but never at the cost of the
/// row it shares: an over-long title keeps its head with an
/// ellipsis, and a session without one (saved before titles existed,
/// or a naming call that never answered) takes a dash so the column
/// stays aligned across rows. A title is prose in any script, so
/// both the pad and the cap measure display cells — the same
/// measure the pane spends — never characters, and the title
/// carries the display policy first, so a joiner cluster spends
/// what it paints.
pub(crate) fn fit_picker_title(title: Option<&str>) -> String {
    const WIDTH: usize = 24;
    let text = for_display(title.unwrap_or("—"));
    let cells = str_width(&text);
    if cells <= WIDTH {
        let pad = " ".repeat(WIDTH.saturating_sub(cells));
        return format!("{text}{pad}");
    }
    let truncated = format!("{}…", prefix_within(&text, WIDTH.saturating_sub(1)));
    let used = str_width(&truncated);
    format!("{truncated}{}", " ".repeat(WIDTH.saturating_sub(used)))
}

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::commands::{self, Command};
    use crate::config_fields;
    use crate::message::TuiMessage;
    use crate::observer::{CompactionEvent, TurnPublication};
    use crate::queue::{SessionCommand, SessionPick, SessionUpdate};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn a_live_resume_releases_the_replaced_conversation_s_debts() {
        // The interleaving the debt list cannot survive: a resume
        // replaces the conversation wholesale, so an owed index names a
        // row that no longer exists — and a turn publishing the same
        // pass count over the resumed history would pin its summary
        // onto whatever now sits at that index. The replacement
        // releases the debt, and the resumed rows keep their own words.
        let mut app = app();
        app.state
            .compactions
            .lock()
            .expect("the compaction lock")
            .push(CompactionEvent {
                pass: 1,
                record: compacted_row("threshold exceeded"),
            });
        app.drain_shared_state();
        assert_eq!(
            app.compaction_summary_owed,
            vec![(1, 0)],
            "the debt stands before the resume"
        );
        app.apply_session_update(crate::queue::SessionUpdate::Resumed {
            id: "resumed-id".to_string(),
            transcript: vec![TuiMessage::Compacted {
                tokens_before: 20,
                tokens_after: 8,
                reason: "the resumed session's own pass".to_string(),
                evicted: 2,
                duration_ms: 90,
                compactor: Some("structured".to_string()),
                compression_ratio: Some(2.5),
                summary: Some("the resumed session's own summary".to_string()),
                timestamp: chrono::Utc::now(),
            }],
            tokens: crate::queue::ResumedTokens {
                cumulative_input: 11,
                cumulative_output: 7,
                last_input: 5,
                context_tokens: 3,
            },
            permission_mode: dch_config::PermissionMode::Auto,
        });
        assert!(
            app.compaction_summary_owed.is_empty(),
            "the replacement releases the replaced conversation's debts"
        );
        let history = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- a later turn's publication",
        )]);
        app.state
            .turn_histories
            .lock()
            .expect("the turn-history lock")
            .push(TurnPublication { passes: 1, history });
        app.drain_shared_state();
        match app.conversation().first() {
            Some(TuiMessage::Compacted { summary, .. }) => assert!(
                summary
                    .as_deref()
                    .is_some_and(|text| text.contains("the resumed session's own summary")),
                "the released debt never backfills the resumed row: {summary:?}"
            ),
            other => panic!("the resumed compacted row stands: {other:?}"),
        }
    }

    #[test]
    fn a_cleared_session_owes_nothing_from_the_one_before_it() {
        // `/clear` is the other replacement: the fresh conversation owes
        // no summary from the one it displaces, and a publication
        // arriving afterwards finds no debtor to serve.
        let mut app = app();
        app.state
            .compactions
            .lock()
            .expect("the compaction lock")
            .push(CompactionEvent {
                pass: 1,
                record: compacted_row("threshold exceeded"),
            });
        app.drain_shared_state();
        assert_eq!(
            app.compaction_summary_owed,
            vec![(1, 0)],
            "the debt stands before the clear"
        );
        app.clear_command();
        assert!(
            app.compaction_summary_owed.is_empty(),
            "the clear releases the debt with the conversation it named"
        );
        assert!(
            app.conversation().is_empty(),
            "the clear emptied the display"
        );
        let history = std::sync::Arc::new(vec![loopctl::message::Message::user(
            "## Conversation summary (compacted)\n- a late publication",
        )]);
        app.state
            .turn_histories
            .lock()
            .expect("the turn-history lock")
            .push(TurnPublication { passes: 1, history });
        app.drain_shared_state();
        assert!(
            app.conversation().is_empty(),
            "a publication after the clear attaches nothing — no row \
             exists to take it"
        );
    }

    #[test]
    fn version_command_prints_the_running_version() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/version");
        assert!(out.contains("dch "), "got {out:?}");
        assert!(out.contains(env!("CARGO_PKG_VERSION")), "got {out:?}");
    }

    #[test]
    fn themes_command_lists_the_settable_command_names() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/themes");
        // The live palette (the fallback theme) is starred by its command key —
        // the very word you would pass to `/theme`, not its pretty label.
        assert!(out.contains("* dch"), "got {out:?}");
        for key in crate::theme::Theme::command_names() {
            assert!(out.contains(key), "missing command name {key} in {out:?}");
        }
        // The display spelling (`"Gruvbox Dark"`) is deliberately not listed.
        assert!(
            !out.contains("Gruvbox Dark"),
            "lists keys, not display labels"
        );
    }

    #[test]
    fn verbosity_command_reports_then_sets() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // No argument: report the current level, change nothing.
        let report = command_output(&mut app, "/verbosity");
        assert!(report.contains("verbosity is"), "got {report:?}");
        // A level: apply it to the live display through the shared setter.
        command_output(&mut app, "/verbosity verbose");
        assert!(matches!(
            app.display.verbosity,
            dch_config::Verbosity::Verbose
        ));
    }

    #[test]
    fn clear_empties_the_transcript_and_offers_a_fresh_session() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let now = chrono::Utc::now();
        app.push_message(TuiMessage::User {
            text: "keep?".to_string(),
            timestamp: now,
            steered: false,
            unsent: false,
        });
        app.push_message(TuiMessage::System {
            text: "a note".to_string(),
            timestamp: now,
        });
        let out = command_output(&mut app, "/clear");
        assert!(app.conversation.is_empty(), "the transcript is emptied");
        assert!(out.contains("session cleared"), "got {out:?}");
    }

    #[test]
    fn mode_names_sets_and_refuses() {
        // The short door over `/set permission_mode`: bare names the
        // mode in effect, a spelling lands through the setter's own
        // machinery — its notice, its ledger, its persist — and an
        // unknown spelling is refused with the mode kept.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // A driver channel from the start, so every landed move also
        // crosses to the loop and the notices stay the setter's own.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.host.submit_tx = Some(tx);
        let out = command_output(&mut app, "/mode");
        assert!(
            out.contains("mode is auto"),
            "the bare form names the mode in effect: {out}"
        );
        let out = command_output(&mut app, "/mode plan");
        assert!(
            out.contains("permission_mode = read_only"),
            "the notice names the canonical spelling in force — a retired \
             alias lands as its replacement: {out}"
        );
        assert_eq!(
            app.config.runner.permission_mode,
            dch_config::PermissionMode::ReadOnly,
            "the session config carries the mode"
        );
        let sent = rx.try_recv().expect("/mode hands the move to the driver");
        let crate::queue::Submission::Command { command } = sent else {
            panic!("the mode travels as a command, got {sent:?}")
        };
        let SessionCommand::Mode { mode } = command else {
            panic!("the command is a mode move, got {command:?}")
        };
        assert_eq!(
            mode,
            dch_config::PermissionMode::ReadOnly,
            "the loop is asked for the mode just saved"
        );
        let out = command_output(&mut app, "/mode bogus");
        assert!(
            out.contains("could not set permission_mode"),
            "an unknown spelling is refused: {out}"
        );
        assert!(
            rx.try_recv().is_err(),
            "a refused spelling never reaches the loop"
        );
        let (_, candidates, _) = app.fresh_completion("/mode ");
        assert!(
            candidates.contains(&"auto".to_string())
                && candidates.contains(&"read_only".to_string()),
            "completion offers the mode's spellings: {candidates:?}"
        );

        // A second spelling lands and crosses the same way — and mid-run
        // the mode moves live: the shared cell the gate re-reads flips
        // on the spot, so the very next tool call runs under it.
        let cell: dch_config::SharedPermissionMode =
            std::sync::Arc::new(std::sync::Mutex::new(dch_config::PermissionMode::Auto));
        app.set_permission_mode_cell(Some(std::sync::Arc::clone(&cell)));
        app.state
            .agent_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let out = command_output(&mut app, "/mode plan");
        assert!(
            out.contains("permission_mode = read_only"),
            "a run in flight still moves the mode live — and the notice \
             names the canonical spelling, not the alias: {out}"
        );
        assert_eq!(
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dch_config::PermissionMode::ReadOnly,
            "the gate's cell flips on the spot — the next dispatch reads it"
        );
        assert_eq!(
            app.config.runner.permission_mode,
            dch_config::PermissionMode::ReadOnly,
            "the session config carries the live move"
        );
    }

    #[test]
    fn bare_preset_lists_the_configured_presets() {
        // The listing the spec promises: bare `/preset` reaches the
        // listing branch now that the parser admits it (the arg shape
        // matches `/mode`) — naming the configured presets and the
        // live selection; with none configured it says so instead.
        let mut app = TuiApp::new(
            dch_config::DchConfig::from_toml_str(concat!(
                "[permissions]\n",
                "preset = \"rust-dev\"\n",
                "\n",
                "[permissions.presets.rust-dev]\n",
                "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
            ))
            .expect("the config parses"),
        );
        app.preset_command(None);
        let (text, _) = app
            .transient_notice
            .as_ref()
            .expect("the listing posts a notice")
            .clone();
        assert!(
            text.contains("presets: rust-dev") && text.contains("selected: rust-dev"),
            "the listing names the presets and the live selection: {text}"
        );

        let mut empty = TuiApp::new(dch_config::DchConfig::default());
        empty.preset_command(None);
        let (text, _) = empty
            .transient_notice
            .as_ref()
            .expect("the empty listing posts a notice")
            .clone();
        assert!(
            text.contains("no [permissions.presets] configured"),
            "an empty preset table is named, not listed silently: {text}"
        );
    }

    #[test]
    fn a_preset_selection_waits_for_repair_and_takes_effect_at_it() {
        // The round-4 damage interaction, end to end: a healthy swap
        // joins the new preset's rules at once; under a standing
        // damage verdict the swap records the selection and holds —
        // the config layer stays tightened, so the new preset's rules
        // cannot release anything ahead of the repair — and the
        // repair re-seeds at the selection the session actually
        // chose.
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
        let mut app = TuiApp::new(config.clone());
        let rules_cell =
            std::sync::Arc::new(std::sync::Mutex::new(config.permissions.engine_rules()));
        let preset_cell =
            std::sync::Arc::new(std::sync::Mutex::new(config.permissions.preset.clone()));
        app.set_permission_config_rules_cell(Some(std::sync::Arc::clone(&rules_cell)));
        app.set_permission_preset_cell(Some(std::sync::Arc::clone(&preset_cell)));
        // The swap runs under the driver's adoption guard in production;
        // hand the app the same lane so the paths below exercise it —
        // and prove at the end that every swap gave it back.
        let adoption_guard = std::sync::Arc::new(std::sync::Mutex::new(()));
        app.set_permission_adoption_guard(Some(std::sync::Arc::clone(&adoption_guard)));

        app.preset_command(Some("p2"));

        let held_join = config.permissions.engine_rules_at(Some("p2"));
        assert_eq!(
            *rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            held_join,
            "a healthy swap joins the new preset's rules at once"
        );
        assert_eq!(
            *preset_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some("p2".to_string()),
            "the swap records the selection the driver re-seeds from"
        );
        app.permission
            .permission_memory
            .adopt(Vec::new(), Vec::new(), true);

        app.preset_command(Some("p1"));

        assert_eq!(
            *rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            held_join,
            "the held swap leaves the tightened cell exactly as it stood"
        );
        assert_eq!(
            *preset_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some("p1".to_string()),
            "the selection is recorded even while its rules wait"
        );
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the held swap answers with a notice");
        assert!(
            notice.contains("wait for /permissions repair"),
            "the notice names the door the rules wait at: {notice}"
        );

        app.permissions_command(Some("repair"));

        assert_eq!(
            *rules_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            config.permissions.engine_rules_at(Some("p1")),
            "the repair re-seeds at the selection in effect — the swap the \
             damage held takes effect with it"
        );

        assert!(
            adoption_guard.try_lock().is_ok(),
            "a finished swap always releases the adoption guard — a leak \
             would deadlock the next adoption"
        );
    }

    #[test]
    fn the_listing_names_the_selected_preset_rules_and_numbers_them() {
        // The gate evaluates the preset's rules beside the top-level
        // ones, so the listing shows both: the preset's rows marked
        // with their origin and numbered into the same sequence — and
        // `forget` on a preset row answers the configured wording, the
        // preset being config-side like the file's own rules.
        let mut config = dch_config::DchConfig::default();
        config.permissions.rules = vec![dch_config::PermissionRuleConfig {
            id: Some("no-env-writes".to_string()),
            outcome: dch_config::PermissionRuleOutcome::Deny,
            tool: None,
            shell_prefix: None,
            path_glob: Some("**/.env".to_string()),
        }];
        config.permissions.preset = Some("rust-dev".to_string());
        config.permissions.presets = std::collections::BTreeMap::from([(
            "rust-dev".to_string(),
            dch_permissions::Preset {
                rules: vec![dch_config::PermissionRuleConfig {
                    id: Some("deny-cargo-publish".to_string()),
                    outcome: dch_config::PermissionRuleOutcome::Deny,
                    tool: None,
                    shell_prefix: Some("cargo publish".to_string()),
                    path_glob: None,
                }],
            },
        )]);
        let mut app = TuiApp::new(config);

        app.permissions_command(None);
        let texts: Vec<String> = app
            .conversation()
            .iter()
            .filter_map(|message| match message {
                TuiMessage::System { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let joined = texts.join("\n");
        assert!(
            joined.contains("from preset rust-dev"),
            "the preset's rules are listed with their origin: {joined}"
        );
        assert!(
            joined.contains("2. deny shell_prefix cargo publish"),
            "the preset rule carries the number after the top-level rule: {joined}"
        );

        app.permissions_command(Some("forget 2"));
        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the preset-row ask answers with a notice");
        assert!(
            notice.contains("rule 2 is configured"),
            "a preset rule is config-side — forget answers the configured wording: {notice}"
        );
    }

    #[test]
    fn help_lists_the_palette_and_its_footer() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/help");
        assert!(out.contains("In-session commands"), "got {out:?}");
        assert!(out.contains("/clear"), "the listing shows a known command");
        assert!(out.contains("/mode"), "the short mode door is listed");
        assert!(
            out.contains("/help <command>"),
            "the footer points at per-command help, got {out:?}"
        );
    }

    #[test]
    fn help_explains_one_command_and_flags_a_missing_topic() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let detail = command_output(&mut app, "/help model");
        assert!(detail.contains("/model"), "got {detail:?}");
        // An unknown topic points back at the list rather than inventing one.
        let miss = command_output(&mut app, "/help zzz");
        assert!(miss.contains("no command /zzz"), "got {miss:?}");
    }

    #[test]
    fn set_theme_applies_a_name_and_keeps_the_current_on_a_miss() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // `/theme nord` resolves through by_name; the palette's own `name` is
        // its display spelling ("Nord"), which is what the notice and field show.
        let hit = command_output(&mut app, "/theme nord");
        assert!(
            !hit.contains("theme set to"),
            "the switch is silent: {hit:?}"
        );
        assert_eq!(app.theme.name, "Nord");
        assert_eq!(
            app.config.display.theme, "nord",
            "the config and the file carry the registry key — the one spelling \
             /themes lists and the config documents, never the display name"
        );
        let miss = command_output(&mut app, "/theme notarealtheme");
        assert!(
            miss.contains("unknown theme 'notarealtheme'"),
            "got {miss:?}"
        );
        assert_eq!(app.theme.name, "Nord", "a miss keeps the current palette");
        assert_eq!(
            app.config.display.theme, "nord",
            "and a miss keeps the stored key"
        );
    }

    #[test]
    fn theme_without_a_name_steps_the_palette() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let before = app.theme.name;
        let out = command_output(&mut app, "/theme");
        assert!(
            !out.contains("theme preview"),
            "the step is silent — the palette change is the feedback: {out:?}"
        );
        assert_ne!(
            app.theme.name, before,
            "the bare command steps to the next palette"
        );
    }

    #[test]
    fn tab_completes_a_theme_argument_to_its_command_name() {
        // `/theme dark` then Tab offers the command spelling `dark_plus` — the
        // word `/theme` accepts and the config file writes, not the display
        // label ("Dark Plus"). Completion and the command share one
        // vocabulary.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.input.set_text("/theme dark".to_string());
        assert!(app.complete_command_line(false));
        assert_eq!(app.composer.input.text(), "/theme dark_plus");
    }

    #[test]
    fn a_browse_left_by_clearing_restores_the_theme() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.input.set_text("/theme ".to_string());
        let original = app.theme.name;
        assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert!(
            app.theme.name != original,
            "the arrow previewed a different theme"
        );
        // Ctrl+C clears the draft; the browse is gone, the theme returns.
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert_eq!(
            app.theme.name, original,
            "the browse the clear ended restores the session's theme"
        );
    }

    #[test]
    fn esc_on_a_popup_clears_the_whole_command() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.composer.input.set_text("/the".to_string());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            app.handle_key(esc),
            "the standing popup is Esc's first duty"
        );
        assert!(
            app.composer.input.is_empty(),
            "one press clears the suggestion and the draft together"
        );
        assert!(!app.completion_popup_live(), "the popup is gone");
        assert!(
            !app.handle_key(esc),
            "an empty composer leaves Esc unclaimed"
        );
    }

    #[test]
    fn models_without_profiles_says_the_model_renames_directly() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/models");
        assert!(out.contains("no [[models]] configured"), "got {out:?}");
    }

    #[test]
    fn config_lists_the_values_in_effect() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/config");
        assert!(out.contains("Configuration in effect"), "got {out:?}");
        assert!(out.contains("model ="), "the model row shows, got {out:?}");
        // The footer's live claim is pinned against the classification
        // itself, not a copied string: every key `live` classes Display
        // must be named in the footer, so the next Display key cannot
        // ship unmentioned the way diff_layout did.
        let footer = out
            .lines()
            .find(|line| line.contains("change live"))
            .expect("the footer explains what changes live");
        for key in config_fields::KEYS {
            if matches!(config_fields::live(key), Some(config_fields::Live::Display)) {
                assert!(
                    footer.contains(key),
                    "the footer names {key} among the live rows: {footer}"
                );
            }
        }
    }

    #[test]
    fn set_writes_a_startup_read_value_for_the_next_session() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/set auto_compact false");
        assert!(
            out.contains("saved") && out.contains("next session"),
            "the notice names honestly when a startup-read value lands: {out:?}"
        );
        assert!(
            !app.config.runner.auto_compact,
            "the value is written to the session's config, ready for the next start"
        );
        // And /unset hands back what the file gave.
        let out = command_output(&mut app, "/unset auto_compact");
        assert!(out.contains("restored to true"), "got {out:?}");
        assert!(app.config.runner.auto_compact, "the original is back");
    }

    #[test]
    fn unset_restores_an_absent_value_to_absence() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        assert!(app.config.api.reasoning_effort.is_none(), "absent at rest");
        command_output(&mut app, "/set reasoning_effort high");
        assert_eq!(
            app.config.api.reasoning_effort.as_deref(),
            Some("high"),
            "the override stands in"
        );
        let out = command_output(&mut app, "/unset reasoning_effort");
        assert!(out.contains("restored to unset"), "got {out:?}");
        assert!(
            app.config.api.reasoning_effort.is_none(),
            "the restore reaches the same absence the file spelled by omission, not the literal word"
        );
    }

    #[test]
    fn unset_restores_an_absent_api_key_to_none() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        assert!(app.config.api.api_key.is_none(), "absent at rest");
        command_output(&mut app, "/set api_key sk-live");
        assert_eq!(app.config.api.api_key.as_deref(), Some("sk-live"));
        let out = command_output(&mut app, "/unset api_key");
        assert!(
            out.contains("restored to unset"),
            "an absent original is named as the absence it is, not masked \
             as a restored credential: {out:?}"
        );
        assert!(
            !out.contains("••••"),
            "bullets over an absence read as a restored value: {out:?}"
        );
        assert!(
            app.config.api.api_key.is_none(),
            "an absent key does not come back as an empty value shadowing the environment credential"
        );
    }

    #[test]
    fn sessions_parse_and_refuse_under_a_run() {
        let commands::Parsed::Command(command) = commands::parse("/sessions") else {
            panic!("the bare spelling is the whole command");
        };
        assert!(matches!(command, Command::Sessions));
        assert!(
            matches!(
                commands::parse("/sessions now"),
                commands::Parsed::Misused { .. }
            ),
            "the command takes no argument"
        );

        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.state
            .agent_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);
        let out = command_output(&mut app, "/sessions");
        assert!(
            out.contains("commands run between turns"),
            "the picker asks between turns, like every conversation mover: {out:?}"
        );
        assert!(rx.try_recv().is_err(), "no listing travels under a run");
    }

    #[test]
    fn the_session_picker_walks_and_enter_resumes_the_chosen_row() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let rows = vec![
            SessionPick {
                id: "11111111-1111-1111-1111-111111111111".to_string(),
                model: "newest-model".to_string(),
                saved_at: chrono::Utc::now(),
                prompts: 3,
                title: None,
                workdir: None,
            },
            SessionPick {
                id: "22222222-2222-2222-2222-222222222222".to_string(),
                model: "older-model".to_string(),
                saved_at: chrono::Utc::now(),
                prompts: 7,
                title: None,
                workdir: None,
            },
        ];
        app.apply_session_update(SessionUpdate::SessionCatalog { rows });
        assert!(
            app.pickers.session_picker.is_some(),
            "a catalog with rows opens the picker"
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            app.handle_key(down),
            "the arrows are the picker's while it stands"
        );
        assert!(app.handle_key(enter), "Enter takes the row");
        assert!(
            app.pickers.session_picker.is_none(),
            "resuming closes the picker"
        );
        let sent = rx
            .try_recv()
            .expect("Enter handed the row to the resume lane");
        let crate::queue::Submission::Command { command } = sent else {
            panic!("the picker's Enter sends a command, got {sent:?}")
        };
        let SessionCommand::Resume { id } = command else {
            panic!("the chosen row travels as a resume, got {command:?}")
        };
        assert_eq!(
            id, "22222222-2222-2222-2222-222222222222",
            "the walked-to row is the one that resumes"
        );

        // An empty catalog says so without opening a sheet, and Esc
        // closes a standing picker.
        app.apply_session_update(SessionUpdate::SessionCatalog { rows: Vec::new() });
        assert!(
            app.pickers.session_picker.is_none(),
            "an empty catalog never opens the sheet"
        );
        app.apply_session_update(SessionUpdate::SessionCatalog {
            rows: vec![SessionPick {
                id: "33333333-3333-3333-3333-333333333333".to_string(),
                model: "m".to_string(),
                saved_at: chrono::Utc::now(),
                prompts: 1,
                title: None,
                workdir: None,
            }],
        });
        assert!(app.pickers.session_picker.is_some());
        assert!(app.handle_key(esc), "Esc is the picker's");
        assert!(
            app.pickers.session_picker.is_none(),
            "Esc closes the picker"
        );
    }

    #[test]
    fn resume_refuses_while_a_run_is_in_flight() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.state
            .agent_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);
        let id = uuid::Uuid::new_v4().to_string();
        let out = command_output(&mut app, &format!("/resume {id}"));
        assert!(
            out.contains("commands run between turns"),
            "the registry's refusal promise holds: {out:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing travels to the driver under a run"
        );
    }

    #[test]
    fn a_set_value_is_overridable_and_unset_restores_the_original() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        // Default verbosity is Normal; set it to Verbose, then undo exactly.
        command_output(&mut app, "/set verbosity verbose");
        assert!(
            matches!(app.display.verbosity, dch_config::Verbosity::Verbose),
            "the set applies to the live display"
        );
        let out = command_output(&mut app, "/unset verbosity");
        assert!(out.contains("restored"), "got {out:?}");
        assert!(
            !matches!(app.display.verbosity, dch_config::Verbosity::Verbose),
            "unset reverts to the value before the override"
        );
    }

    #[test]
    fn unset_of_a_never_overridden_key_says_so() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/unset theme");
        assert!(out.contains("no session override"), "got {out:?}");
    }

    #[test]
    fn a_resume_target_that_is_not_a_session_id_is_refused_locally() {
        // The driver's load keys on the id alone, so a target that is not
        // one is refused before anything is sent: the notice names the
        // shape /resume takes, and the channel carries nothing.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);
        let out = command_output(&mut app, "/resume 7");
        assert!(out.contains("not a session id"), "got {out:?}");
        assert!(
            rx.try_recv().is_err(),
            "a malformed target sends the driver nothing"
        );
        let bare = command_output(&mut app, "/resume");
        assert!(bare.contains("which session?"), "got {bare:?}");
    }

    #[test]
    fn a_well_formed_resume_target_sends_the_driver_the_swap() {
        // The live swap's send half: an id that parses travels to the
        // driver untouched, and the transcript stays free of
        // slash-command rows.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);
        let id = uuid::Uuid::new_v4().to_string();
        let out = command_output(&mut app, &format!("/resume {id}"));
        assert!(out.contains("resuming"), "got {out:?}");
        let sent = rx
            .try_recv()
            .expect("the resume command reaches the driver");
        assert!(
            matches!(
                sent,
                crate::queue::Submission::Command {
                    command: crate::queue::SessionCommand::Resume { .. }
                }
            ),
            "the driver gets the live swap: {sent:?}"
        );
    }

    #[test]
    fn the_help_listing_renders_in_the_registrys_grouped_order() {
        // The palette's own order is the listing's order: /help renders
        // the registry grouped by category without sorting anything of
        // its own, so the two cannot drift apart.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/help");
        let help_at = out.find("Help:").expect("the Help heading leads");
        let sessions_at = out.find("Sessions:").expect("the Sessions heading follows");
        let appearance_at = out
            .find("Appearance:")
            .expect("the Appearance heading sits mid-list");
        let config_at = out
            .find("Configuration:")
            .expect("the Configuration heading trails");
        let context_at = out.find("Context:").expect("the Context heading closes");
        assert!(
            help_at < sessions_at
                && sessions_at < appearance_at
                && appearance_at < config_at
                && config_at < context_at,
            "the listing keeps the palette's deliberate order: {out:?}"
        );
    }

    #[test]
    fn a_mode_move_never_reaches_the_persisted_config() {
        // The mode is session state: `/set permission_mode` moves the
        // live config and the session file remembers it — the user's
        // config file never hears of it, so what one session chose
        // cannot govern the next. A later non-mode `/set` persists the
        // whole file copy, and that copy still carries no mode.
        let dir = tempfile::tempdir().expect("persist tempdir");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        app.set_config_command("permission_mode", "interactive");
        assert_eq!(
            app.config.runner.permission_mode,
            dch_config::PermissionMode::Interactive,
            "the mode moves for this session"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .expect("the persist dir")
                .next()
                .is_none(),
            "a mode move writes nothing to the config directory"
        );

        app.set_config_command("verbosity", "quiet");
        let written: Vec<String> = std::fs::read_dir(dir.path())
            .expect("the persist dir")
            .filter_map(|entry| std::fs::read_to_string(entry.expect("an entry").path()).ok())
            .collect();
        assert!(!written.is_empty(), "a non-mode /set persists the config");
        assert!(
            written.iter().all(|text| !text.contains("\"interactive\"")),
            "the persisted config carries no session's choice: {written:?}"
        );
    }

    #[test]
    fn unset_persists_the_restored_value_to_the_config_file() {
        // The undo is a change like the change it undoes: /unset restores
        // the value configuration had and writes it back, so a restart
        // cannot resurrect the override.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        app.run_command(Command::Set {
            key: "theme".to_string(),
            value: "nord".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the set landed");
        assert!(saved.contains("\"nord\""), "got {saved}");
        app.run_command(Command::Unset {
            key: "theme".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the restore landed");
        assert!(
            !saved.contains("\"nord\""),
            "the restore is written back: {saved}"
        );
    }

    #[test]
    fn a_set_persists_the_file_not_the_run_scoped_overrides() {
        // --unsafe-paths is a per-run statement; a /theme in the same
        // session must not write it into the file the next run loads.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut live = dch_config::DchConfig::default();
        live.runner.unsafe_paths = true; // what --unsafe-paths left in effect
        let mut app = TuiApp::new(live);
        app.set_config_file(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        app.run_command(Command::Set {
            key: "theme".to_string(),
            value: "nord".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the set landed");
        assert!(
            saved.contains("\"nord\""),
            "the user's change is written: {saved}"
        );
        assert!(
            !saved.contains("unsafe_paths = true"),
            "the run-scoped flag never lands: {saved}"
        );
    }

    #[test]
    fn a_preset_selection_is_session_state_and_never_reaches_the_config_file() {
        // The `/preset` spec calls the selection process state — neither
        // a restart nor a launch `--resume` carries it — and the
        // command's own doc says it never reaches the configuration
        // file; the two claims this pins the behavior half of. The
        // selection takes in the session, the next persisting change
        // still lands (the theme proves the save went through), and
        // the file the save wrote carries no `preset` selection — so a
        // restart and every fresh session resolve under the
        // configuration's own selection, exactly what the spec's last
        // sentence promises. A `preset` key here would mean the
        // session-scoped choice leaked into the file.
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dch_config::DchConfig::from_toml_str(concat!(
            "[permissions.presets.rust-dev]\n",
            "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
        ))
        .expect("a preset table parses");
        let mut app = TuiApp::new(live);
        app.set_config_file(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        app.preset_command(Some("rust-dev"));
        assert_eq!(
            app.config.permissions.preset.as_deref(),
            Some("rust-dev"),
            "the selection takes in the session"
        );
        app.run_command(Command::Set {
            key: "theme".to_string(),
            value: "nord".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the set landed");
        assert!(
            saved.contains("\"nord\""),
            "the persisting change is written, so the save went through: {saved}"
        );
        let reloaded = dch_config::DchConfig::from_toml_str(&saved).expect("the file reloads");
        assert!(
            reloaded.permissions.preset.is_none(),
            "the session-scoped selection never lands in the file: {saved}"
        );
    }

    #[test]
    fn a_stand_in_persist_resets_the_session_preset_selection() {
        // The file-wired save cannot leak — it serializes the file's own
        // copy. The stand-in save — a host that wired no config file —
        // serializes the live config, which `/preset` mutated; without
        // the reset the session-scoped selection would ride the save
        // into a file the swap promised never to touch, and outlive the
        // process the spec says it dies with.
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dch_config::DchConfig::from_toml_str(concat!(
            "[permissions.presets.rust-dev]\n",
            "rules = [{ outcome = \"allow\", shell_prefix = \"cargo test\" }]\n",
        ))
        .expect("a preset table parses");
        let mut app = TuiApp::new(live);
        // No `set_config_file` — persist_config takes the stand-in path.
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        app.preset_command(Some("rust-dev"));
        assert_eq!(
            app.config.permissions.preset.as_deref(),
            Some("rust-dev"),
            "the selection takes in the session"
        );
        app.run_command(Command::Set {
            key: "theme".to_string(),
            value: "nord".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the set landed");
        assert!(
            saved.contains("\"nord\""),
            "the persisting change is written, so the save went through: {saved}"
        );
        let reloaded = dch_config::DchConfig::from_toml_str(&saved).expect("the file reloads");
        assert!(
            reloaded.permissions.preset.is_none(),
            "the stand-in save carries no session-scoped selection: {saved}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_persist_names_the_class_honestly() {
        // The two live classes fail differently and the failure notice
        // must say which happened: a display value has already applied
        // and only the save is lost, while a restart-class value applied
        // nothing this session and, unsaved, applies nothing next start
        // either.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("the dir refuses staging");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        let out = command_output(&mut app, "/set max_turns 50");
        assert!(
            out.contains("not saved") && out.contains("never applied this session"),
            "a restart-class failure says nothing applied and nothing will: {out:?}"
        );
        let out = command_output(&mut app, "/set verbosity verbose");
        assert!(
            out.contains("applied for this session only"),
            "a display-class failure says the session kept the change: {out:?}"
        );
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the dir is writable again for cleanup");
    }

    #[test]
    fn set_of_an_unknown_theme_is_refused_whole() {
        // The whole /set theme nonsense story at the command level: the
        // notice refuses naming the word, the config keeps its theme, and
        // nothing reaches the file — no "applied now", no silent
        // fallback at the next launch.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        let out = command_output(&mut app, "/set theme not-a-theme");
        assert!(
            out.contains("could not set theme") && out.contains("not-a-theme"),
            "the refusal names the field and the word: {out:?}"
        );
        assert!(
            !out.contains("applied now"),
            "a refused set never claims it applied: {out:?}"
        );
        assert_eq!(
            app.config.display.theme, "dch",
            "the config keeps its theme"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "a refused set persists nothing"
        );
    }

    #[test]
    fn set_of_an_off_ladder_effort_is_refused_whole() {
        // The command-level half: `/set reasoning_effort hihg` is
        // refused with the ladder named, the config keeps its value,
        // and nothing reaches the file — the next start stays
        // constructible.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        let out = command_output(&mut app, "/set reasoning_effort hihg");
        assert!(
            out.contains("could not set reasoning_effort") && out.contains("hihg"),
            "the refusal names the field and the word: {out:?}"
        );
        assert!(
            app.config.api.reasoning_effort.is_none(),
            "the config keeps its value"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "a refused set persists nothing"
        );
    }

    #[test]
    fn set_of_a_bricking_value_is_refused_whole() {
        // The command-level half of the bricking fix: `/set
        // turn_timeout_secs 60` is refused at the field gate with the
        // loader's floor named, the config keeps its value, and the file
        // is never written — the next start stays possible.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        let out = command_output(&mut app, "/set turn_timeout_secs 60");
        assert!(
            out.contains("could not set turn_timeout_secs") && out.contains("120"),
            "the refusal names the field and the floor: {out:?}"
        );
        assert_eq!(
            app.config.runner.turn_timeout_secs, 300,
            "the config keeps its value"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "a refused set persists nothing"
        );
    }
}
