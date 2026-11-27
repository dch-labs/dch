//! The `[[models]]` entries and the selection that resolves one
//! onto `[api]`.
//!
//! A [`ModelProfile`] is a partial [`ApiConfig`]: its stated fields
//! replace the base's and the rest are inherited, which is what
//! [`DchConfig::select_model`] folds onto the bundle a run reads.

use super::api::{ApiConfig, ApiType};
use crate::is_reasoning_effort;
use crate::{DchConfig, DchConfigError, REASONING_EFFORT_WIRE_NAMES};

/// One selectable model and the provider settings that reach it.
///
/// A `[[models]]` entry: a partial [`ApiConfig`] whose stated fields replace
/// `[api]`'s when the entry is chosen, so one config can hold several models
/// — each with its own endpoint, protocol, credential, and limits — without
/// repeating the settings they share. The first entry is the default run;
/// `[api]` supplies the settings an entry leaves unstated, and `dch --model
/// NAME` picks an entry by its [`model`](Self::model), ignoring case.
///
/// Every field but [`model`](Self::model) is optional for the reason
/// [`ApiConfig`] carries struct-level `#[serde(default)]`: an omitted field
/// has to mean "inherit `[api]`", not "replace it with this type's default".
/// An optional setting the entry *does* name is set; one it names as an empty
/// string is dropped outright — the only way an entry clears a base
/// `reasoning_effort` or `fallback_model` its model cannot carry.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ModelProfile {
    /// The model this entry names, as its provider knows it.
    ///
    /// Doubles as the selector `dch --model` matches, ignoring case; the run
    /// sends this entry's spelling rather than the spelling typed. Empty is
    /// rejected at load — an unnamed entry could never be chosen.
    pub model: String,

    /// Endpoint replacing `[api] base_url` for this model.
    ///
    /// Usually the reason an entry exists: a second provider lives on another
    /// host. Unset inherits, which suits a second model on the same server.
    pub base_url: Option<String>,

    /// Provider protocol replacing `[api] api_type` for this model.
    ///
    /// Paired with [`base_url`](Self::base_url): a cross-provider entry names
    /// both, a same-provider entry names neither.
    pub api_type: Option<ApiType>,

    /// Credential replacing `[api] api_key` for this model.
    ///
    /// Providers still read their own environment variables when this is
    /// unset, so a second provider keyed from the environment needs no entry
    /// of its own on that count. An empty string drops the inherited key.
    pub api_key: Option<String>,

    /// Output cap per response replacing `[api] max_tokens`.
    ///
    /// Bound to the entry's own model, whose cheaper tiers often carry
    /// a smaller completion budget than the shared base assumes; unset
    /// inherits `[api]`'s value, and the provider honors it exactly as
    /// it honors the base's.
    pub max_tokens: Option<u32>,

    /// Context window replacing `[api] context_window` for this model.
    ///
    /// Models in one config routinely differ here, so an entry usually states
    /// its own. An entry cannot ask for provider discovery where `[api]`
    /// already states a number: leave `[api]` without a window and name each
    /// entry's own to keep discovery for the models that want it.
    pub context_window: Option<u64>,

    /// Per-request timeout replacing `[api] request_timeout_secs`.
    ///
    /// A model that reasons at length routinely outlives a budget tuned
    /// for quick chats, so an entry states its own; unset inherits
    /// `[api]`'s value unchanged.
    pub request_timeout_secs: Option<u64>,

    /// Fallback model replacing `[api] fallback_model`.
    ///
    /// Names a model on *this* entry's provider — a fallback shares the
    /// selected bundle's endpoint and credential. An empty string selects no
    /// fallback at all.
    pub fallback_model: Option<String>,

    /// Azure resource replacing `[api] azure_resource`.
    ///
    /// For an entry whose model is served by a different Azure
    /// deployment than the base's. Unset inherits `[api]`'s value, and
    /// the name is consulted only when the entry's own `api_type` is
    /// `azure`.
    pub azure_resource: Option<String>,

    /// Thinking budget replacing `[api] reasoning_effort`.
    ///
    /// The field most in need of clearing: a base budget inherited by a model
    /// that cannot carry it is a request the provider rejects, so an entry
    /// whose model has no dial names this empty.
    pub reasoning_effort: Option<String>,

    /// Prompt-cache breakpoints replacing `[api] prompt_cache`.
    ///
    /// An Anthropic-protocol entry on an endpoint that rejects the
    /// breakpoints' extra field names this `false` without touching the
    /// shared base setting; unset inherits `[api]`'s value.
    pub prompt_cache: Option<bool>,
}

