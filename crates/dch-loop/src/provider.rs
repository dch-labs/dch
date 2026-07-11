//! Construction of the loopctl API client from dch configuration.

use std::sync::Arc;
use std::time::Duration;

use dch_config::ApiConfig;
use dch_config::ApiType;
use loopctl::api::SharedApiClient;
use loopctl::provider::AnthropicClient;
use loopctl::provider::GeminiClient;
use loopctl::provider::OpenAiClient;

use crate::error::RunnerError;

/// Sentinel API key used for providers that require no authentication.
///
/// `loopctl`'s OpenAI-compatible client builder rejects an empty key, so the
/// local Ollama provider (no auth) is given this fixed value, matching
/// loopctl's own Ollama constructor.
const NO_AUTH_KEY: &str = "ollama";

/// Build a [`loopctl::api::SharedApiClient`] for the provider named by
/// `config.api_type`.
///
/// Variants are mapped by wire-protocol family: OpenAI-compatible providers
/// (`OpenAi`, `Ollama`, `DeepSeek`, `Grok`) use an [`OpenAiClient`];
/// Anthropic-compatible providers (`Anthropic`, `Zai`) use an
/// [`AnthropicClient`]; `Gemini` uses a [`GeminiClient`]. An empty `base_url`
/// falls back to [`ApiType::default_base_url`](dch_config::ApiType::default_base_url).
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
pub fn create_client(config: &ApiConfig) -> Result<SharedApiClient, RunnerError> {
    let base_url = effective_base_url(config);
    let api_key = resolve_api_key(config)?;
    let timeout = Duration::from_secs(config.request_timeout_secs);

    let client: SharedApiClient = match config.api_type {
        ApiType::OpenAi | ApiType::Ollama | ApiType::DeepSeek | ApiType::Grok => Arc::new(
            OpenAiClient::builder()
                .api_key(api_key)
                .base_url(base_url)
                .model(config.model.as_str())
                .timeout(timeout)
                .build()
                .map_err(|e| RunnerError::Client(e.to_string()))?,
        ),
        ApiType::Anthropic | ApiType::Zai => Arc::new(
            AnthropicClient::builder()
                .api_key(api_key)
                .base_url(base_url)
                .model(config.model.as_str())
                .max_tokens(config.max_tokens)
                .timeout(timeout)
                .build()
                .map_err(|e| RunnerError::Client(e.to_string()))?,
        ),
        ApiType::Gemini => Arc::new(
            GeminiClient::builder()
                .api_key(api_key)
                .base_url(base_url)
                .model(config.model.as_str())
                .timeout(timeout)
                .build()
                .map_err(|e| RunnerError::Client(e.to_string()))?,
        ),
    };
    Ok(client)
}

/// Resolve the base URL, falling back to the provider default when empty.
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

/// Candidate API-key environment variables for each provider, in fallback order.
fn candidate_env_vars(api_type: ApiType) -> Vec<&'static str> {
    match api_type {
        ApiType::OpenAi | ApiType::DeepSeek | ApiType::Grok => vec!["OPENAI_API_KEY"],
        ApiType::Anthropic | ApiType::Zai => vec!["ANTHROPIC_API_KEY"],
        ApiType::Gemini => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        ApiType::Ollama => vec!["OLLAMA_API_KEY"],
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
    use std::sync::Mutex;

    use dch_config::ApiConfig;
    use dch_config::ApiType;

    use super::*;
    use crate::RunnerError;

    static ENV_GUARD: Mutex<()> = Mutex::new(());

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

    fn set_env(var: &str, val: &str) {
        // SAFETY: every env-touching test acquires `ENV_GUARD` before calling
        // this, so no other test mutates the process environment concurrently.
        unsafe {
            std::env::set_var(var, val);
        }
    }

    fn remove_env(var: &str) {
        // SAFETY: see [`set_env`].
        unsafe {
            std::env::remove_var(var);
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
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_env("OLLAMA_API_KEY");
        let c = cfg(ApiType::Ollama, "http://localhost:11434/v1", None);
        let client = create_client(&c).expect("ollama builds with no key");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn ollama_empty_base_url_uses_default() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_env("OLLAMA_API_KEY");
        let c = cfg(ApiType::Ollama, "", None);
        let client = create_client(&c).expect("ollama builds via default base_url");
        assert_eq!(client.model(), "test-model");
    }

    #[test]
    fn ollama_cloud_key_from_env() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_env("OLLAMA_API_KEY", "env-key");
        let c = cfg(ApiType::Ollama, "https://cloud.example.com/v1", None);
        let client = create_client(&c).expect("cloud ollama builds with OLLAMA_API_KEY");
        assert_eq!(client.model(), "test-model");
        remove_env("OLLAMA_API_KEY");
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
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_env("OPENAI_API_KEY", "env-key");
        let c = cfg(ApiType::OpenAi, "https://api.openai.com/v1", None);
        let client = create_client(&c).expect("openai builds with env key");
        assert_eq!(client.model(), "test-model");
        remove_env("OPENAI_API_KEY");
    }

    #[test]
    fn deepseek_key_via_openai_env() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_env("OPENAI_API_KEY", "env-key");
        let c = cfg(ApiType::DeepSeek, "https://api.deepseek.com", None);
        let client = create_client(&c).expect("deepseek builds with OPENAI_API_KEY");
        assert_eq!(client.model(), "test-model");
        remove_env("OPENAI_API_KEY");
    }

    #[test]
    fn grok_key_via_openai_env() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_env("OPENAI_API_KEY", "env-key");
        let c = cfg(ApiType::Grok, "https://api.x.ai/v1", None);
        let client = create_client(&c).expect("grok builds with OPENAI_API_KEY");
        assert_eq!(client.model(), "test-model");
        remove_env("OPENAI_API_KEY");
    }

    #[test]
    fn anthropic_key_from_env() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_env("ANTHROPIC_API_KEY", "env-key");
        let c = cfg(ApiType::Anthropic, "https://api.anthropic.com", None);
        let client = create_client(&c).expect("anthropic builds with env key");
        assert_eq!(client.model(), "test-model");
        remove_env("ANTHROPIC_API_KEY");
    }

    #[test]
    fn missing_key_clear_error() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_env("OPENAI_API_KEY");
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
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_env("ANTHROPIC_API_KEY", "env-key");
        let c = cfg(ApiType::Zai, "https://api.z.ai/api", None);
        let client = create_client(&c).expect("zai builds with ANTHROPIC_API_KEY");
        assert_eq!(client.model(), "test-model");
        remove_env("ANTHROPIC_API_KEY");
    }

    #[test]
    fn gemini_key_from_gemini_env() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_env("GOOGLE_API_KEY");
        set_env("GEMINI_API_KEY", "env-key");
        let c = cfg(
            ApiType::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            None,
        );
        let client = create_client(&c).expect("gemini builds with GEMINI_API_KEY");
        assert_eq!(client.model(), "test-model");
        remove_env("GEMINI_API_KEY");
    }

    #[test]
    fn gemini_key_falls_back_to_google_env() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_env("GEMINI_API_KEY");
        set_env("GOOGLE_API_KEY", "env-key");
        let c = cfg(
            ApiType::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            None,
        );
        let client = create_client(&c).expect("gemini builds with GOOGLE_API_KEY");
        assert_eq!(client.model(), "test-model");
        remove_env("GOOGLE_API_KEY");
    }

    #[test]
    fn gemini_missing_key_names_both_vars() {
        let _g = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_env("GEMINI_API_KEY");
        remove_env("GOOGLE_API_KEY");
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
}
