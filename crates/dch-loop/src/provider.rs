//! Construction of the loopctl API client from dch configuration.
//!
//! Maps the provider settings in [`dch_config::ApiConfig`] onto loopctl's
//! concrete provider clients, wrapping the result in the [`DchClient`] enum
//! the agent loop monomorphizes over.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dch_config::ApiConfig;
use dch_config::ApiType;
use futures::Stream;
use loopctl::api::ApiClient;
use loopctl::api::NonStreamingResponse;
use loopctl::api::StreamRequest;
use loopctl::api::error::ApiError;
use loopctl::message::Message;
use loopctl::provider::AnthropicClient;
use loopctl::provider::BedrockClient;
use loopctl::provider::GeminiClient;
use loopctl::provider::OpenAiClient;
use loopctl::stream::StreamEvent;

use crate::error::RunnerError;

/// The concrete provider client dch monomorphizes the agent loop over.
///
/// A runtime-selected enum over loopctl's four provider client families, so
/// the agent loop's per-turn LLM call is statically dispatched rather than
/// going through `dyn ApiClient`. [`create_client`] picks the variant from
/// [`ApiConfig::api_type`] (by wire-protocol family: OpenAI-compatible
/// providers map to [`OpenAi`], Anthropic-compatible to [`Anthropic`], Gemini
/// to [`Gemini`], AWS Bedrock to [`Bedrock`]); every other method on
/// `DchClient` forwards to the inner client unchanged.
///
/// [`OpenAi`]: Self::OpenAi
/// [`Anthropic`]: Self::Anthropic
/// [`Gemini`]: Self::Gemini
/// [`Bedrock`]: Self::Bedrock
pub enum DchClient {
    /// An OpenAI-protocol provider client.
    ///
    /// Selected for the OpenAI wire-protocol family: `OpenAi`, `Ollama`,
    /// `DeepSeek`, and `Grok` all speak the OpenAI chat-completions API (the
    /// latter three via a custom `base_url`). Wraps loopctl's `OpenAiClient`,
    /// which the other `ApiClient` methods forward to.
    OpenAi(OpenAiClient),

    /// An Anthropic-protocol provider client.
    ///
    /// Selected for the Anthropic wire-protocol family: `Anthropic` and `Zai`
    /// (the latter via a custom `base_url`). Wraps loopctl's `AnthropicClient`,
    /// which the other `ApiClient` methods forward to.
    Anthropic(AnthropicClient),

    /// A Google Gemini provider client.
    ///
    /// Selected for `Gemini`, which uses its own wire protocol distinct from
    /// the OpenAI and Anthropic families. Wraps loopctl's `GeminiClient`, which
    /// the other `ApiClient` methods forward to.
    Gemini(GeminiClient),

    /// An AWS Bedrock provider client.
    ///
    /// Selected for `Bedrock`: the native SigV4-authenticated endpoint whose
    /// wire protocol (Anthropic-style for `anthropic.*` models, Converse for
    /// the rest) loopctl's `BedrockClient` owns. The other `ApiClient` methods
    /// forward to it.
    Bedrock(BedrockClient),
}

impl ApiClient for DchClient {
    fn model(&self) -> String {
        match self {
            Self::OpenAi(c) => c.model(),
            Self::Anthropic(c) => c.model(),
            Self::Gemini(c) => c.model(),
            Self::Bedrock(c) => c.model(),
        }
    }

    fn set_model(&self, model: &str) -> bool {
        match self {
            Self::OpenAi(c) => c.set_model(model),
            Self::Anthropic(c) => c.set_model(model),
            Self::Gemini(c) => c.set_model(model),
            Self::Bedrock(c) => c.set_model(model),
        }
    }

    fn base_url(&self) -> String {
        match self {
            Self::OpenAi(c) => c.base_url(),
            Self::Anthropic(c) => c.base_url(),
            Self::Gemini(c) => c.base_url(),
            Self::Bedrock(c) => c.base_url(),
        }
    }

    fn stream_messages(
        &self,
        request: &StreamRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send + 'static>> {
        match self {
            Self::OpenAi(c) => c.stream_messages(request),
            Self::Anthropic(c) => c.stream_messages(request),
            Self::Gemini(c) => c.stream_messages(request),
            Self::Bedrock(c) => c.stream_messages(request),
        }
    }

    fn create_message(
        &self,
        request: &StreamRequest,
    ) -> Pin<Box<dyn Future<Output = Result<NonStreamingResponse, ApiError>> + Send + '_>> {
        match self {
            Self::OpenAi(c) => c.create_message(request),
            Self::Anthropic(c) => c.create_message(request),
            Self::Gemini(c) => c.create_message(request),
            Self::Bedrock(c) => c.create_message(request),
        }
    }

    fn stream_messages_with_options(
        &self,
        request: &StreamRequest,
        options: loopctl::structured::RequestOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send + 'static>> {
        match self {
            Self::OpenAi(c) => c.stream_messages_with_options(request, options),
            Self::Anthropic(c) => c.stream_messages_with_options(request, options),
            Self::Gemini(c) => c.stream_messages_with_options(request, options),
            Self::Bedrock(c) => c.stream_messages_with_options(request, options),
        }
    }

    fn create_message_with_options(
        &self,
        request: &StreamRequest,
        options: loopctl::structured::RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<NonStreamingResponse, ApiError>> + Send + '_>> {
        match self {
            Self::OpenAi(c) => c.create_message_with_options(request, options),
            Self::Anthropic(c) => c.create_message_with_options(request, options),
            Self::Gemini(c) => c.create_message_with_options(request, options),
            Self::Bedrock(c) => c.create_message_with_options(request, options),
        }
    }

    fn extract_structured(&self, message: &Message) -> serde_json::Value {
        match self {
            Self::OpenAi(c) => c.extract_structured(message),
            Self::Anthropic(c) => c.extract_structured(message),
            Self::Gemini(c) => c.extract_structured(message),
            Self::Bedrock(c) => c.extract_structured(message),
        }
    }
}