impl ModelProfile {
    /// The bundle this entry selects: `base` with each field the entry states
    /// taking the place of `[api]`'s.
    ///
    /// Pure over the two values, so the caller decides whether the result
    /// replaces the config's bundle. Fields the entry leaves unset keep
    /// `base`'s value; [`cleared`] is how an entry drops one it names empty.
    /// The model itself is handed over trimmed — the spelling the entry
    /// knows, without the space beside it — so a name assembled in code
    /// with padding still reaches the provider as the model alone.
    fn apply_onto(&self, base: &ApiConfig) -> ApiConfig {
        ApiConfig {
            model: self.model.trim().to_string(),
            base_url: self
                .base_url
                .clone()
                .unwrap_or_else(|| base.base_url.clone()),
            api_type: self.api_type.unwrap_or(base.api_type),
            api_key: cleared(self.api_key.as_ref(), base.api_key.as_deref()),
            max_tokens: self.max_tokens.unwrap_or(base.max_tokens),
            context_window: self.context_window.or(base.context_window),
            request_timeout_secs: self
                .request_timeout_secs
                .unwrap_or(base.request_timeout_secs),
            fallback_model: cleared(self.fallback_model.as_ref(), base.fallback_model.as_deref()),
            azure_resource: cleared(self.azure_resource.as_ref(), base.azure_resource.as_deref()),
            reasoning_effort: cleared(
                self.reasoning_effort.as_ref(),
                base.reasoning_effort.as_deref(),
            ),
            prompt_cache: self.prompt_cache.unwrap_or(base.prompt_cache),
        }
    }
}

/// The effective value of an optional setting an entry may replace or drop.
///
/// Unset inherits `base`; a stated non-empty value replaces it; a stated empty
/// string states "none". Whitespace-only counts as empty — a value typed in
/// order to clear is clearing, whatever the editor left around it.
fn cleared(declared: Option<&String>, base: Option<&str>) -> Option<String> {
    match declared {
        None => base.map(str::to_string),
        Some(text) if text.trim().is_empty() => None,
        Some(text) => Some(text.clone()),
    }
}

impl DchConfig {
    /// Fold model-name whitespace out of the names as stored.
    ///
    /// A model name is an identity, and an identity a stray space can
    /// hide is one every later comparison can misread: the selection
    /// match, the base-model fallback, the duplicate check, and the
    /// list a refusal prints all speak the trimmed spelling — a base
    /// carrying editor padding would be listed under one spelling and
    /// refused under another. So the padding is folded away once here,
    /// at the single point a file becomes a config, and the name that
    /// later rides a provider request is the model, never the space
    /// beside it. The comparisons themselves trim too (see
    /// [`select_model`](Self::select_model) and
    /// [`validate_models`](Self::validate_models)), so a config
    /// assembled in code rather than parsed behaves like one loaded.
    pub(super) fn trim_model_names(&mut self) {
        self.api.model = self.api.model.trim().to_string();
        for entry in &mut self.models {
            entry.model = entry.model.trim().to_string();
        }
    }

