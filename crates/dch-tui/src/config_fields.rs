//! The settable configuration surface behind `/set`, `/unset`, and `/config`.
//!
//! The composer lets a value be overridden for the running session without
//! editing and restarting on the config file. Every editable value is named
//! here by one key spelling — the same words [`KEYS`] offers for completion,
//! what `/config` prints, and what `/set` accepts — so the three commands and
//! `Tab` all speak one vocabulary and cannot drift apart.
//!
//! Setting goes through serde, exactly as the config file does: a scalar parses
//! as its type, an enum as its renamed string (`verbosity = "verbose"` reads the
//! same way `verbose` is typed after `/set verbosity`). Reusing the config's own
//! (de)serialization is what keeps the accepted spellings identical to the file
//! format — the command cannot accept a word the file would reject, or reject
//! one the file would take.
//!
//! Structured values (the `[[models]]` profiles, the MCP server list, role
//! overrides) are *not* settable one-key at a time and are deliberately absent:
//! they are edited as blocks in the file, and a partial single-key write could
//! leave one half of a profile out of step with the other. Three scalar
//! leaves are deliberately absent too: `[runner] role` (edited beside its
//! overrides block), `[api] azure_resource` (part of the provider's own
//! block), and `[runner] unsafe_paths` (the operator-only containment
//! opt-out, fixed at runner construction) — the table here is exhaustive
//! over the keys the surface takes, not over the file's every leaf.

use dch_config::{
    ApiType, CaretDisplay, CompactorSelection, DchConfig, DiffLayout, DiffPalette, PathStyle,
    PermissionMode, StripPosition, StripStyle, TURN_TIMEOUT_FLOOR_SECS, Verbosity,
};

/// How a changed value takes effect, so the command can tell the user
/// honestly.
///
/// The classification `/set`'s notice reads: it separates the values
/// the running panel can adopt from the ones only a fresh start reads,
/// so the wording can name when a change lands instead of implying a
/// liveness nothing delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// The running panel can adopt it now — applied to its own state.
    ///
    /// The theme, the verbosity, the caret display, and the diff
    /// layout: fields the panel re-reads — the first two from caches
    /// the setters re-bake, the caret and the diff layout straight off
    /// the config each render — so the change shows on the next draw
    /// and `/unset`'s restore does too.
    Display,

    /// Nothing running reads it live from here; it applies to the next session.
    ///
    /// Read at construction — by the loop, the HTTP client, or a
    /// spawned process — so the write lands in the config now and the
    /// next `dch` start picks it up.
    Restart,
}

/// The keys `/set` and `/unset` accept and `/config` lists, in display order.
///
/// The names are the leaf fields grouped by their config section; they are the
/// whole vocabulary of the value commands.
pub const KEYS: &[&str] = &[
    "model",
    "base_url",
    "api_type",
    "max_tokens",
    "context_window",
    "request_timeout_secs",
    "fallback_model",
    "reasoning_effort",
    "prompt_cache",
    "api_key",
    "verbosity",
    "theme",
    "no_color",
    "strip_position",
    "strip_style",
    "path_style",
    "diff_layout",
    "diff_palette",
    "mouse_capture",
    "caret_display",
    "copy_command",
    "max_turns",
    "turn_timeout_secs",
    "auto_compact",
    "compact_threshold",
    "compactor",
    "permission_mode",
    "parallel_tool_dispatch",
    "read_max_lines",
    "read_max_bytes",
    "max_tool_output_chars",
    "log_level",
    "json_logs",
    "redact_secrets",
];

/// Whether the value is secret and must be masked when displayed.
///
/// Only `api_key` today: `/config`'s survey and `/unset`'s notice print
/// bullets for it, never the value, so a shoulder-surfer or a shared
/// screenshot learns nothing.
#[must_use]
pub fn is_secret(key: &str) -> bool {
    key == "api_key"
}