/// Build a [`DchClient`] for the provider named by `config.api_type`.
///
/// Variants are mapped by wire-protocol family: OpenAI-compatible providers
/// (`OpenAi`, `Ollama`, `DeepSeek`, `Grok`, `Azure`, `Moonshot`) wrap an
/// [`OpenAiClient`]; Anthropic-compatible providers (`Anthropic`, `Zai`)
/// wrap an [`AnthropicClient`]; `Gemini` wraps a [`GeminiClient`]. The
/// profiled providers start from loopctl's pre-seeded profile builders,
/// which own their endpoints, credential variables, and default models;
/// dch layers its config precedence on top (config values replace the
/// seeds; `request_timeout_secs` always applies). An empty `base_url`
/// falls back to [`ApiType::default_base_url`] for the stock providers and
/// to the seeded profile endpoint for the profiled ones. Ollama's profile
/// carries no default model, so the raw `config.model` seeds its builder:
/// the out-of-box configuration (empty model) builds a client whose model
/// is empty — a model must be named in the config or on the command line
/// before a request can succeed.
///
/// # API-key resolution
///
/// `config.api_key` wins for every provider that accepts a key; Bedrock
/// is the exception — it rejects a configured `api_key` (authentication
/// is `SigV4` via the `AWS_*` environment variables). When `None`, the stock
/// providers fall back to their conventional environment variables
/// (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` or
/// `GOOGLE_API_KEY`); the profiled providers inherit their profile's
/// seeded variable (for example `DEEPSEEK_API_KEY`, `MOONSHOT_API_KEY`)
/// from the builder, and Ollama tolerates a missing key. If a required
/// key is missing, returns [`RunnerError::Client`] naming the expected
/// environment variable.
///
/// # Errors
///
/// - [`RunnerError::Client`] if a required API key or model is missing, a
///   profiled fact is invalid (for example a malformed Azure resource
///   name), or the underlying HTTP client cannot be constructed.
pub fn create_client(config: &ApiConfig) -> Result<DchClient, RunnerError> {
    let timeout = Duration::from_secs(config.request_timeout_secs);
    let client_error = |error: ApiError| RunnerError::Client(error.to_string());

    let client = match config.api_type {
        ApiType::OpenAi => {
            let base_url = effective_base_url(config);
            let api_key = resolve_api_key(config)?;
            DchClient::OpenAi(
                OpenAiClient::builder()
                    .with_api_key(api_key)
                    .with_base_url(base_url)
                    .with_model(config.model.as_str())
                    .with_timeout(timeout)
                    .build()
                    .map_err(client_error)?,
            )
        }
        ApiType::Anthropic => {
            let base_url = effective_base_url(config);
            let api_key = resolve_api_key(config)?;
            DchClient::Anthropic(
                AnthropicClient::builder()
                    .with_api_key(api_key)
                    .with_base_url(base_url)
                    .with_model(config.model.as_str())
                    .with_max_tokens(config.max_tokens)
                    .with_timeout(timeout)
                    .build()
                    .map_err(client_error)?,
            )
        }
        ApiType::Gemini => {
            let base_url = effective_base_url(config);
            let api_key = resolve_api_key(config)?;
            DchClient::Gemini(
                GeminiClient::builder()
                    .with_api_key(api_key)
                    .with_base_url(base_url)
                    .with_model(config.model.as_str())
                    .with_timeout(timeout)
                    .build()
                    .map_err(client_error)?,
            )
        }
        ApiType::Ollama => DchClient::OpenAi(
            profiled(
                loopctl::provider::ollama_builder(config.model.as_str()),
                config,
            )
            .build()
            .map_err(client_error)?,
        ),
        ApiType::DeepSeek => DchClient::OpenAi(
            profiled(loopctl::provider::deepseek_builder(), config)
                .build()
                .map_err(client_error)?,
        ),
        ApiType::Grok => DchClient::OpenAi(
            profiled(loopctl::provider::grok_builder(), config)
                .build()
                .map_err(client_error)?,
        ),
        ApiType::Azure => DchClient::OpenAi(
            profiled(azure_seeded_builder(config)?, config)
                .build()
                .map_err(client_error)?,
        ),
        ApiType::Moonshot => DchClient::OpenAi(
            profiled(loopctl::provider::moonshot_builder(), config)
                .build()
                .map_err(client_error)?,
        ),
        ApiType::Zai => DchClient::Anthropic(
            profiled(loopctl::provider::zai_builder(), config)
                .build()
                .map_err(client_error)?,
        ),
        ApiType::Bedrock => DchClient::Bedrock(build_bedrock(config)?),
    };
    Ok(client)
}

/// Resource-name seed for gateway deployments that set `base_url` explicitly.
///
/// The profile builder derives its endpoint from a syntactically valid
/// resource name, so a gateway configuration seeds this placeholder
/// whenever `base_url` is set — the configured resource, valid or not, is
/// irrelevant behind an overridden endpoint; the configured `base_url`
/// replaces the derived endpoint immediately and the seed never reaches a
/// request.
const GATEWAY_RESOURCE_SEED: &str = "gateway";