    /// The model names `--model` can choose among, in file order.
    ///
    /// The entries alone: with `[[models]]` declared the list is a
    /// closed set — only entry names are selectable, so only entry
    /// names are listed, top first in the file's order. It is the set
    /// [`select_model`](Self::select_model) checks a requested name
    /// against and what its error lists. Each name is spoken trimmed —
    /// the entry's own spelling minus whatever padding an entry
    /// assembled in code may carry. Empty for every config
    /// without entries: that is the open case, where a requested name
    /// passes through unvalidated instead of being refused for
    /// matching nothing.
    #[must_use]
    pub fn model_names(&self) -> Vec<&str> {
        self.models.iter().map(|entry| entry.model.trim()).collect()
    }

    /// Point the run at one of the configured `[[models]]` entries.
    ///
    /// `Some(name)` selects the entry whose [`model`](ModelProfile::model)
    /// matches it, ignoring case, and that entry's stated fields replace
    /// `[api]`'s. That is the whole reason entries exist: a different model
    /// usually means a different endpoint, protocol, and credential, which a
    /// bare name could never reach. The resolved bundle lands in
    /// [`ApiConfig`] — the field every consumer already reads — so selecting
    /// changes nothing downstream of here.
    ///
    /// `None` selects what the config itself names: a saved `[api] model`
    /// that matches a declared entry selects that entry, and only a saved
    /// name that is empty or matches nothing falls back to the **first**
    /// entry — the list is ordered and the top of it is the default run.
    /// The saved name is what a landed `/model` switch persisted, so a
    /// fresh start opening on the switched model is the whole point of
    /// that write; the fallback keeps the top-of-the-list default
    /// readable for configs whose `[api] model` names nothing declared.
    /// `[api]` stays the selection only while the config declares no
    /// entries, so every config written before entries existed keeps
    /// behaving as it always did, and `[api]` keeps supplying the settings
    /// an entry leaves unstated. An entry named the same as `[api] model`
    /// is not a conflict: it *is* that model's own config, and being at
    /// the top of the list makes it the default.
    ///
    /// The declared list is a closed set: with entries standing, a name
    /// is selectable exactly when an entry declares it — `[api]`'s own
    /// model name included. Naming the base's model as an entry is what
    /// makes it choosable, so a persisted switch always resolves on the
    /// next start: its entry if it still stands, the head of the list if
    /// a hand edit removed it.
    ///
    /// With no entries declared, `Some(name)` renames
    /// [`ApiConfig::model`] exactly as the flag always has — a name of
    /// only whitespace is refused outright either way — and `None`
    /// changes nothing, so a config without entries behaves identically
    /// to every config written before entries existed.
    ///
    /// One selection per run is the contract: a second call layers its entry
    /// over the first's result rather than over `[api]`, so a runner that
    /// switches models after startup re-reads the config first.
    ///
    /// # Errors
    ///
    /// Returns [`DchConfigError::UnknownModel`] when entries are declared
    /// and `requested` matches none of them: a typo caught at startup
    /// naming the names that exist, rather than a request spent against
    /// the wrong endpoint. A name of only whitespace errors the same
    /// way — a blank selection must never silently fall back to the
    /// default entry, whose profile the user may believe served the run.
    pub fn select_model(&mut self, requested: Option<&str>) -> Result<(), DchConfigError> {
        let Some(wanted) = requested else {
            let saved = self.api.model.trim();
            let saved_entry = if saved.is_empty() {
                None
            } else {
                self.models
                    .iter()
                    .find(|entry| entry.model.trim().eq_ignore_ascii_case(saved))
            };
            if let Some(entry) = saved_entry.or_else(|| self.models.first()) {
                let entry = entry.clone();
                self.api = entry.apply_onto(&self.api);
            }
            return Ok(());
        };
        if wanted.trim().is_empty() {
            return Err(DchConfigError::UnknownModel {
                requested: wanted.to_string(),
                available: self.model_names().join(", "),
            });
        }
        let wanted = wanted.trim();
        if self.models.is_empty() {
            self.api.model = wanted.to_string();
            return Ok(());
        }
        let selected = self
            .models
            .iter()
            .find(|entry| entry.model.trim().eq_ignore_ascii_case(wanted))
            .cloned();
        match selected {
            Some(entry) => {
                self.api = entry.apply_onto(&self.api);
                Ok(())
            }
            None => Err(DchConfigError::UnknownModel {
                requested: wanted.to_string(),
                available: self.model_names().join(", "),
            }),
        }
    }