/// How setting `key` takes effect, or `None` when `key` is not settable.
///
/// A display value — the theme and the verbosity — can be pushed into a
/// running session, because those are the fields the panel itself re-reads.
/// Every other value is read at construction (by the loop, the HTTP client,
/// or a spawned process), so `/set` writes it — the session's config and the
/// file — and it reaches the running loop the next time dch starts; the
/// notice names which of the two landed rather than implying a live change
/// it cannot make. (The model is the one loop value a user can move now,
/// but through `/model`, which drives the loop directly — not through this
/// table.)
#[must_use]
pub fn live(key: &str) -> Option<Live> {
    match key {
        "theme" | "verbosity" | "caret_display" | "diff_layout" | "diff_palette"
        | "strip_position" | "strip_style" | "path_style" | "copy_command" => Some(Live::Display),
        _ if is_key(key) => Some(Live::Restart),
        _ => None,
    }
}

/// The fixed values a key accepts, as completion offers them.
///
/// Enum keys spell their variants and boolean keys spell their two
/// words — the same spellings [`set`] accepts and the config file
/// documents, so what a completion offers is always pasteable. Keys
/// whose values are free-form (names, numbers, secrets) carry no
/// fixed vocabulary to offer and answer `None`.
#[must_use]
pub fn value_candidates(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "api_type" => &[
            "ollama",
            "openai",
            "anthropic",
            "gemini",
            "deepseek",
            "grok",
            "azure",
            "moonshot",
            "zai",
            "bedrock",
            "self_hosted",
        ],
        "verbosity" => &["quiet", "normal", "verbose"],
        "strip_position" => &["composer", "top"],
        "strip_style" => &[
            "breathe",
            "sweep",
            "bounce",
            "travel",
            "snake",
            "worm",
            "worm_long",
            "off",
            "glow",
        ],
        "path_style" => &["full", "relative"],
        "diff_layout" => &["side-by-side", "inline"],
        "diff_palette" => &["classic", "soft", "blue-orange"],
        "compactor" => &["truncating", "qa", "structured"],
        "permission_mode" => &["auto", "plan", "accept_edits", "interactive"],
        "no_color" | "mouse_capture" | "auto_compact" => &["true", "false"],
        _ => return None,
    })
}

/// The current value of `key` rendered as the file would spell it, or `None`.
///
/// Scalars render as their literals, enums as their renamed strings,
/// and an absent optional as the word `unset` — the shape `/config`
/// prints, and a display word only: [`set`] refuses it for the optional
/// string keys rather than storing it, and `/unset` is the command that
/// actually clears. `None` means the key names no settable value.
#[must_use]
pub fn get(config: &DchConfig, key: &str) -> Option<String> {
    // Scalars render plainly; Option renders `unset` when there is no value.
    let rendered: String = match key {
        "model" => config.api.model.clone(),
        "base_url" => config.api.base_url.clone(),
        "api_type" => name_of(&config.api.api_type),
        "max_tokens" => config.api.max_tokens.to_string(),
        "context_window" => opt_render(config.api.context_window),
        "request_timeout_secs" => config.api.request_timeout_secs.to_string(),
        "fallback_model" => opt_render(config.api.fallback_model.as_ref()),
        "reasoning_effort" => opt_render(config.api.reasoning_effort.as_ref()),
        "prompt_cache" => config.api.prompt_cache.to_string(),
        "api_key" => opt_render(config.api.api_key.as_deref()),
        "verbosity" => name_of(&config.display.verbosity),
        "theme" => config.display.theme.clone(),
        "no_color" => config.display.no_color.to_string(),
        "strip_position" => name_of(&config.display.strip_position),
        "strip_style" => name_of(&config.display.strip_style),
        "path_style" => name_of(&config.display.path_style),
        "diff_layout" => name_of(&config.display.diff_layout),
        "diff_palette" => name_of(&config.display.diff_palette),
        "mouse_capture" => config.display.mouse_capture.to_string(),
        "caret_display" => name_of(&config.display.caret_display),
        "copy_command" => opt_render(config.display.copy_command.as_ref()),
        "max_turns" => config.runner.max_turns.to_string(),
        "turn_timeout_secs" => config.runner.turn_timeout_secs.to_string(),
        "auto_compact" => config.runner.auto_compact.to_string(),
        "compact_threshold" => config.runner.compact_threshold.to_string(),
        "compactor" => name_of(&config.runner.compactor),
        "permission_mode" => name_of(&config.runner.permission_mode),
        "parallel_tool_dispatch" => config.runner.parallel_tool_dispatch.to_string(),
        "read_max_lines" => config.runner.read_max_lines.to_string(),
        "read_max_bytes" => config.runner.read_max_bytes.to_string(),
        "max_tool_output_chars" => config.runner.max_tool_output_chars.to_string(),
        "log_level" => config.telemetry.level.clone(),
        "json_logs" => config.telemetry.json_logs.to_string(),
        "redact_secrets" => config.security.redact_secrets.to_string(),
        _ => return None,
    };
    Some(rendered)
}