/// Start the Azure profile builder for `config`.
///
/// The endpoint is derived from the resource name, which comes from
/// [`ApiConfig::azure_resource`] or, when that is unset or empty, the
/// `AZURE_OPENAI_RESOURCE` environment variable. An explicitly configured
/// `base_url` (a gateway or proxy deployment) replaces the derived
/// endpoint, so the resource is irrelevant there and the placeholder seed
/// stands in for it.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] when the endpoint must be derived but no
/// resource name resolves.
fn azure_seeded_builder(
    config: &ApiConfig,
) -> Result<loopctl::provider::OpenAiClientBuilder, RunnerError> {
    if config.base_url.is_empty() {
        Ok(loopctl::provider::azure_builder(azure_resource(config)?))
    } else {
        Ok(loopctl::provider::azure_builder(GATEWAY_RESOURCE_SEED))
    }
}

/// Resolve the Azure resource name for `config`.
///
/// [`ApiConfig::azure_resource`] wins when non-empty; otherwise the
/// `AZURE_OPENAI_RESOURCE` environment variable supplies it. The name forms
/// the deployment endpoint, and loopctl's builder validates its shape at
/// build time.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] naming both sources when neither is set.
fn azure_resource(config: &ApiConfig) -> Result<String, RunnerError> {
    config
        .azure_resource
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("AZURE_OPENAI_RESOURCE")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| {
            RunnerError::Client(
                "azure: no resource name: set api.azure_resource or AZURE_OPENAI_RESOURCE"
                    .to_string(),
            )
        })
}

/// The config precedence both profile families apply on top of their
/// seeded builder.
///
/// One implementation per family keeps dch's defaults-in-waiting contract
/// in a single place: config values replace the seeds only when the
/// config carries them, and the read timeout always applies.
trait Profiled {
    /// Replace the seeded endpoint with the configured `base_url`.
    fn apply_base_url(self, url: &str) -> Self;

    /// Replace the seeded model with the configured `model`.
    fn apply_model(self, model: &str) -> Self;

    /// Replace the seeded credential with the configured `api_key`.
    fn apply_api_key(self, key: &str) -> Self;

    /// Cap each request's read gap at the configured timeout.
    fn apply_timeout(self, timeout: Duration) -> Self;

    /// Bound each reply at the configured completion budget. Families
    /// without a completion knob ignore it.
    fn apply_max_tokens(self, tokens: u32) -> Self;
}

impl Profiled for loopctl::provider::OpenAiClientBuilder {
    fn apply_base_url(self, url: &str) -> Self {
        loopctl::provider::OpenAiClientBuilder::with_base_url(self, url)
    }

    fn apply_model(self, model: &str) -> Self {
        loopctl::provider::OpenAiClientBuilder::with_model(self, model)
    }

    fn apply_api_key(self, key: &str) -> Self {
        loopctl::provider::OpenAiClientBuilder::with_api_key(self, key)
    }

    fn apply_timeout(self, timeout: Duration) -> Self {
        loopctl::provider::OpenAiClientBuilder::with_timeout(self, timeout)
    }

    fn apply_max_tokens(self, _tokens: u32) -> Self {
        self
    }
}

impl Profiled for loopctl::provider::AnthropicClientBuilder {
    fn apply_base_url(self, url: &str) -> Self {
        loopctl::provider::AnthropicClientBuilder::with_base_url(self, url)
    }

    fn apply_model(self, model: &str) -> Self {
        loopctl::provider::AnthropicClientBuilder::with_model(self, model)
    }

    fn apply_api_key(self, key: &str) -> Self {
        loopctl::provider::AnthropicClientBuilder::with_api_key(self, key)
    }

    fn apply_timeout(self, timeout: Duration) -> Self {
        loopctl::provider::AnthropicClientBuilder::with_timeout(self, timeout)
    }

    fn apply_max_tokens(self, tokens: u32) -> Self {
        loopctl::provider::AnthropicClientBuilder::with_max_tokens(self, tokens)
    }
}

/// Apply dch's config precedence onto a pre-seeded profile builder.
///
/// The seeds are defaults-in-waiting: `base_url` and `model` replace the
/// seeded values only when the config carries them, and `api_key` only
/// when configured — so config-beats-environment falls out of the
/// ordering. `request_timeout_secs` and `max_tokens` are family settings
/// that always apply.
fn profiled<B: Profiled>(builder: B, config: &ApiConfig) -> B {
    let builder = if config.base_url.is_empty() {
        builder
    } else {
        builder.apply_base_url(&config.base_url)
    };
    let builder = if config.model.is_empty() {
        builder
    } else {
        builder.apply_model(&config.model)
    };
    let builder = match &config.api_key {
        Some(key) => builder.apply_api_key(key),
        None => builder,
    };
    builder
        .apply_max_tokens(config.max_tokens)
        .apply_timeout(Duration::from_secs(config.request_timeout_secs))
}