    /// Reject `[[models]]` entries that could not be selected unambiguously.
    ///
    /// An entry naming no model can never match a request, and two entries
    /// sharing a name make `--model NAME` a coin flip between two providers.
    /// Both are startup errors naming the entry, the way the rest of
    /// validation turns a silently-rewritten value into a message. Names are
    /// compared case-insensitively because that is how [`select_model`](Self::select_model)
    /// matches them.
    ///
    /// Two entries sharing `[api]`'s model name is not a conflict — that entry
    /// is the model's own config, and it wins both as the default (top of the
    /// list) and under `--model`, since entries are searched first.
    ///
    /// # Errors
    ///
    /// Returns [`DchConfigError::Invalid`] for an entry naming no model, for
    /// two entries sharing one name, or for an entry whose stated
    /// `reasoning_effort` is neither a rung of the ladder nor blank — the
    /// blank spelling is the documented drop-dial override and passes, the
    /// same as any other optional an entry names empty.
    pub(super) fn validate_models(&self) -> Result<(), DchConfigError> {
        let mut seen: Vec<String> = Vec::new();
        for entry in &self.models {
            if entry.model.trim().is_empty() {
                return Err(DchConfigError::Invalid(
                    "a [[models]] entry names no model: add model = \"...\" so --model can \
                     select it"
                        .to_string(),
                ));
            }
            let named = entry.model.trim();
            if seen
                .iter()
                .any(|seen: &String| seen.eq_ignore_ascii_case(named))
            {
                return Err(DchConfigError::Invalid(format!(
                    "duplicate [[models]] entry for model {named:?}: --model could not choose \
                     between them"
                )));
            }
            if let Some(effort) = &entry.reasoning_effort
                && !effort.trim().is_empty()
                && !is_reasoning_effort(effort)
            {
                return Err(DchConfigError::Invalid(format!(
                    "[[models]] {named:?} carries reasoning_effort = {effort:?}, not a rung \
                     of the ladder: selecting that entry would hand the client a level \
                     its construction refuses — expected one of {}",
                    REASONING_EFFORT_WIRE_NAMES.join(", ")
                )));
            }
            seen.push(named.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DchConfig;

    /// One local server plus one hosted provider, the shape `[[models]]`
    /// exists to describe: `[api]` carries the shared settings and the default
    /// model, and each entry states only where it differs.
    fn config_with_two_models() -> DchConfig {
        DchConfig::from_toml_str(
            r#"
[api]
model = "qwen-local"
base_url = "http://localhost:8080/v1"
api_type = "ollama"
max_tokens = 16_384
request_timeout_secs = 600
reasoning_effort = "high"

[[models]]
model = "glm-5.3"
base_url = "https://api.z.ai/api/anthropic"
api_type = "anthropic"
api_key = "zk-one"
context_window = 999_999

[[models]]
model = "qwen-fast"
context_window = 210_000
"#,
        )
        .expect("the two-entry fixture parses")
    }

    #[test]
    fn a_config_without_entries_keeps_the_api_section_alone() {
        let mut config =
            DchConfig::from_toml_str("[api]\nmodel = \"m\"\nbase_url = \"http://h/v1\"\n")
                .expect("a pre-entries config parses");
        assert!(
            config.models.is_empty(),
            "a config that never heard of [[models]] has none to choose among"
        );

        config.select_model(None).expect("nothing to select");
        assert_eq!(config.api.model, "m", "[api] stays in force");

        config
            .select_model(Some("brand-new"))
            .expect("no closed set");
        assert_eq!(config.api.model, "brand-new");
        assert_eq!(
            config.api.base_url, "http://h/v1",
            "with no entries declared the flag renames and leaves the endpoint alone, exactly \
             as it did before entries existed"
        );
    }

    #[test]
    fn a_selected_entry_brings_its_own_endpoint_protocol_and_key() {
        let mut config = config_with_two_models();
        config
            .select_model(Some("glm-5.3"))
            .expect("a declared name");
        assert_eq!(config.api.model, "glm-5.3");
        assert_eq!(config.api.base_url, "https://api.z.ai/api/anthropic");
        assert_eq!(config.api.api_type, ApiType::Anthropic);
        assert_eq!(config.api.api_key.as_deref(), Some("zk-one"));
        assert_eq!(config.api.context_window, Some(999_999));
        assert_eq!(
            config.api.max_tokens, 16_384,
            "a field the entry never names inherits [api]"
        );
        assert_eq!(config.api.request_timeout_secs, 600);
        assert_eq!(
            config.api.reasoning_effort.as_deref(),
            Some("high"),
            "a shared thinking budget reaches an entry that says nothing about it"
        );
    }

    #[test]
    fn selection_ignores_case_and_sends_the_configured_spelling() {
        let mut config = config_with_two_models();
        config
            .select_model(Some("GLM-5.3"))
            .expect("a case-insensitive match");
        assert_eq!(
            config.api.model, "glm-5.3",
            "the match is forgiving, but what the provider receives is the spelling it knows"
        );
    }

    #[test]
    fn a_padded_entry_assembled_in_code_selects_and_sends_its_trimmed_name() {
        // trim_model_names folds the padding away at load, so a file-fed
        // config never carries it — a config assembled in code can. The
        // explicit match then reads the name trimmed, as the default
        // selection and the duplicate check already do, and the name
        // that rides the provider request is the entry's own spelling
        // minus the space beside it.
        let mut config = config_with_two_models();
        config.models[1].model = "  Qwen-Fast  ".to_string();
        config
            .select_model(Some("qwen-fast"))
            .expect("the trimmed name selects the padded entry, ignoring case");
        assert_eq!(
            config.api.model, "Qwen-Fast",
            "the entry's spelling goes to the provider — never the padding"
        );
        assert_eq!(
            config.api.context_window,
            Some(210_000),
            "and it is that entry's bundle the run reads"
        );
    }

    #[test]
    fn a_refusal_offers_a_padded_entry_by_its_trimmed_name() {
        // The list a refusal prints is the list a request is matched
        // against, and both speak the trimmed name: an entry assembled
        // in code with padding is offered under the one spelling a
        // user can actually type, not the spelling stored.
        let mut config = config_with_two_models();
        config.models[1].model = "  Qwen-Fast  ".to_string();
        assert_eq!(
            config.model_names(),
            vec!["glm-5.3", "Qwen-Fast"],
            "model_names speaks the entry's spelling minus the padding"
        );
        let message = config
            .select_model(Some("cloude-haiku"))
            .expect_err("an unknown name is refused")
            .to_string();
        assert!(
            message.contains("Qwen-Fast") && !message.contains("  Qwen-Fast"),
            "the refusal lists the trimmed spelling, not the padding: {message}"
        );
    }

    #[test]
    fn a_second_selection_layers_over_the_firsts_result() {
        // One selection per run is the contract, but the call itself is
        // pure config math: an entry replaces the fields it names and
        // inherits the rest from the bundle as it stands. A second
        // selection therefore stacks on the first's result — a runner
        // that switches models after startup re-reads the config
        // first — with every field the second entry names standing on
        // its own value.
        let mut config = config_with_two_models();
        config
            .select_model(Some("glm-5.3"))
            .expect("the first declared name");
        config
            .select_model(Some("qwen-fast"))
            .expect("the second declared name");
        assert_eq!(config.api.model, "qwen-fast");
        assert_eq!(
            config.api.context_window,
            Some(210_000),
            "a field the second entry names stands on its own value"
        );
        assert_eq!(
            config.api.base_url, "https://api.z.ai/api/anthropic",
            "a field the second entry leaves out keeps the first selection's value, not [api]'s"
        );
        assert_eq!(
            config.api.api_type,
            ApiType::Anthropic,
            "the protocol rides with the endpoint it arrived with"
        );
        assert_eq!(
            config.api.api_key.as_deref(),
            Some("zk-one"),
            "the first selection's credential survives a second that names none"
        );
    }

    #[test]
    fn an_entry_drops_a_base_setting_its_model_cannot_carry() {
        let mut config = config_with_two_models();
        config.models[1].reasoning_effort = Some(String::new());
        config
            .select_model(Some("qwen-fast"))
            .expect("the second entry");
        assert_eq!(
            config.api.reasoning_effort, None,
            "an entry naming an optional setting empty clears the inherited one, rather than \
             sending a dial the selected model cannot carry"
        );
    }

    #[test]
    fn from_toml_str_accepts_an_entry_that_drops_the_reasoning_dial() {
        // The documented contract ("" drops the dial) must survive the
        // loader: validate_models treats a blank override as the drop
        // spelling, not an unknown rung, so the file loads and selecting
        // the entry clears what [api] would hand over. Whitespace-only is
        // the same drop, the way every other cleared() optional reads it.
        let config = DchConfig::from_toml_str(
            "[api]\nmodel = \"qwen\"\nreasoning_effort = \"medium\"\n\n\
             [[models]]\nmodel = \"m\"\nreasoning_effort = \"\"\n\n\
             [[models]]\nmodel = \"m-spaces\"\nreasoning_effort = \"   \"\n",
        )
        .expect("a blank override is the drop spelling, not an unknown rung");
        for name in ["m", "m-spaces"] {
            let mut selected = config.clone();
            selected
                .select_model(Some(name))
                .expect("the entry selects");
            assert_eq!(
                selected.api.reasoning_effort, None,
                "selecting {name:?} clears the inherited dial"
            );
        }
    }

    #[test]
    fn from_toml_str_still_refuses_a_non_blank_unknown_reasoning_rung() {
        let err = DchConfig::from_toml_str(
            "[api]\nmodel = \"qwen\"\n\n[[models]]\nmodel = \"m\"\nreasoning_effort = \"sideways\"\n",
        )
        .expect_err("an unknown non-blank rung stays a startup error");
        assert!(
            err.to_string().contains("sideways"),
            "the refusal names the entry's own value: {err}"
        );
    }

    #[test]
    fn an_unknown_name_is_refused_naming_the_models_that_exist() {
        let mut config = config_with_two_models();
        let before = config.api.clone();
        let message = config
            .select_model(Some("cloude-haiku"))
            .expect_err("a declared set closes the name")
            .to_string();
        for named in ["glm-5.3", "qwen-fast"] {
            assert!(
                message.contains(named),
                "the error lists {named}: {message}"
            );
        }
        assert!(
            !message.contains("qwen-local"),
            "a base name no entry claims is not listed as available: {message}"
        );
        assert_eq!(
            config.api, before,
            "a refused selection leaves the configured bundle exactly as it was"
        );
    }

    #[test]
    fn a_blank_model_name_is_refused_like_an_unknown_one() {
        // A name of only whitespace must not fall back to the head
        // entry: the user asked for a model, and silently running the
        // default would leave them believing a named profile served
        // the run.
        let mut config = config_with_two_models();
        let before = config.api.clone();
        let message = config
            .select_model(Some("   "))
            .expect_err("a blank name selects nothing")
            .to_string();
        assert!(
            message.contains("glm-5.3"),
            "the error names the models that exist, like any unknown: {message}"
        );
        assert_eq!(
            config.api, before,
            "the refused blank leaves the configured bundle untouched"
        );
    }

    #[test]
    fn the_base_name_selects_only_as_an_entry() {
        // The closed-set rule cuts both ways: the base's own name is
        // selectable exactly when an entry declares it — here no entry
        // does, so the name is refused naming the entries that exist.
        // Naming the base's model as an entry is what makes it choosable.
        let mut config = config_with_two_models();
        let err = config
            .select_model(Some("qwen-local"))
            .expect_err("a base name no entry claims is not selectable");
        let message = err.to_string();
        assert!(
            message.contains("glm-5.3") && message.contains("qwen-fast"),
            "the refusal names the selectable models: {message}"
        );
    }

    #[test]
    fn the_top_entry_is_the_default_run() {
        let mut config = config_with_two_models();
        config.select_model(None).expect("the top entry defaults");
        assert_eq!(
            config.api.model, "glm-5.3",
            "the list is ordered and the top of it is what a bare `dch` runs"
        );
        assert_eq!(
            config.api.base_url, "https://api.z.ai/api/anthropic",
            "the default run is that entry's provider, not [api]'s"
        );
        assert_eq!(
            config.api.request_timeout_secs, 600,
            "and it still inherits what [api] supplies for everyone"
        );
    }

    #[test]
    fn a_saved_selection_names_the_next_default_run() {
        // The read side of `/model`'s persistence: a switch that landed
        // wrote `[api] model`, and the next boot's selection honors that
        // name — its own entry lays over the base, not the list's head.
        // Without this, the first `[[models]]` entry would silently win
        // every restart and the saved switch would revert.
        let mut config = config_with_two_models();
        config.api.model = "qwen-fast".to_string();
        config.select_model(None).expect("the saved name selects");
        assert_eq!(
            config.api.model, "qwen-fast",
            "the saved selection stands — the head entry does not overwrite it"
        );
        assert_eq!(
            config.api.context_window,
            Some(210_000),
            "the saved entry's own window is what the run measures"
        );
        assert_eq!(
            config.api.base_url, "http://localhost:8080/v1",
            "an entry stating no endpoint inherits [api]'s, as a restart would"
        );
        assert_eq!(
            config.api.api_key, None,
            "no other entry's credential rides in — the base carries none"
        );
    }

    #[test]
    fn an_unknown_saved_name_falls_back_to_the_top_entry() {
        // A saved name nothing declares — a hand-edited file, a profile
        // deleted since the switch — cannot strand the run: the top of
        // the list is the default, and the default takes over.
        let mut config = config_with_two_models();
        config.api.model = "deleted-since-the-switch".to_string();
        config.select_model(None).expect("the fallback selects");
        assert_eq!(
            config.api.model, "glm-5.3",
            "a saved name nothing declares loses to the list's head"
        );
    }

    #[test]
    fn model_names_lists_the_entries_alone() {
        // A declared list is a closed set: only entry names select, so
        // only entry names are listed — `[api]`'s own model name is
        // not a choice once entries exist, and listing it would print
        // a name in the refusal that the refusal itself rejects.
        let config = config_with_two_models();
        assert_eq!(
            config.model_names(),
            vec!["glm-5.3", "qwen-fast"],
            "order is the file's order, so the default leads rather than the name that sorts \
             first — a map of tables would have alphabetized this"
        );
    }

    #[test]
    fn model_names_is_empty_without_entries() {
        // The open case: with no `[[models]]` declared there is no
        // closed set to list, and a requested name passes through
        // unvalidated rather than being refused for matching nothing.
        let config = DchConfig::from_toml_str("[api]\nmodel = \"  glm-5.3  \"\n")
            .expect("a padded base parses");
        assert!(
            config.model_names().is_empty(),
            "no entries declared is no closed set — nothing is listed or refused"
        );
    }

    #[test]
    fn a_base_name_no_entry_claims_is_refused() {
        // Listing and selecting are one contract: with entries
        // declared, only entry names select — `[api]`'s own padded
        // name is neither listed nor accepted, and naming the base's
        // model as an entry is what makes it choosable.
        let mut config = DchConfig::from_toml_str(
            "[api]\nmodel = \"  glm-5.3  \"\n\
             [[models]]\nmodel = \"other\"\n",
        )
        .expect("a padded base parses");
        let err = config
            .select_model(Some("glm-5.3"))
            .expect_err("a name no entry declares is refused, base or not");
        assert!(
            err.to_string().contains("other"),
            "the refusal names the entries that exist: {err}"
        );
    }

    #[test]
    fn two_entries_sharing_a_name_under_padding_fail_the_load() {
        // "glm" and "glm " name one model to `--model`, and the
        // duplicate check folds names the way selection does — an
        // editor's stray space must not mint a second identity no
        // selection could ever tell from the first.
        let err = DchConfig::from_toml_str(
            "[api]\nmodel = \"base\"\n\
             [[models]]\nmodel = \"glm\"\n[[models]]\nmodel = \"glm \"\n",
        )
        .expect_err("padding folds away, so these are the same name");
        assert!(
            err.to_string().to_lowercase().contains("glm"),
            "the error names the doubled model: {err}"
        );
    }

    #[test]
    fn two_entries_sharing_a_name_under_padding_fail_a_save_of_a_config_assembled_in_code() {
        // The one production route where names reach `validate` without a
        // parse in front of it: a config assembled in code and saved. The
        // duplicate check trims on its own, and names the name it found.
        let config = DchConfig {
            models: vec![
                ModelProfile {
                    model: "glm".to_string(),
                    ..ModelProfile::default()
                },
                ModelProfile {
                    model: "glm ".to_string(),
                    ..ModelProfile::default()
                },
            ],
            ..DchConfig::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let err = config
            .save_to_dir(dir.path())
            .expect_err("the padded duplicate is refused at the save");
        assert!(
            err.to_string()
                .contains("duplicate [[models]] entry for model \"glm\":"),
            "the refusal names the trimmed name: {err}"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "a refused save touches no target"
        );
    }

    #[test]
    fn two_entries_sharing_a_name_fail_the_load() {
        let err = DchConfig::from_toml_str(
            "[api]\nmodel = \"base\"\n\
             [[models]]\nmodel = \"twin\"\n[[models]]\nmodel = \"TWIN\"\n",
        )
        .expect_err("case folds together, so these are the same name");
        assert!(
            err.to_string().to_lowercase().contains("twin"),
            "the error names the entry, in whichever spelling came second: {err}"
        );
    }

    #[test]
    fn an_entry_naming_no_model_fails_the_load() {
        let err = DchConfig::from_toml_str("[[models]]\ncontext_window = 4096\n")
            .expect_err("an unnamed entry could never be selected");
        assert!(
            err.to_string().contains("no model"),
            "the error says what to add: {err}"
        );
    }

    #[test]
    fn an_entry_carrying_the_bases_own_model_name_wins() {
        // The shape a hand-written config actually takes: [api] names a model
        // that also has a full entry of its own. Rejecting that would refuse a
        // file whose entries are complete per-model configs.
        let mut config = DchConfig::from_toml_str(
            r#"
[api]
model = "shared"
request_timeout_secs = 600

[[models]]
model = "shared"
base_url = "http://localhost:8080/v1"
api_type = "ollama"
"#,
        )
        .expect("an entry may carry [api]'s model name");
        config
            .select_model(None)
            .expect("as the top entry it is the default");
        assert_eq!(config.api.base_url, "http://localhost:8080/v1");
        assert_eq!(
            config.api.request_timeout_secs, 600,
            "the entry overrides the endpoint and still inherits the shared timeout"
        );
        assert_eq!(
            config.model_names(),
            vec!["shared"],
            "one model, one name — listed once even though two blocks spell it"
        );
    }

    #[test]
    fn entries_round_trip_through_serialization() {
        let config = config_with_two_models();
        let text = toml::to_string(&config).expect("the config serializes");
        let back = DchConfig::from_toml_str(&text).expect("it reads back");
        assert_eq!(back.model_names(), config.model_names());
        assert_eq!(
            back.models, config.models,
            "an entry survives the round trip"
        );
    }
}