/// Set `key` to `value` on `config`, validating the value's shape.
///
/// The value is parsed exactly as the config file parses it; a value that will
/// not deserialize into the field's type comes back `Err` naming the field and
/// the reason, leaving `config` untouched. The bounds split by who else
/// enforces them: the loader-refused ones — the `turn_timeout_secs` floor,
/// the `read_max_bytes` / `max_tool_output_chars` relation, the
/// reasoning-effort ladder — are enforced identically here because a value
/// the next load would reject may never be written (persisting it would
/// brick the next start until a hand edit undid the write), while the ones
/// the loader would only quietly rewrite — an out-of-range
/// `compact_threshold` clamps, an unresolvable theme spelling falls back —
/// are refused here so a write cannot land a value the next start would
/// silently substitute away. The optional string keys refuse the literal
/// word `unset`: it is [`get`]'s display spelling for absence, and storing
/// it would both render identically to absence and shadow whatever the
/// absence stood for; `/unset` is the command that clears.
///
/// # Errors
///
/// Returns a message naming the field and why it was refused when `key` is not
/// a settable value or `value` does not parse into the field's type or past
/// its bound.
pub fn set(config: &mut DchConfig, key: &str, value: &str) -> Result<(), String> {
    if value == "unset" && matches!(key, "api_key" | "fallback_model" | "copy_command") {
        return Err(format!(
            "{key}: `unset` is the display word for an absent value, not a \
             storable one — clear it with /unset {key}"
        ));
    }
    match key {
        "model" => {
            if !value.trim().is_empty()
                && !config.models.is_empty()
                && !config
                    .models
                    .iter()
                    .any(|entry| entry.model.eq_ignore_ascii_case(value.trim()))
            {
                return Err(format!(
                    "model: with [[models]] declared the name must be one of them — {}",
                    config.model_names().join(", ")
                ));
            }
            config.api.model = value.to_string();
        }
        "base_url" => config.api.base_url = value.to_string(),
        "api_type" => config.api.api_type = parse::<ApiType>(key, value)?,
        "max_tokens" => config.api.max_tokens = parse::<u32>(key, value)?,
        "context_window" => config.api.context_window = Some(parse::<u64>(key, value)?),
        "request_timeout_secs" => config.api.request_timeout_secs = parse::<u64>(key, value)?,
        "fallback_model" => config.api.fallback_model = Some(value.to_string()),
        "reasoning_effort" => {
            if !dch_config::is_reasoning_effort(value) {
                return Err(format!(
                    "reasoning_effort must be one of {} (in rung order), not {value:?}",
                    dch_config::REASONING_EFFORT_WIRE_NAMES.join(", ")
                ));
            }
            config.api.reasoning_effort = Some(value.to_string());
        }
        "prompt_cache" => config.api.prompt_cache = parse::<bool>(key, value)?,
        "api_key" => config.api.api_key = Some(value.to_string()),
        "verbosity" => config.display.verbosity = parse::<Verbosity>(key, value)?,
        "theme" => {
            if crate::theme::Theme::by_name(value).is_none() {
                return Err(format!(
                    "theme: no theme named {value:?} — /themes lists every spelling"
                ));
            }
            config.display.theme = crate::theme::Theme::key_for_name(value)
                .unwrap_or(value)
                .to_string();
        }
        "no_color" => config.display.no_color = parse::<bool>(key, value)?,
        "strip_position" => config.display.strip_position = parse::<StripPosition>(key, value)?,
        "strip_style" => config.display.strip_style = parse::<StripStyle>(key, value)?,
        "path_style" => config.display.path_style = parse::<PathStyle>(key, value)?,
        "diff_layout" => config.display.diff_layout = parse::<DiffLayout>(key, value)?,
        "diff_palette" => {
            config.display.diff_palette = parse::<DiffPalette>(key, value)?;
        }
        "mouse_capture" => config.display.mouse_capture = parse::<bool>(key, value)?,
        "caret_display" => config.display.caret_display = parse::<CaretDisplay>(key, value)?,
        "copy_command" => config.display.copy_command = Some(value.to_string()),
        "max_turns" => config.runner.max_turns = parse::<usize>(key, value)?,
        "turn_timeout_secs" => {
            let timeout: u64 = parse::<u64>(key, value)?;
            if timeout < TURN_TIMEOUT_FLOOR_SECS {
                return Err(format!(
                    "turn_timeout_secs must be at least {TURN_TIMEOUT_FLOOR_SECS}, not {timeout}"
                ));
            }
            config.runner.turn_timeout_secs = timeout;
        }
        "auto_compact" => config.runner.auto_compact = parse::<bool>(key, value)?,
        "compact_threshold" => {
            let threshold: u8 = parse::<u8>(key, value)?;
            if threshold > 100 {
                return Err(format!(
                    "compact_threshold must be 0..=100, not {threshold}"
                ));
            }
            config.runner.compact_threshold = threshold;
        }
        "compactor" => config.runner.compactor = parse::<CompactorSelection>(key, value)?,
        "permission_mode" => config.runner.permission_mode = parse::<PermissionMode>(key, value)?,
        "parallel_tool_dispatch" => {
            config.runner.parallel_tool_dispatch = parse::<bool>(key, value)?;
        }
        "read_max_lines" => config.runner.read_max_lines = parse::<usize>(key, value)?,
        "read_max_bytes" => {
            let ceiling: usize = parse::<usize>(key, value)?;
            let cap = config.runner.max_tool_output_chars;
            if cap != 0 && ceiling > cap {
                return Err(format!(
                    "read_max_bytes must not exceed max_tool_output_chars = {cap}, not {ceiling}"
                ));
            }
            config.runner.read_max_bytes = ceiling;
        }
        "max_tool_output_chars" => {
            let cap: usize = parse::<usize>(key, value)?;
            let ceiling = config.runner.read_max_bytes;
            if cap != 0 && ceiling > cap {
                return Err(format!(
                    "max_tool_output_chars must be 0 (no cap) or at least read_max_bytes = \
                     {ceiling}, not {cap}"
                ));
            }
            config.runner.max_tool_output_chars = cap;
        }
        "log_level" => config.telemetry.level = value.to_string(),
        "json_logs" => config.telemetry.json_logs = parse::<bool>(key, value)?,
        "redact_secrets" => config.security.redact_secrets = parse::<bool>(key, value)?,
        _ => return Err(format!("no such setting: {key}")),
    }
    Ok(())
}