/// Build the Bedrock client for `config`.
///
/// Credentials come from the standard `AWS_*` environment variables
/// (`AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`); Bedrock signs
/// requests with `SigV4`, so a configured `api_key` or `base_url` is rejected
/// loudly instead of being silently ignored. [`ApiConfig::model`] must name the
/// model (or inference-profile) id — Bedrock has no portable default.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] when the `AWS_*` environment is incomplete,
/// `api_key`/`base_url` are set, or `model` is empty.
fn build_bedrock(config: &ApiConfig) -> Result<BedrockClient, RunnerError> {
    if config.api_key.is_some() {
        return Err(RunnerError::Client(
            "bedrock: api_key does not apply; authentication is SigV4 via AWS_* env vars"
                .to_string(),
        ));
    }
    if !config.base_url.is_empty() {
        return Err(RunnerError::Client(
            "bedrock: base_url does not apply; the endpoint is derived from AWS_REGION".to_string(),
        ));
    }
    if config.model.is_empty() {
        return Err(RunnerError::Client(
            "bedrock: api.model must name the model or inference-profile id".to_string(),
        ));
    }
    BedrockClient::from_env()
        .map_err(|e| RunnerError::Client(e.to_string()))
        .and_then(|client| {
            client
                .set_model(&config.model)
                .then_some(client)
                .ok_or_else(|| RunnerError::Client("bedrock: model could not be set".to_string()))
        })
}

/// Resolve the effective API base URL for `config`.
///
/// Returns the configured [`ApiConfig::base_url`] verbatim when the user set
/// one; otherwise falls back to the provider's stock default. This is what
/// lets a config omit `base_url` entirely (the common case for stock
/// OpenAI/Anthropic/Gemini) while still allowing an override for self-hosted
/// or proxy deployments. Only the stock arms call this helper — the profiled
/// providers seed their endpoint from their builder — and every stock
/// provider has a default, so the `None` arm of
/// [`ApiType::default_base_url`] cannot be reached from here.
fn effective_base_url(config: &ApiConfig) -> String {
    if config.base_url.is_empty() {
        config.api_type.default_base_url().unwrap_or("").to_owned()
    } else {
        config.base_url.clone()
    }
}

/// Resolve the API key for `config`.
///
/// Resolution is uniform across the stock providers: `config.api_key` wins;
/// otherwise each provider's candidate environment variables are tried in
/// order. A miss yields a [`RunnerError::Client`] naming the variables that
/// were tried. The profiled providers (`Ollama`, `DeepSeek`, `Grok`, `Azure`,
/// Moonshot, Zai) resolve theirs through the seeded profile builders and do
/// not pass through here.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] when the key is neither configured nor
/// available in any of the provider's environment variables.
fn resolve_api_key(config: &ApiConfig) -> Result<String, RunnerError> {
    if let Some(key) = &config.api_key {
        return Ok(key.clone());
    }
    let candidates = candidate_env_vars(config.api_type);
    for var in &candidates {
        if let Ok(key) = std::env::var(var) {
            return Ok(key);
        }
    }
    match candidates.as_slice() {
        [] => Err(RunnerError::Client(
            "no API key: `api_key` not set".to_string(),
        )),
        [single] => Err(RunnerError::Client(format!(
            "no API key: `api_key` not set and env var {single} is unset"
        ))),
        multiple => Err(RunnerError::Client(format!(
            "no API key: `api_key` not set and none of {} set",
            multiple.join(" / ")
        ))),
    }
}

/// Candidate API-key environment variables for the stock providers, in
/// fallback order.
///
/// Consulted by [`resolve_api_key`] when [`ApiConfig::api_key`] is unset, so
/// a user can avoid putting the key in the config file by exporting it. Each
/// stock provider maps to the env var its official client reads, and `Gemini`
/// tries both `GEMINI_API_KEY` and the older `GOOGLE_API_KEY`. The profiled
/// providers' variables are owned by their loopctl profile builders.
fn candidate_env_vars(api_type: ApiType) -> Vec<&'static str> {
    match api_type {
        ApiType::OpenAi => vec!["OPENAI_API_KEY"],
        ApiType::Anthropic => vec!["ANTHROPIC_API_KEY"],
        ApiType::Gemini => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        ApiType::Ollama
        | ApiType::DeepSeek
        | ApiType::Grok
        | ApiType::Azure
        | ApiType::Moonshot
        | ApiType::Zai
        | ApiType::Bedrock => Vec::new(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::field_reassign_with_default
)]
mod tests {
    use dch_config::ApiConfig;
    use dch_config::ApiType;

    use super::*;
    use crate::RunnerError;

    fn cfg(api_type: ApiType, base_url: &str, key: Option<&str>) -> ApiConfig {
        ApiConfig {
            api_type,
            base_url: base_url.to_string(),
            api_key: key.map(String::from),
            model: "test-model".to_string(),
            max_tokens: 1024,
            ..ApiConfig::default()
        }
    }

