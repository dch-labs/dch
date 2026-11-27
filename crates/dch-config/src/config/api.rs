//! The `[api]` section: the provider connection and the wire
//! vocabulary it carries.
//!
//! [`ApiConfig`] holds the endpoint, credential, and limits one model
//! runs against, [`ApiType`] names the protocol family and its stock
//! hosts, and the reasoning-effort ladder is the one word list the
//! loader and the write-through gate both read.

/// The window assumed when neither the config nor the provider names one.
///
/// The stand-in every discovery failure lands on: large enough for a
/// frontier-model session, small enough to keep a compaction threshold
/// meaningful, and identical everywhere the fallback is needed so the
/// config mapping and the runner agree on it.
pub const FALLBACK_CONTEXT_WINDOW: u64 = 200_000;
/// Which LLM provider to target.
///
/// Selects the wire protocol and default [`ApiConfig::base_url`] used when
/// talking to a model. The variant also gates provider-specific request shaping
/// elsewhere in the application. Serde (de)serializes values as their lowercase
/// name (e.g. `ollama`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ApiType {
    /// `OpenAI`-compatible API.
    ///
    /// The Chat Completions schema that several other providers (including
    /// vLLM servers) speak, so they map to this variant's client family;
    /// `DeepSeek` and `Grok` carry their own variants selecting their
    /// provider profiles while reusing the same wire protocol.
    OpenAi,

    /// `Anthropic` Messages API (also used by Z.AI).
    ///
    /// Targets the Messages-style request/response shape rather than the
    /// OpenAI-compatible one.
    Anthropic,

    /// Google `Gemini`.
    ///
    /// Targets Google's Generative Language endpoint, which has its own request
    /// schema distinct from the OpenAI-compatible family.
    Gemini,

    /// Local `Ollama` server — no API key needed.
    ///
    /// The default variant, reflecting that a freshly configured `dch` assumes
    /// a locally running server reachable without credentials.
    #[default]
    Ollama,

    /// A self-hosted server speaking the OpenAI-compatible wire.
    ///
    /// The bring-your-own-server arm — llama.cpp, vLLM, LM Studio,
    /// and local inference servers generally: the chat-completions
    /// schema with no credential requirement (a placeholder key rides
    /// the Authorization header; servers that ignore it are the
    /// norm). Usage reporting stays on, so the session's token
    /// accounting and calibration anchor to the server's own counts.
    #[serde(rename = "self_hosted")]
    SelfHosted,

    /// `DeepSeek` API.
    ///
    /// A standalone variant selecting its provider profile, whose endpoint,
    /// credential variable, and default model the profile supplies.
    DeepSeek,

    /// xAI `Grok`.
    ///
    /// A standalone variant selecting its provider profile, whose endpoint,
    /// credential variable, and default model the profile supplies.
    Grok,

    /// `Z.AI` API.
    ///
    /// A standalone variant selecting its provider profile, whose endpoint,
    /// credential variable, and default model the profile supplies.
    Zai,

    /// Azure OpenAI.
    ///
    /// The deployment-style endpoint under `https://{resource}.openai.azure.com`.
    /// The resource name comes from [`ApiConfig::azure_resource`] or the
    /// `AZURE_OPENAI_RESOURCE` environment variable; credentials follow the
    /// provider profile (`AZURE_OPENAI_API_KEY`).
    Azure,

    /// Moonshot AI.
    ///
    /// An OpenAI-compatible API served by Moonshot; its endpoint,
    /// credentials, and default model come from its provider profile
    /// (`MOONSHOT_API_KEY`).
    Moonshot,

    /// AWS Bedrock.
    ///
    /// SigV4-authenticated native endpoint; credentials come from the standard
    /// `AWS_*` environment variables, and `api_key`/`base_url` do not apply.
    Bedrock,
}

impl ApiType {
    /// The stock default `base_url` for this provider, when it has one.
    ///
    /// Consulted as the effective endpoint when [`ApiConfig::base_url`] is
    /// unset, for the stock providers (`OpenAi`, `Anthropic`, `Gemini`).
    /// The self-hosted arm defaults to llama.cpp's local port, so an
    /// unset `base_url` still lands on a working local endpoint. The
    /// profiled providers (`Ollama`, `DeepSeek`, `Grok`, `Azure`,
    /// `Moonshot`, `Zai`) take their default endpoint from their provider
    /// profile instead, and `Bedrock` derives its endpoint from the AWS
    /// region, so this returns `None` for all of them — the type itself
    /// keeps "defer to the profile" distinct from a configured host.
    /// Provider clients may append their own path suffixes on top of a
    /// host root.
    #[must_use]
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com"),
            Self::SelfHosted => Some("http://localhost:8080/v1"),
            Self::Ollama
            | Self::DeepSeek
            | Self::Grok
            | Self::Zai
            | Self::Azure
            | Self::Bedrock
            | Self::Moonshot => None,
        }
    }

    /// Every provider variant, the enumeration tests iterate.
    ///
    /// A manually maintained list, not a derived one: Rust has no
    /// exhaustive-enumeration reflection, so a variant added to
    /// [`ApiType`] is compiler-forced into
    /// [`Self::default_base_url`]'s match — the real enforcement point —
    /// but not into this list, whose per-variant pins would then silently
    /// skip it. `api_type_all_lists_each_variant_once` catches duplicates
    /// only — a mistyped variant path is a compile error; extend this
    /// list in the same change that adds a variant.
    pub const ALL: [ApiType; 11] = [
        ApiType::OpenAi,
        ApiType::Anthropic,
        ApiType::Gemini,
        ApiType::Ollama,
        ApiType::SelfHosted,
        ApiType::DeepSeek,
        ApiType::Grok,
        ApiType::Zai,
        ApiType::Azure,
        ApiType::Moonshot,
        ApiType::Bedrock,
    ];
}