/// Whether `key` names a settable value.
///
/// Membership in [`KEYS`] — the one vocabulary `/set`, `/unset`,
/// `/config`, and `Tab` completion all share.
#[must_use]
pub fn is_key(key: &str) -> bool {
    KEYS.contains(&key)
}

/// Whether an optional key's value is absent, for the keys that can be.
///
/// The five optional leaves hold `None` when the file left them out; a
/// non-optional key always carries a value and reports `false`.
fn optional_is_none(config: &DchConfig, key: &str) -> Option<bool> {
    let absent = match key {
        "context_window" => config.api.context_window.is_none(),
        "fallback_model" => config.api.fallback_model.is_none(),
        "reasoning_effort" => config.api.reasoning_effort.is_none(),
        "api_key" => config.api.api_key.is_none(),
        "copy_command" => config.display.copy_command.is_none(),
        _ => return None,
    };
    Some(absent)
}

/// Whether `key` names an optional value the config carries nothing for.
///
/// The override ledger reads this when it records a pre-`/set` original:
/// an absent original must restore to the same absence — `None` in the
/// field, no value spelled like one — and only this predicate tells
/// absence apart from a present value that renders as `unset`-adjacent
/// text. Keys that always carry a value are never absent.
#[must_use]
pub fn is_absent(config: &DchConfig, key: &str) -> bool {
    optional_is_none(config, key).unwrap_or(false)
}