    #[test]
    fn openai_happy_path() {
        let c = cfg(ApiType::OpenAi, "https://api.openai.com/v1", Some("k"));
        let client = create_client(&c).expect("openai builds");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn anthropic_happy_path() {
        let c = cfg(ApiType::Anthropic, "https://api.anthropic.com", Some("k"));
        let client = create_client(&c).expect("anthropic builds");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn ollama_via_base_url_no_key() {
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_API_KEY"]);
        env.remove("OLLAMA_API_KEY");
        let c = cfg(ApiType::Ollama, "http://localhost:11434/v1", None);
        let client = create_client(&c).expect("ollama builds with no key");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn ollama_empty_base_url_uses_default() {
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_API_KEY", "OLLAMA_BASE_URL"]);
        env.remove("OLLAMA_API_KEY");
        env.remove("OLLAMA_BASE_URL");
        let c = cfg(ApiType::Ollama, "", None);
        let client = create_client(&c).expect("ollama builds via default base_url");
        assert_eq!(client.model(), "test-model");
        assert_eq!(
            client.base_url(),
            "http://localhost:11434/v1",
            "an empty base_url must fall back to the seeded local Ollama endpoint"
        );
    }

    #[test]
    fn ollama_env_base_url_overrides_the_seed() {
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_BASE_URL"]);
        env.set("OLLAMA_BASE_URL", "http://env-host:11434/v1");
        let c = cfg(ApiType::Ollama, "", None);
        let client = create_client(&c).expect("ollama builds from the env endpoint");
        assert_eq!(
            client.base_url(),
            "http://env-host:11434/v1",
            "an empty base_url must honor the profile's OLLAMA_BASE_URL variable"
        );
        env.remove("OLLAMA_BASE_URL");
    }

    #[test]
    fn ollama_cloud_key_from_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_API_KEY"]);
        env.set("OLLAMA_API_KEY", "env-key");
        let c = cfg(ApiType::Ollama, "https://cloud.example.com/v1", None);
        let client = create_client(&c).expect("cloud ollama builds with OLLAMA_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("OLLAMA_API_KEY");
    }

    #[test]
    fn deepseek_via_base_url() {
        let c = cfg(ApiType::DeepSeek, "https://api.deepseek.com", Some("k"));
        let client = create_client(&c).expect("deepseek builds");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn grok_via_base_url() {
        let c = cfg(ApiType::Grok, "https://api.x.ai/v1", Some("k"));
        let client = create_client(&c).expect("grok builds");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn zai_via_anthropic() {
        let c = cfg(ApiType::Zai, "https://api.z.ai/api", Some("k"));
        let client = create_client(&c).expect("zai builds");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn gemini_happy_path() {
        let c = cfg(
            ApiType::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            Some("k"),
        );
        let client = create_client(&c).expect("gemini builds");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn openai_key_from_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["OPENAI_API_KEY"]);
        env.set("OPENAI_API_KEY", "env-key");
        let c = cfg(ApiType::OpenAi, "https://api.openai.com/v1", None);
        let client = create_client(&c).expect("openai builds with env key");
        assert_eq!(client.model(), "test-model");
        env.remove("OPENAI_API_KEY");
    }

    #[test]
    fn deepseek_key_from_deepseek_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["DEEPSEEK_API_KEY"]);
        env.set("DEEPSEEK_API_KEY", "env-key");
        let c = cfg(ApiType::DeepSeek, "", None);
        let client = create_client(&c).expect("deepseek builds with DEEPSEEK_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("DEEPSEEK_API_KEY");
    }

    #[test]
    fn deepseek_empty_model_seeds_the_profile_default() {
        let env = loopctl::testing::EnvGuard::acquire(&["DEEPSEEK_API_KEY", "DEEPSEEK_MODEL"]);
        env.set("DEEPSEEK_API_KEY", "env-key");
        env.remove("DEEPSEEK_MODEL");
        let mut c = cfg(ApiType::DeepSeek, "", None);
        c.model = String::new();
        let client = create_client(&c).expect("the profile default model applies");
        assert_eq!(client.model(), "deepseek-chat");
        env.remove("DEEPSEEK_API_KEY");
    }

    #[test]
    fn grok_key_from_xai_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["XAI_API_KEY", "GROK_API_KEY"]);
        env.set("XAI_API_KEY", "env-key");
        env.remove("GROK_API_KEY");
        let c = cfg(ApiType::Grok, "", None);
        let client = create_client(&c).expect("grok builds with XAI_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("XAI_API_KEY");
    }

    #[test]
    fn grok_key_falls_back_to_the_grok_alias() {
        let env = loopctl::testing::EnvGuard::acquire(&["XAI_API_KEY", "GROK_API_KEY"]);
        env.remove("XAI_API_KEY");
        env.set("GROK_API_KEY", "env-key");
        let c = cfg(ApiType::Grok, "", None);
        let client = create_client(&c).expect("grok builds with the GROK_API_KEY alias");
        assert_eq!(client.model(), "test-model");
        env.remove("GROK_API_KEY");
    }

    #[test]
    fn anthropic_key_from_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["ANTHROPIC_API_KEY"]);
        env.set("ANTHROPIC_API_KEY", "env-key");
        let c = cfg(ApiType::Anthropic, "https://api.anthropic.com", None);
        let client = create_client(&c).expect("anthropic builds with env key");
        assert_eq!(client.model(), "test-model");
        env.remove("ANTHROPIC_API_KEY");
    }

    #[test]
    fn missing_key_clear_error() {
        let env = loopctl::testing::EnvGuard::acquire(&["OPENAI_API_KEY"]);
        env.remove("OPENAI_API_KEY");
        let c = cfg(ApiType::OpenAi, "https://api.openai.com/v1", None);
        let err = create_client(&c)
            .err()
            .expect("openai without key should error");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("OPENAI_API_KEY"),
            "error message should name the env var: {msg}"
        );
    }

