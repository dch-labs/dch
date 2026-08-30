//! Construction of the loopctl API client from dch configuration.

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

/// Sentinel API key used for providers that require no authentication.
///
/// A local Ollama server accepts any credential, so when no key is configured
/// this dummy is sent rather than erroring — the request succeeds and the
/// user is not forced to invent a placeholder key. Cloud-hosted deployments
/// with real authentication override it via `api_key` or `OLLAMA_API_KEY`.
const NO_AUTH_KEY: &str = "ollama";

/// The concrete provider client dch monomorphizes the agent loop over.
///
/// A runtime-selected enum over loopctl's three provider client families, so
/// the agent loop's per-turn LLM call is statically dispatched rather than
/// going through `dyn ApiClient`. [`create_client`] picks the variant from
/// [`ApiConfig::api_type`] (by wire-protocol family: OpenAI-compatible
/// providers map to [`OpenAi`], Anthropic-compatible to [`Anthropic`], Gemini
/// to [`Gemini`]); every other method on `DchClient` forwards to the inner
/// client unchanged.
///
/// [`OpenAi`]: Self::OpenAi
/// [`Anthropic`]: Self::Anthropic
/// [`Gemini`]: Self::Gemini
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
/// (`OpenAi`, `Ollama`, `DeepSeek`, `Grok`) wrap an [`OpenAiClient`];
/// Anthropic-compatible providers (`Anthropic`, `Zai`) wrap an
/// [`AnthropicClient`]; `Gemini` wraps a [`GeminiClient`]. An empty `base_url`
/// falls back to [`ApiType::default_base_url`].
///
/// # API-key resolution
///
/// `config.api_key` wins. When `None`, the factory falls back to the family's
/// conventional environment variable (`OPENAI_API_KEY` for the OpenAI family,
/// `ANTHROPIC_API_KEY` for the Anthropic family, `GEMINI_API_KEY` or
/// `GOOGLE_API_KEY` for Gemini). `Ollama` needs no key and is given a dummy.
/// If a required key is missing, returns [`RunnerError::Client`] naming the
/// expected environment variable.
///
/// # Errors
///
/// - [`RunnerError::Client`] if a required API key is missing or if the
///   underlying HTTP client cannot be constructed.
pub fn create_client(config: &ApiConfig) -> Result<DchClient, RunnerError> {
    let timeout = Duration::from_secs(config.request_timeout_secs);

    let client = match config.api_type {
        ApiType::OpenAi | ApiType::Ollama | ApiType::DeepSeek | ApiType::Grok => {
            let base_url = effective_base_url(config);
            let api_key = resolve_api_key(config)?;
            DchClient::OpenAi(
                OpenAiClient::builder()
                    .with_api_key(api_key)
                    .with_base_url(base_url)
                    .with_model(config.model.as_str())
                    .with_timeout(timeout)
                    .build()
                    .map_err(|e| RunnerError::Client(e.to_string()))?,
            )
        }
        ApiType::Anthropic | ApiType::Zai => {
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
                    .map_err(|e| RunnerError::Client(e.to_string()))?,
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
                    .map_err(|e| RunnerError::Client(e.to_string()))?,
            )
        }
        ApiType::Azure => DchClient::OpenAi(build_azure(config)?),
        ApiType::Moonshot => DchClient::OpenAi(build_moonshot(config)?),
        ApiType::Bedrock => DchClient::Bedrock(build_bedrock(config)?),
    };
    Ok(client)
}

/// Build the Azure OpenAI client for `config`.
///
/// The resource name comes from [`ApiConfig::azure_resource`] or the
/// `AZURE_OPENAI_RESOURCE` environment variable; the credential and deployment
/// model follow the provider profile (`AZURE_OPENAI_API_KEY` and
/// `AZURE_OPENAI_MODEL`, both required by the endpoint style). A non-empty
/// [`ApiConfig::model`] overrides the deployment model from configuration.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] when the resource name is unresolvable or
/// the provider profile rejects its environment.
fn build_azure(config: &ApiConfig) -> Result<OpenAiClient, RunnerError> {
    let resource = config.azure_resource.clone().or_else(|| {
        std::env::var("AZURE_OPENAI_RESOURCE")
            .ok()
            .filter(|v| !v.is_empty())
    });
    let resource = resource.ok_or_else(|| {
        RunnerError::Client(
            "azure: no resource name: set api.azure_resource or AZURE_OPENAI_RESOURCE".to_string(),
        )
    })?;
    let client =
        loopctl::provider::azure(&resource).map_err(|e| RunnerError::Client(e.to_string()))?;
    if !config.model.is_empty() && client.model() != config.model {
        client.set_model(&config.model);
    }
    Ok(client)
}