/// Restore an optional key to the absent state the file's omission spelled.
///
/// The `/unset` restore calls this when the ledger recorded that the value
/// was absent before the override: the field returns to `None`, so an
/// environment credential or a provider default takes over again rather
/// than an empty value shadowing it.
///
/// # Errors
///
/// `Err` names the key when it always carries a value and cannot be made
/// absent — those restore through [`set`] instead.
pub fn clear(config: &mut DchConfig, key: &str) -> Result<(), String> {
    match key {
        "context_window" => config.api.context_window = None,
        "fallback_model" => config.api.fallback_model = None,
        "reasoning_effort" => config.api.reasoning_effort = None,
        "api_key" => config.api.api_key = None,
        "copy_command" => config.display.copy_command = None,
        _ => {
            return Err(format!(
                "{key} always carries a value; a restore replaces it rather than clearing it"
            ));
        }
    }
    Ok(())
}

/// Parse a scalar or enum value the way the config file would.
///
/// Numbers and bools parse from their literal text; an enum parses from a
/// quoted string so serde applies the enum's own `rename_all` — the accepted
/// spellings are exactly the file's.
///
/// # Errors
///
/// Returns serde's message, prefixed with the key, when `value` does not
/// deserialize into `T`.
fn parse<T>(key: &str, value: &str) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let jsonish = match value {
        "true" | "false" => value.to_string(),
        other if looks_numeric(other) => other.to_string(),
        other => format!("\"{other}\""),
    };
    serde_json::from_str::<T>(&jsonish).map_err(|err| format!("{key}: {err}"))
}

/// Whether `text` looks like a JSON number (a leading digit or sign).
///
/// The parser's disambiguator: a numeric-looking value parses bare
/// while anything else is quoted, so an enum named `42` could still be
/// spelled — a number is read as a number because nothing else reads
/// that way.
fn looks_numeric(text: &str) -> bool {
    matches!(text.as_bytes().first(), Some(b'0'..=b'9' | b'-'))
}

/// Render an enum by the string its config field serializes to.
///
/// Round-trips through serde's own serialization so the accepted
/// spelling is whatever `rename_all` produced — the same word the file
/// carries, never a second spelling to keep in step.
fn name_of<T>(value: &T) -> String
where
    T: serde::Serialize,
{
    serde_json::to_value(value)
        .ok()
        .map(|value| match value.as_str() {
            Some(text) => text.to_string(),
            None => value.to_string(),
        })
        .unwrap_or_default()
}