    #[test]
    fn deepseek_missing_key_names_the_expected_env_var() {
        let env = loopctl::testing::EnvGuard::acquire(&["DEEPSEEK_API_KEY"]);
        env.remove("DEEPSEEK_API_KEY");
        let c = cfg(ApiType::DeepSeek, "", None);
        let err = create_client(&c)
            .err()
            .expect("deepseek without key should error");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("DEEPSEEK_API_KEY"),
            "the profiled missing-key error should name the env var: {msg}"
        );
    }

    #[test]
    fn zai_key_from_zai_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["ZAI_API_KEY", "ZHIPUAI_API_KEY"]);
        env.set("ZAI_API_KEY", "env-key");
        env.remove("ZHIPUAI_API_KEY");
        let c = cfg(ApiType::Zai, "", None);
        let client = create_client(&c).expect("zai builds with ZAI_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("ZAI_API_KEY");
    }

    #[test]
    fn gemini_key_from_gemini_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
        env.remove("GOOGLE_API_KEY");
        env.set("GEMINI_API_KEY", "env-key");
        let c = cfg(
            ApiType::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            None,
        );
        let client = create_client(&c).expect("gemini builds with GEMINI_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("GEMINI_API_KEY");
    }

    #[test]
    fn gemini_key_falls_back_to_google_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
        env.remove("GEMINI_API_KEY");
        env.set("GOOGLE_API_KEY", "env-key");
        let c = cfg(
            ApiType::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            None,
        );
        let client = create_client(&c).expect("gemini builds with GOOGLE_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("GOOGLE_API_KEY");
    }

    #[test]
    fn gemini_missing_key_names_both_vars() {
        let env = loopctl::testing::EnvGuard::acquire(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
        env.remove("GEMINI_API_KEY");
        env.remove("GOOGLE_API_KEY");
        let c = cfg(
            ApiType::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            None,
        );
        let err = create_client(&c)
            .err()
            .expect("gemini without key should error");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("GEMINI_API_KEY") && msg.contains("GOOGLE_API_KEY"),
            "error message should name both env vars: {msg}"
        );
    }

    #[test]
    fn default_api_config_builds() {
        // ApiConfig::default() is api_type=Ollama, empty base_url, no key.
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_BASE_URL"]);
        env.remove("OLLAMA_BASE_URL");
        let mut c = ApiConfig::default();
        c.model = "default-model".to_string();
        let client = create_client(&c).expect("default ApiConfig should build");
        assert_eq!(client.model(), "default-model");
        assert_eq!(
            client.base_url(),
            "http://localhost:11434/v1",
            "the out-of-box configuration must land on the seeded local Ollama endpoint"
        );
    }

    #[test]
    fn default_api_config_without_a_model_builds_an_empty_model_client() {
        // Ollama's profile has no default model to fall back on, so the
        // out-of-box client builds with an empty one — the documented
        // boundary, distinct from a seeded default like deepseek-chat.
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_BASE_URL"]);
        env.remove("OLLAMA_BASE_URL");
        let client = create_client(&ApiConfig::default())
            .expect("the out-of-box configuration builds without a model");
        assert_eq!(
            client.model(),
            "",
            "no config model and no profile default means an empty model"
        );
    }

    #[test]
    fn dchclient_variant_matches_api_type_family() {
        // OpenAI-protocol family → DchClient::OpenAi.
        for api_type in [
            ApiType::OpenAi,
            ApiType::Ollama,
            ApiType::DeepSeek,
            ApiType::Grok,
        ] {
            let c = cfg(api_type, "https://example.invalid", Some("k"));
            let DchClient::OpenAi(_) = create_client(&c).expect("builds") else {
                panic!("{api_type:?} should map to DchClient::OpenAi");
            };
        }
        // Anthropic-protocol family → DchClient::Anthropic.
        for api_type in [ApiType::Anthropic, ApiType::Zai] {
            let c = cfg(api_type, "https://example.invalid", Some("k"));
            let DchClient::Anthropic(_) = create_client(&c).expect("builds") else {
                panic!("{api_type:?} should map to DchClient::Anthropic");
            };
        }
        // Gemini → DchClient::Gemini.
        let c = cfg(ApiType::Gemini, "https://example.invalid", Some("k"));
        let DchClient::Gemini(_) = create_client(&c).expect("builds") else {
            panic!("Gemini should map to DchClient::Gemini");
        };
    }

    #[test]
    fn dchclient_forwards_model_and_set_model_to_the_inner_provider() {
        let c = cfg(ApiType::OpenAi, "https://api.openai.com/v1", Some("k"));
        let client = create_client(&c).expect("openai builds");
        assert_eq!(client.model(), "test-model");
        assert!(
            client.set_model("other-model"),
            "OpenAiClient supports runtime model swap"
        );
        assert_eq!(client.model(), "other-model");
    }

    #[test]
    fn dchclient_forwards_base_url_to_the_inner_provider() {
        let c = cfg(
            ApiType::Anthropic,
            "https://api.anthropic.example",
            Some("k"),
        );
        let client = create_client(&c).expect("anthropic builds");
        assert_eq!(client.base_url(), "https://api.anthropic.example");
    }

    #[test]
    fn dchclient_forwards_set_model_and_base_url_on_every_variant() {
        for (api_type, base_url) in [
            (ApiType::OpenAi, "https://openai.example/v1"),
            (ApiType::Anthropic, "https://anthropic.example"),
            (ApiType::Gemini, "https://gemini.example/v1beta"),
        ] {
            let c = cfg(api_type, base_url, Some("k"));
            let client = create_client(&c).expect("builds");
            assert_eq!(
                client.base_url(),
                base_url,
                "{api_type:?} must forward base_url"
            );
            assert!(
                client.set_model("renamed-model"),
                "{api_type:?} must forward set_model"
            );
            assert_eq!(
                client.model(),
                "renamed-model",
                "{api_type:?} must forward model"
            );
        }
    }

    #[test]
    fn azure_builds_from_env_profile() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.set("AZURE_OPENAI_RESOURCE", "my-resource");
        env.set("AZURE_OPENAI_API_KEY", "env-key");
        env.set("AZURE_OPENAI_MODEL", "deployment-a");
        let mut c = cfg(ApiType::Azure, "", None);
        c.model = String::new();
        c.azure_resource = None;
        let client = create_client(&c).expect("azure builds from env");
        assert_eq!(client.model(), "deployment-a");
    }

    #[test]
    fn azure_resource_from_config_wins_and_model_overrides() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.remove("AZURE_OPENAI_RESOURCE");
        env.set("AZURE_OPENAI_API_KEY", "env-key");
        env.set("AZURE_OPENAI_MODEL", "deployment-a");
        let mut c = cfg(ApiType::Azure, "", None);
        c.azure_resource = Some("configured-resource".to_string());
        c.model = "configured-model".to_string();
        let client = create_client(&c).expect("azure builds with configured resource");
        assert_eq!(client.model(), "configured-model");
    }

    #[test]
    fn azure_missing_resource_names_both_sources() {
        let env = loopctl::testing::EnvGuard::acquire(&["AZURE_OPENAI_RESOURCE"]);
        env.remove("AZURE_OPENAI_RESOURCE");
        let c = cfg(ApiType::Azure, "", None);
        let err = create_client(&c)
            .err()
            .expect("azure without resource errors");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("AZURE_OPENAI_RESOURCE") && msg.contains("azure_resource"),
            "error must name both sources: {msg}"
        );
    }

    #[test]
    fn azure_missing_model_names_the_deployment_variable() {
        let env = loopctl::testing::EnvGuard::acquire(&["AZURE_OPENAI_MODEL"]);
        env.remove("AZURE_OPENAI_MODEL");
        let mut c = cfg(ApiType::Azure, "", Some("k"));
        c.model = String::new();
        c.azure_resource = Some("configured-resource".to_string());
        let err = create_client(&c)
            .err()
            .expect("azure without a model should error");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("AZURE_OPENAI_MODEL"),
            "the missing-model error should name the env var: {msg}"
        );
    }

    #[test]
    fn moonshot_builds_from_env_profile() {
        let env = loopctl::testing::EnvGuard::acquire(&["MOONSHOT_API_KEY", "MOONSHOT_MODEL"]);
        env.set("MOONSHOT_API_KEY", "env-key");
        env.remove("MOONSHOT_MODEL");
        let c = cfg(ApiType::Moonshot, "", None);
        let client = create_client(&c).expect("moonshot builds from env");
        let DchClient::OpenAi(_) = client else {
            panic!("moonshot rides the OpenAI-protocol variant");
        };
    }

    #[test]
    fn moonshot_base_url_override_builds_directly() {
        let c = cfg(
            ApiType::Moonshot,
            "https://moonshot-proxy.example/v1",
            Some("k"),
        );
        let client = create_client(&c).expect("moonshot with base_url builds directly");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn azure_config_api_key_builds_without_env_key() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.set("AZURE_OPENAI_RESOURCE", "my-resource");
        env.remove("AZURE_OPENAI_API_KEY");
        env.set("AZURE_OPENAI_MODEL", "deployment-a");
        let mut c = cfg(ApiType::Azure, "", Some("cfg-key"));
        c.azure_resource = None;
        let client = create_client(&c).expect("azure builds with the configured key");
        assert_eq!(client.model(), "test-model", "the configured model wins");
    }

    #[test]
    fn azure_empty_configured_resource_falls_back_to_env() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.set("AZURE_OPENAI_RESOURCE", "env-resource");
        env.set("AZURE_OPENAI_API_KEY", "env-key");
        env.set("AZURE_OPENAI_MODEL", "deployment-a");
        let mut c = cfg(ApiType::Azure, "", Some("k"));
        c.azure_resource = Some(String::new());
        let client = create_client(&c).expect("empty configured resource defers to the env");
        assert!(
            client
                .base_url()
                .starts_with("https://env-resource.openai.azure.com"),
            "the env resource must form the endpoint: {}",
            client.base_url()
        );
    }

    #[test]
    fn azure_base_url_override_replaces_the_derived_endpoint() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.remove("AZURE_OPENAI_RESOURCE");
        env.set("AZURE_OPENAI_API_KEY", "env-key");
        env.remove("AZURE_OPENAI_MODEL");
        let c = cfg(ApiType::Azure, "https://gateway.example/v1", Some("k"));
        let client = create_client(&c).expect("an explicit base_url needs no resource name");
        assert_eq!(client.base_url(), "https://gateway.example/v1");
    }

    #[test]
    fn azure_gateway_ignores_a_malformed_configured_resource() {
        // Behind a gateway the resource name is irrelevant: it must not be
        // validated into a hard failure the direct path would report.
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.remove("AZURE_OPENAI_RESOURCE");
        env.remove("AZURE_OPENAI_MODEL");
        let mut c = cfg(ApiType::Azure, "https://gateway.example/v1", Some("k"));
        c.azure_resource = Some("bad resource!".to_string());
        let client = create_client(&c).expect("a gateway config must not validate the resource");
        assert_eq!(client.base_url(), "https://gateway.example/v1");
    }

    #[test]
    fn azure_model_from_config_needs_no_env_model() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.set("AZURE_OPENAI_RESOURCE", "my-resource");
        env.set("AZURE_OPENAI_API_KEY", "env-key");
        env.remove("AZURE_OPENAI_MODEL");
        let c = cfg(ApiType::Azure, "", Some("k"));
        let client = create_client(&c).expect("the configured model must satisfy azure");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn azure_rejects_malformed_resource_name() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AZURE_OPENAI_RESOURCE",
            "AZURE_OPENAI_API_KEY",
            "AZURE_OPENAI_MODEL",
        ]);
        env.remove("AZURE_OPENAI_RESOURCE");
        env.remove("AZURE_OPENAI_MODEL");
        let mut c = cfg(ApiType::Azure, "", Some("k"));
        c.azure_resource = Some("bad resource!".to_string());
        let err = create_client(&c)
            .err()
            .expect("a malformed resource name must be rejected");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("resource name"),
            "error must name the malformed resource: {msg}"
        );
    }

    #[test]
    fn moonshot_config_api_key_builds_without_env_key() {
        let env = loopctl::testing::EnvGuard::acquire(&["MOONSHOT_API_KEY", "MOONSHOT_MODEL"]);
        env.remove("MOONSHOT_API_KEY");
        env.remove("MOONSHOT_MODEL");
        let c = cfg(ApiType::Moonshot, "", Some("cfg-key"));
        let client = create_client(&c).expect("moonshot builds with the configured key");
        let DchClient::OpenAi(_) = client else {
            panic!("moonshot rides the OpenAI-protocol variant");
        };
    }

    #[test]
    fn moonshot_without_a_model_uses_the_profile_default() {
        let env = loopctl::testing::EnvGuard::acquire(&["MOONSHOT_API_KEY", "MOONSHOT_MODEL"]);
        env.set("MOONSHOT_API_KEY", "env-key");
        env.remove("MOONSHOT_MODEL");
        let mut c = cfg(ApiType::Moonshot, "", None);
        c.model = String::new();
        let client = create_client(&c).expect("moonshot builds on the profile default");
        assert_eq!(client.model(), "kimi-k3");
        env.remove("MOONSHOT_API_KEY");
    }

    #[test]
    fn bedrock_builds_from_env_with_configured_model() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AWS_REGION",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ]);
        env.set("AWS_REGION", "us-east-1");
        env.set("AWS_ACCESS_KEY_ID", "test-key-id");
        env.set("AWS_SECRET_ACCESS_KEY", "test-secret");
        env.remove("AWS_SESSION_TOKEN");
        let c = cfg(ApiType::Bedrock, "", None);
        let client = create_client(&c).expect("bedrock builds from env");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn bedrock_rejects_api_key_config() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AWS_REGION",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ]);
        env.set("AWS_REGION", "us-east-1");
        env.set("AWS_ACCESS_KEY_ID", "test-key-id");
        env.set("AWS_SECRET_ACCESS_KEY", "test-secret");
        let c = cfg(ApiType::Bedrock, "", Some("bearer-key"));
        let err = create_client(&c).err().expect("api_key must be rejected");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("SigV4"),
            "error must explain the credential model: {msg}"
        );
    }

    #[test]
    fn bedrock_rejects_base_url_config() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AWS_REGION",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ]);
        env.set("AWS_REGION", "us-east-1");
        env.set("AWS_ACCESS_KEY_ID", "test-key-id");
        env.set("AWS_SECRET_ACCESS_KEY", "test-secret");
        let c = cfg(ApiType::Bedrock, "https://bedrock.example", None);
        let err = create_client(&c).err().expect("base_url must be rejected");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("base_url"),
            "error must name the rejected field: {msg}"
        );
    }

    #[test]
    fn bedrock_requires_a_model() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AWS_REGION",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ]);
        env.set("AWS_REGION", "us-east-1");
        env.set("AWS_ACCESS_KEY_ID", "test-key-id");
        env.set("AWS_SECRET_ACCESS_KEY", "test-secret");
        let mut c = cfg(ApiType::Bedrock, "", None);
        c.model = String::new();
        let err = create_client(&c)
            .err()
            .expect("empty model must be rejected");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("api.model"),
            "error must name the required field: {msg}"
        );
    }

    #[test]
    fn bedrock_missing_env_names_the_variables() {
        let env = loopctl::testing::EnvGuard::acquire(&[
            "AWS_REGION",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ]);
        env.remove("AWS_REGION");
        env.remove("AWS_ACCESS_KEY_ID");
        env.remove("AWS_SECRET_ACCESS_KEY");
        let c = cfg(ApiType::Bedrock, "", None);
        let err = create_client(&c).err().expect("missing AWS env must error");
        let RunnerError::Client(msg) = &err else {
            panic!("expected Client error, got {err:?}");
        };
        assert!(
            msg.contains("AWS_REGION"),
            "error must name the missing variable: {msg}"
        );
    }

    #[test]
    fn dchclient_forwards_extract_structured_to_the_inner_provider() {
        // extract_structured is synchronous (no network), so it can be exercised
        // offline. A dropped forward would panic on the match (unreachable) or
        // fail to compile; reaching the inner client's impl proves the arm
        // delegates. The exact Value depends on the inner impl; we only assert
        // the call returns without panicking across every variant.
        let message = loopctl::message::Message::user("hello");
        for api_type in [ApiType::OpenAi, ApiType::Anthropic, ApiType::Gemini] {
            let c = cfg(api_type, "https://example.invalid", Some("k"));
            let client = create_client(&c).expect("builds");
            let _value: serde_json::Value = client.extract_structured(&message);
        }
    }
}