/// Build the Moonshot client for `config`.
///
/// Uses the provider profile (`MOONSHOT_API_KEY`, optional `MOONSHOT_MODEL`)
/// unless [`ApiConfig::base_url`] is set, in which case an OpenAI-protocol
/// client is built against that URL with the usual key resolution. A non-empty
/// [`ApiConfig::model`] overrides the profile's model.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] when the profile rejects its environment or
/// the HTTP client cannot be constructed.
fn build_moonshot(config: &ApiConfig) -> Result<OpenAiClient, RunnerError> {
    if !config.base_url.is_empty() {
        return OpenAiClient::builder()
            .with_api_key(resolve_api_key(config)?)
            .with_base_url(config.base_url.clone())
            .with_model(config.model.as_str())
            .with_timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| RunnerError::Client(e.to_string()));
    }
    let client = loopctl::provider::moonshot().map_err(|e| RunnerError::Client(e.to_string()))?;
    if !config.model.is_empty() && client.model() != config.model {
        client.set_model(&config.model);
    }
    Ok(client)
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
/// one; otherwise falls back to [`ApiType::default_base_url`] for the
/// configured provider. This is what lets a config omit `base_url` entirely
/// (the common case for stock OpenAI/Anthropic/Gemini) while still allowing an
/// override for self-hosted or proxy deployments.
fn effective_base_url(config: &ApiConfig) -> String {
    if config.base_url.is_empty() {
        config.api_type.default_base_url().to_owned()
    } else {
        config.base_url.clone()
    }
}

/// Resolve the API key for `config`.
///
/// Resolution is uniform across providers: `config.api_key` wins; otherwise
/// each provider's candidate environment variables are tried in order. A miss
/// yields a [`RunnerError::Client`] naming the variables that were tried.
///
/// Ollama is the one exception: a local Ollama server needs no authentication,
/// so when no key is configured it falls back to a fixed dummy rather than
/// erroring. A cloud-hosted Ollama with authentication works like any other
/// provider via `api_key` or `OLLAMA_API_KEY`.
///
/// # Errors
///
/// Returns [`RunnerError::Client`] when the key is neither configured nor
/// available in any of the provider's environment variables (except for
/// Ollama, which falls back to a dummy).
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
    if config.api_type == ApiType::Ollama {
        return Ok(NO_AUTH_KEY.to_owned());
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

/// Candidate API-key environment variables for `api_type`, in fallback order.
///
/// Consulted by [`resolve_api_key`] when [`ApiConfig::api_key`] is unset, so a
/// user can avoid putting the key in the config file by exporting it. Each
/// provider maps to the env var its official client reads; `OpenAI`-compatible
/// providers (`DeepSeek`, `Grok`) share `OPENAI_API_KEY` since they speak the
/// same protocol, and `Gemini` tries both `GEMINI_API_KEY` and the older
/// `GOOGLE_API_KEY`. [`ApiType::Ollama`] is included for uniformity even though
/// a local Ollama needs no key.
fn candidate_env_vars(api_type: ApiType) -> Vec<&'static str> {
    match api_type {
        ApiType::OpenAi | ApiType::DeepSeek | ApiType::Grok => vec!["OPENAI_API_KEY"],
        ApiType::Anthropic | ApiType::Zai => vec!["ANTHROPIC_API_KEY"],
        ApiType::Gemini => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        ApiType::Ollama => vec!["OLLAMA_API_KEY"],
        ApiType::Azure => vec!["AZURE_OPENAI_API_KEY"],
        ApiType::Moonshot => vec!["MOONSHOT_API_KEY"],
        ApiType::Bedrock => Vec::new(),
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
        let env = loopctl::testing::EnvGuard::acquire(&["OLLAMA_API_KEY"]);
        env.remove("OLLAMA_API_KEY");
        let c = cfg(ApiType::Ollama, "", None);
        let client = create_client(&c).expect("ollama builds via default base_url");
        assert_eq!(client.model(), "test-model");
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
    fn deepseek_key_via_openai_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["OPENAI_API_KEY"]);
        env.set("OPENAI_API_KEY", "env-key");
        let c = cfg(ApiType::DeepSeek, "https://api.deepseek.com", None);
        let client = create_client(&c).expect("deepseek builds with OPENAI_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("OPENAI_API_KEY");
    }

    #[test]
    fn grok_key_via_openai_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["OPENAI_API_KEY"]);
        env.set("OPENAI_API_KEY", "env-key");
        let c = cfg(ApiType::Grok, "https://api.x.ai/v1", None);
        let client = create_client(&c).expect("grok builds with OPENAI_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("OPENAI_API_KEY");
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
    fn zai_key_via_anthropic_env() {
        let env = loopctl::testing::EnvGuard::acquire(&["ANTHROPIC_API_KEY"]);
        env.set("ANTHROPIC_API_KEY", "env-key");
        let c = cfg(ApiType::Zai, "https://api.z.ai/api", None);
        let client = create_client(&c).expect("zai builds with ANTHROPIC_API_KEY");
        assert_eq!(client.model(), "test-model");
        env.remove("ANTHROPIC_API_KEY");
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
        let mut c = ApiConfig::default();
        c.model = "default-model".to_string();
        let client = create_client(&c).expect("default ApiConfig should build");
        assert_eq!(client.model(), "default-model");
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