/// Provider connection settings.
///
/// Everything needed to point the agent at a model endpoint: which model, where
/// it lives, how to authenticate, and the token/timeout limits to enforce. All
/// fields default via the manual [`Default`] impl below.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Primary model identifier.
    ///
    /// The model name the provider expects (e.g. `gpt-4o`). Defaults to an empty
    /// string, meaning "not yet configured".
    pub model: String,

    /// Base URL of the provider endpoint.
    ///
    /// The host root the API client builds request URLs against. Defaults to
    /// an empty string; the stock providers fall back to
    /// [`ApiType::default_base_url`], while the profiled providers fall back
    /// to their loopctl provider-profile endpoint.
    pub base_url: String,

    /// Which provider to speak to.
    ///
    /// Selects the wire protocol and default endpoint. Defaults to
    /// [`ApiType::Ollama`]. See [`ApiType`].
    pub api_type: ApiType,

    /// Optional API key; may also come from an env var.
    ///
    /// Sent as a bearer credential when present. Defaults to `None`; local
    /// providers like [`ApiType::Ollama`] do not need one.
    pub api_key: Option<String>,

    /// Max response tokens per turn.
    ///
    /// Caps the length of each model reply, in tokens. Defaults to `32_000`.
    pub max_tokens: u32,

    /// Context window size of the primary model, in tokens.
    ///
    /// The full window one request may carry. Unset (the default) means
    /// discover it from the provider when it starts — a local
    /// OpenAI-compatible server is asked directly, first its capability
    /// document and then its model listing; then the built-in table of
    /// documented cloud-model windows answers for the models it knows.
    /// Anything else falls back to [`FALLBACK_CONTEXT_WINDOW`]. An
    /// explicit value always wins over discovery. Feeds into compaction
    /// decisions via [`crate::DchConfig::to_session_config`].
    pub context_window: Option<u64>,

    /// Per-request timeout in seconds.
    ///
    /// How long a single API request may take before being aborted. Defaults to
    /// `120` seconds.
    pub request_timeout_secs: u64,

    /// Secondary model used if the primary errors out.
    ///
    /// Routes to this model once the primary has failed often enough to trip
    /// the fallback breaker — not on the first failure. Defaults to `None`,
    /// meaning no fallback is configured.
    pub fallback_model: Option<String>,

    /// Azure OpenAI resource name.
    ///
    /// The `{resource}` in `https://{resource}.openai.azure.com`. Consulted only
    /// for [`ApiType::Azure`]; falls back to the `AZURE_OPENAI_RESOURCE`
    /// environment variable when `None`.
    pub azure_resource: Option<String>,

    /// Request-level thinking budget for reasoning-capable models.
    ///
    /// One of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or
    /// `max`, mapped at provider construction onto the family's native
    /// dial — the OpenAI `reasoning_effort` label (each level's own
    /// name, verbatim), the Anthropic `thinking.budget_tokens` ladder,
    /// the Gemini `thinkingConfig.thinkingBudget` budget. `none` is the
    /// explicit off — the provider's own no-thinking value where one
    /// exists — while an unset field is inheritance: byte-identical
    /// requests and the provider's own thinking defaults. An
    /// unrecognized value fails client construction rather than
    /// silently defaulting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// Whether Anthropic-protocol requests carry prompt-cache breakpoints.
    ///
    /// When `true` (the default), requests to `[api] api_type` `anthropic`
    /// and `zai` mark the provider's prompt-cache breakpoints (tools,
    /// system, and the trailing message blocks), so the stable
    /// conversation prefix is served from cache across a session's turns
    /// instead of re-processed every request. Set `false` for an
    /// Anthropic-compatible endpoint that rejects the extra field.
    /// OpenAI-protocol providers cache automatically server-side and
    /// ignore this setting on the wire; the cached share is reported in
    /// usage either way. Loaded from `[api] prompt_cache`.
    pub prompt_cache: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            base_url: String::new(),
            api_type: ApiType::default(),
            api_key: None,
            max_tokens: 32_000,
            context_window: None,
            request_timeout_secs: 120,
            fallback_model: None,
            azure_resource: None,
            reasoning_effort: None,
            prompt_cache: true,
        }
    }
}