/// Render an optional scalar: its value or `unset` when absent.
///
/// The word the file's omission spells — what `/config` shows for a
/// value the file never set, and the shape the display uses for keys
/// whose absence is itself a setting.
fn opt_render<T>(value: Option<T>) -> String
where
    T: serde::Serialize,
{
    match value {
        Some(value) => serde_json::to_value(value)
            .ok()
            .map(|value| match value.as_str() {
                Some(text) => text.to_string(),
                None => value.to_string(),
            })
            .unwrap_or_default(),
        None => "unset".to_string(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_a_live_class_and_a_renderer() {
        let config = DchConfig::default();
        for key in KEYS {
            assert!(
                live(key).is_some(),
                "{key} should be classed for how it takes effect"
            );
            assert!(is_key(key));
            assert!(
                get(&config, key).is_some(),
                "{key} should render a current value"
            );
        }
    }

    #[test]
    fn a_scalar_round_trips_through_set_and_get() {
        let mut config = DchConfig::default();
        set(&mut config, "verbosity", "verbose").expect("verbose parses as verbosity");
        assert_eq!(get(&config, "verbosity").as_deref(), Some("verbose"));
        set(&mut config, "max_turns", "64").expect("64 parses as max_turns");
        assert_eq!(config.runner.max_turns, 64);
        set(&mut config, "auto_compact", "false").expect("false parses");
        assert!(!config.runner.auto_compact);
        set(&mut config, "caret_display", "static").expect("the static spelling parses");
        assert_eq!(
            config.display.caret_display,
            dch_config::CaretDisplay::Static,
            "a static caret is settable"
        );
        assert_eq!(get(&config, "caret_display").as_deref(), Some("static"));
        assert_eq!(
            get(&DchConfig::default(), "api_key").as_deref(),
            Some("unset"),
            "an absent key renders as unset, like every other optional leaf"
        );
    }

    #[test]
    fn the_unset_display_word_is_refused_for_optional_strings() {
        // `unset` is /config's word for an absent optional, not a value:
        // storing it would persist to the file, shadow the environment
        // credential (or name a model that does not exist) on the next
        // start, and render identically to absence so /config could not
        // show the difference. The refusal points at the command that
        // actually clears.
        for key in ["api_key", "fallback_model", "copy_command"] {
            let mut config = DchConfig::default();
            let err = set(&mut config, key, "unset").unwrap_err();
            assert!(
                err.contains("/unset"),
                "{key}: the refusal points at the clearing command: {err}"
            );
            assert!(
                is_absent(&config, key),
                "{key}: the refused write leaves the config untouched"
            );
        }
    }

    #[test]
    fn a_theme_set_folds_the_spelling_to_the_registry_key() {
        // "Gruvbox Dark" resolves through the palette table; the stored
        // spelling must be the registry key it folds to, so the file and
        // /config carry one canonical form whatever casing the user
        // typed.
        let mut config = DchConfig::default();
        set(&mut config, "theme", "Gruvbox Dark").expect("the display spelling resolves");
        assert_eq!(
            config.display.theme, "gruvbox_dark",
            "the stored spelling is the registry key"
        );
        assert_eq!(
            get(&config, "theme").as_deref(),
            Some("gruvbox_dark"),
            "and /config shows the folded key"
        );
    }

    #[test]
    fn an_absent_optional_round_trips_through_clear() {
        let mut config = DchConfig::default();
        assert!(is_absent(&config, "reasoning_effort"), "absent by default");
        set(&mut config, "reasoning_effort", "high").expect("high parses");
        assert!(
            !is_absent(&config, "reasoning_effort"),
            "the override stands in"
        );
        clear(&mut config, "reasoning_effort").expect("an optional key clears");
        assert!(
            is_absent(&config, "reasoning_effort"),
            "clear restores the absence the file spelled by omission"
        );
        assert!(config.api.reasoning_effort.is_none());
    }

    #[test]
    fn clear_refuses_a_key_that_always_carries_a_value() {
        let mut config = DchConfig::default();
        let refused = clear(&mut config, "max_turns").expect_err("max_turns is never absent");
        assert!(refused.contains("max_turns"), "the refusal names the key");
    }

    #[test]
    fn a_bad_value_is_rejected_without_touching_the_config() {
        let mut config = DchConfig::default();
        let before = config.runner.max_turns;
        assert!(set(&mut config, "max_turns", "not-a-number").is_err());
        assert_eq!(
            config.runner.max_turns, before,
            "a rejected write changes nothing"
        );
    }

    #[test]
    fn an_enum_settles_on_the_files_own_spelling() {
        let mut config = DchConfig::default();
        // Each enum key round-trips through the spelling the file
        // itself carries — `get` renders it, `set` accepts it back,
        // and the value stands unchanged — while a display-cased
        // impostor is refused rather than folded in.
        let defaults = DchConfig::default();
        for key in [
            "api_type",
            "verbosity",
            "strip_position",
            "strip_style",
            "path_style",
            "diff_layout",
            "diff_palette",
            "compactor",
            "permission_mode",
        ] {
            let spelling = get(&defaults, key).expect("every enum key renders its default");
            set(&mut config, key, &spelling)
                .unwrap_or_else(|err| panic!("{key} accepts {spelling}: {err}"));
            assert_eq!(
                get(&config, key).as_deref(),
                Some(spelling.as_str()),
                "{key} settles on the file's own spelling"
            );
        }
        assert!(
            set(&mut config, "diff_layout", "Inline").is_err(),
            "a display-cased spelling is refused, not folded"
        );
        assert!(
            set(&mut config, "diff_palette", "Soft").is_err(),
            "a display-cased palette spelling is refused, not folded"
        );
    }

    #[test]
    fn display_side_keys_classify_live_and_runner_keys_restart() {
        // The honest split the footer and the /set notice speak from:
        // keys the renderer answers classify Display — including the
        // strip pair, the place row's spelling, and the copier the
        // live arm rebuilds — while terminal setup and everything the
        // loop owns stays Restart.
        for key in [
            "theme",
            "verbosity",
            "caret_display",
            "diff_layout",
            "diff_palette",
            "strip_position",
            "strip_style",
            "path_style",
            "copy_command",
        ] {
            assert_eq!(live(key), Some(Live::Display), "{key} renders live");
        }
        assert_eq!(
            live("mouse_capture"),
            Some(Live::Restart),
            "mouse capture is terminal setup, not a render read"
        );
        assert_eq!(live("max_turns"), Some(Live::Restart));
        assert_eq!(live("compactor"), Some(Live::Restart));
    }

    #[test]
    fn every_offered_value_candidate_is_accepted() {
        // The completion tables promise pasteable spellings; this pins
        // the promise against the one authority — the setter itself —
        // so a table entry can never drift from what the config
        // accepts.
        let mut config = DchConfig::default();
        for key in KEYS {
            let Some(candidates) = value_candidates(key) else {
                continue;
            };
            assert!(
                !candidates.is_empty(),
                "{key} offers a fixed vocabulary, so it offers something"
            );
            for candidate in candidates {
                set(&mut config, key, candidate)
                    .unwrap_or_else(|err| panic!("{key} accepts {candidate}: {err}"));
            }
        }
    }

    #[test]
    fn threshold_over_one_hundred_is_refused() {
        let mut config = DchConfig::default();
        assert!(set(&mut config, "compact_threshold", "150").is_err());
    }

    #[test]
    fn a_turn_timeout_below_the_loaders_floor_is_refused() {
        // The write-through may never land a value the next load refuses:
        // `/set turn_timeout_secs 60` used to persist, announce "applies
        // next session", and brick that session at startup. The gate
        // mirrors the loader's own floor and leaves the config untouched.
        let mut config = DchConfig::default();
        let refused = set(&mut config, "turn_timeout_secs", "60")
            .expect_err("the loader refuses this value; /set must refuse it here");
        assert!(
            refused.contains("turn_timeout_secs") && refused.contains("120"),
            "the refusal names the field and the floor: {refused}"
        );
        assert_eq!(
            config.runner.turn_timeout_secs, 300,
            "a refused write changes nothing"
        );
        set(&mut config, "turn_timeout_secs", "120")
            .expect("the floor itself parses — the bound is inclusive");
        assert_eq!(config.runner.turn_timeout_secs, 120);
    }

    #[test]
    fn a_read_ceiling_above_the_output_cap_is_refused_in_both_directions() {
        // One relation, two doors: raising the read ceiling past the
        // dispatch cap is refused, and so is lowering the cap under the
        // standing ceiling — either mutation leaves a config the loader
        // rejects, so neither may be written.
        let mut config = DchConfig::default();
        config.runner.max_tool_output_chars = 50_000;
        let refused = set(&mut config, "read_max_bytes", "60000")
            .expect_err("a whole-file view the cap would always cut is no setting at all");
        assert!(
            refused.contains("read_max_bytes") && refused.contains("max_tool_output_chars"),
            "the refusal names both sides of the relation: {refused}"
        );
        assert_eq!(
            config.runner.read_max_bytes,
            DchConfig::default().runner.read_max_bytes,
            "a refused write changes nothing"
        );

        let mut lowered = DchConfig::default();
        let refused = set(&mut lowered, "max_tool_output_chars", "1000")
            .expect_err("a cap under the standing ceiling strands every whole-file read");
        assert!(
            refused.contains("max_tool_output_chars") && refused.contains("read_max_bytes"),
            "the refusal names both sides of the relation: {refused}"
        );
        set(&mut lowered, "max_tool_output_chars", "0")
            .expect("zero is the explicit no-cap spelling and always stands");
        assert_eq!(lowered.runner.max_tool_output_chars, 0);
    }

    #[test]
    fn an_off_ladder_reasoning_effort_is_refused() {
        // The last bricking surface closed: a typo'd rung used to
        // persist and announce "applies next session", and the next
        // start died at client construction. The gate reads the same
        // ladder list the loader validates, so what /set may write,
        // the loader accepts.
        let mut config = DchConfig::default();
        let refused = set(&mut config, "reasoning_effort", "hihg")
            .expect_err("a typo is not a rung of the ladder");
        assert!(
            refused.contains("reasoning_effort")
                && refused.contains("hihg")
                && refused.contains("xhigh"),
            "the refusal names the field, the value, and the ladder: {refused}"
        );
        assert!(
            config.api.reasoning_effort.is_none(),
            "a refused write changes nothing"
        );
        set(&mut config, "reasoning_effort", "medium").expect("a rung parses");
        assert_eq!(config.api.reasoning_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn a_theme_the_palette_table_cannot_resolve_is_refused() {
        // `/set theme nonsense` used to persist, claim "applied now",
        // and leave the next launch to silently fall back — the exact
        // lie `/theme` refuses for the same word.
        let mut config = DchConfig::default();
        let refused = set(&mut config, "theme", "not-a-theme")
            .expect_err("a spelling no palette answers to is no setting at all");
        assert!(
            refused.contains("theme") && refused.contains("not-a-theme"),
            "the refusal names the field and the word: {refused}"
        );
        assert_eq!(
            config.display.theme,
            DchConfig::default().display.theme,
            "a refused write changes nothing"
        );
        set(&mut config, "theme", "nord").expect("a real spelling parses");
        assert_eq!(config.display.theme, "nord");
        assert_eq!(
            get(&config, "theme").as_deref(),
            Some("nord"),
            "the accepted spelling round-trips"
        );
    }

    #[test]
    fn an_unknown_key_is_not_settable() {
        let mut config = DchConfig::default();
        assert!(set(&mut config, "nope", "x").is_err());
        assert!(get(&config, "nope").is_none());
        assert!(!is_key("nope"));
    }

    #[test]
    fn the_secret_key_is_flagged() {
        assert!(is_secret("api_key"));
        assert!(!is_secret("model"));
    }
}