/// The reasoning-effort ladder's wire spellings, in rung order.
///
/// The words `[api] reasoning_effort` (and a `[[models]]` entry's
/// override of it) may carry — the same ladder the loop's client
/// construction maps onto the provider's own levels, spelled exactly
/// as the wire names that mapping reads. Public so the composer's
/// `/set` gate and the loader's validation enforce one list, and so
/// the loop's parse is pinned to it: a value outside this list fails
/// client construction at startup, which is precisely why neither the
/// loader nor the write-through may ever accept one.
pub const REASONING_EFFORT_WIRE_NAMES: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Whether `name` is a rung of the reasoning-effort ladder.
///
/// The single word-list check both config-side gates read — the
/// loader's `validate` and the composer's `/set` — so a value one
/// accepts, the other cannot refuse.
#[must_use]
pub fn is_reasoning_effort(name: &str) -> bool {
    REASONING_EFFORT_WIRE_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-field wrapper so serde rendering of a bare enum value
    /// can be asserted without a full config document.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct TestWrap {
        /// The variant under render.
        api_type: ApiType,
    }

    #[test]
    fn every_variant_round_trips_through_its_lowercase_name() {
        // The literals are the guard on the persisted on-disk format: a
        // variant rename or a rename_all change breaks a literal here,
        // not just the derived-name comparison in the same loop.
        let wire_names = [
            (ApiType::OpenAi, "openai"),
            (ApiType::Anthropic, "anthropic"),
            (ApiType::Gemini, "gemini"),
            (ApiType::Ollama, "ollama"),
            (ApiType::DeepSeek, "deepseek"),
            (ApiType::Grok, "grok"),
            (ApiType::Zai, "zai"),
            (ApiType::Azure, "azure"),
            (ApiType::Moonshot, "moonshot"),
            (ApiType::Bedrock, "bedrock"),
        ];
        for (variant, name) in wire_names {
            let parsed: ApiConfig = toml::from_str(&format!("api_type = \"{name}\"\n")).unwrap();
            assert_eq!(
                parsed.api_type, variant,
                "wire name {name} must keep mapping to {variant:?}"
            );
            assert_eq!(
                format!("{variant:?}").to_lowercase(),
                name,
                "{variant:?} must derive its literal wire name"
            );
            let source = ApiConfig {
                api_type: variant,
                ..ApiConfig::default()
            };
            let written = toml::to_string(&source).unwrap();
            let reparsed: ApiConfig = toml::from_str(&written).unwrap();
            assert_eq!(
                reparsed, source,
                "{variant:?} must survive a serialize/deserialize round trip \
                 whole — an unset Option coming back as an empty string would \
                 turn a missing key into a present-but-empty credential"
            );
        }
    }

    #[test]
    fn default_base_url_pins_stock_hosts_and_defers_every_other_variant() {
        // A reintroduced hard-coded host here would silently shadow the
        // endpoint its provider profile or AWS region derives; iterating
        // ALL keeps a future variant from escaping the guard.
        let stock = [
            (ApiType::OpenAi, "https://api.openai.com/v1"),
            (ApiType::Anthropic, "https://api.anthropic.com"),
            (ApiType::Gemini, "https://generativelanguage.googleapis.com"),
            (ApiType::SelfHosted, "http://localhost:8080/v1"),
        ];
        for api_type in ApiType::ALL {
            match stock.iter().find(|(variant, _)| *variant == api_type) {
                Some((_, host)) => assert_eq!(
                    api_type.default_base_url(),
                    Some(*host),
                    "{api_type:?} must keep its stock host pinned"
                ),
                None => assert_eq!(
                    api_type.default_base_url(),
                    None,
                    "{api_type:?} must defer to its profile- or region-derived endpoint"
                ),
            }
        }
        for (variant, _) in stock {
            assert!(
                ApiType::ALL.contains(&variant),
                "{variant:?} must stay in ALL for its host pin to run"
            );
        }
    }

    #[test]
    fn self_hosted_round_trips_its_snake_case_spelling() {
        let wrap: TestWrap =
            toml::from_str("api_type = \"self_hosted\"").expect("the snake_case spelling parses");
        assert_eq!(wrap.api_type, super::ApiType::SelfHosted);
        let rendered = toml::to_string(&TestWrap {
            api_type: super::ApiType::SelfHosted,
        })
        .expect("renders");
        assert!(
            rendered.contains("self_hosted"),
            "the rendered TOML keeps the snake_case spelling: {rendered}"
        );
    }

    #[test]
    fn api_type_all_lists_each_variant_once() {
        // Only the duplicate half is checkable from here: a variant
        // missing from ALL must be caught by the change that adds it —
        // the enum's exhaustive matches are the enforcement point.
        let mut seen = std::collections::HashSet::new();
        for api_type in ApiType::ALL {
            assert!(
                seen.insert(api_type),
                "duplicate variant in ALL: {api_type:?}"
            );
        }
    }
}
