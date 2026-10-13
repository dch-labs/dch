//! Best-effort discovery of a server's per-request context window.
//!
//! A `--parallel N` llama.cpp server advertises its whole context
//! (`n_ctx`) while each request actually gets `n_ctx / N`; the probe
//! reads the server's `/props` document and returns the per-request
//! share. It is deliberately quiet about failure — a cloud endpoint
//! with no such route, a refused connection, or an unexpected shape
//! all mean "no answer", and the caller falls back to the configured
//! or default window.

use std::time::Duration;

use dch_config::ApiConfig;
use dch_config::ApiType;
use loopctl::api::ApiClient;

/// How long the probe waits for the capability document.
///
/// Discovery rides the runner's construction; a server slow to answer
/// `/props` must not hold a session hostage, so the wait stays short
/// and a timeout reads as no answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Ask the server this session's client resolved for the window one
/// request may carry.
///
/// The endpoint comes from `client` rather than from the configuration:
/// the profiled providers take theirs from a provider profile or an
/// environment variable the config never carries, so only the built
/// client knows where the session actually goes — and the probe has to
/// ask that same server, the one the conversation talks to. Returns
/// `None` whenever discovery has no answer, leaving the caller on its
/// configured window.
pub(crate) async fn discover_window(config: &ApiConfig, client: &dyn ApiClient) -> Option<u64> {
    probe_context_window(config, &client.base_url()).await
}

/// Ask `config`'s server for the window one request may carry.
///
/// Only the OpenAI-compatible families route here — the `/props`
/// document is the local-server shape — and `base_url` is the
/// effective endpoint root, whose trailing `/v1` is stripped to reach
/// the server itself. Returns `None` on any failure.
pub(crate) async fn probe_context_window(config: &ApiConfig, base_url: &str) -> Option<u64> {
    if !matches!(config.api_type, ApiType::OpenAi | ApiType::Ollama) {
        return None;
    }
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;
    let mut request = client.get(format!("{root}/props"));
    if let Some(key) = &config.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        tracing::debug!(status = %response.status(), "no capability document; keeping the configured window");
        return None;
    }
    let props: serde_json::Value = response.json().await.ok()?;
    let advertised = props.get("n_ctx")?.as_u64()?;
    if advertised == 0 {
        return None;
    }
    let slots = props
        .get("total_slots")
        .and_then(serde_json::Value::as_u64)
        .filter(|slots| *slots > 0)
        .unwrap_or(1);
    let window = advertised.checked_div(slots)?;
    (window > 0).then_some(window)
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
    use loopctl::api::NonStreamingResponse;
    use loopctl::api::StreamRequest;
    use loopctl::api::error::ApiError;
    use loopctl::stream::StreamEvent;
    use std::future::Future;
    use std::io::Write as _;
    use std::pin::Pin;

    /// Serve one JSON body on an ephemeral port, once.
    ///
    /// Answers a single connection with `body` and exits — the whole
    /// conversation the probe needs.
    fn serve_once(body: String) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
        let port = listener
            .local_addr()
            .expect("bound socket has an address")
            .port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                drop(std::io::Read::read(&mut stream, &mut buffer));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                drop(stream.write_all(head.as_bytes()));
                drop(stream.write_all(body.as_bytes()));
            }
        });
        port
    }

    fn probe_config(api_type: ApiType) -> ApiConfig {
        ApiConfig {
            api_type,
            ..ApiConfig::default()
        }
    }

    /// A client whose only interesting answer is the endpoint it resolved.
    ///
    /// [`discover_window`] asks the built client where the session actually
    /// goes, because a profiled provider takes its endpoint from a profile or
    /// an environment variable the configuration never carries. This stub
    /// exists to carry exactly that address; neither request path is ever
    /// reached, and both answer as nothing-to-serve so a mistaken call fails
    /// an assertion instead of ending the run.
    struct EndpointOnlyClient {
        /// The endpoint the session's client settled on, profile or
        /// environment included — the one value the probe must read.
        url: String,
    }

    impl ApiClient for EndpointOnlyClient {
        fn model(&self) -> String {
            "endpoint-only".to_string()
        }

        fn base_url(&self) -> String {
            self.url.clone()
        }

        fn stream_messages(
            &self,
            _request: &StreamRequest,
        ) -> Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, ApiError>> + Send + 'static>>
        {
            Box::pin(futures::stream::empty())
        }

        fn create_message(
            &self,
            _request: &StreamRequest,
        ) -> Pin<Box<dyn Future<Output = Result<NonStreamingResponse, ApiError>> + Send + '_>>
        {
            Box::pin(async { Err(ApiError::api("this client only carries an endpoint")) })
        }
    }

    #[tokio::test]
    async fn the_probe_divides_the_advertised_total_by_the_slots() {
        let port = serve_once(r#"{"n_ctx": 262144, "total_slots": 4}"#.to_string());
        let config = probe_config(ApiType::Ollama);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}/v1")).await,
            Some(65_536),
            "the per-request share is the advertised total over the slots"
        );
    }

    #[tokio::test]
    async fn a_missing_slot_count_reads_as_one_slot() {
        let port = serve_once(r#"{"n_ctx": 8192}"#.to_string());
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}")).await,
            Some(8_192)
        );
    }

    #[tokio::test]
    async fn a_failed_probe_yields_nothing() {
        let config = probe_config(ApiType::Ollama);
        assert_eq!(
            probe_context_window(&config, "http://127.0.0.1:1/v1").await,
            None,
            "a refused connection is no answer"
        );
        let port = serve_once(r#"{"unrelated": true}"#.to_string());
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}")).await,
            None,
            "a document without the fields is no answer"
        );
    }

    #[tokio::test]
    async fn a_non_openai_compatible_family_never_probes() {
        let config = probe_config(ApiType::Anthropic);
        assert_eq!(
            probe_context_window(&config, "http://127.0.0.1:1/v1").await,
            None,
            "families without the route skip discovery entirely"
        );
    }

    #[tokio::test]
    async fn discovery_asks_the_endpoint_the_client_resolved() {
        // The stock local install keeps its endpoint out of the
        // configuration — a probe that read `config.base_url` here would
        // have nothing to ask and would report no answer for a server that
        // is plainly there.
        let port = serve_once(r#"{"n_ctx": 16384}"#.to_string());
        let config = probe_config(ApiType::Ollama);
        assert!(
            config.base_url.is_empty(),
            "the stock shape carries no configured endpoint"
        );

        let client = EndpointOnlyClient {
            url: format!("http://127.0.0.1:{port}/v1"),
        };
        assert_eq!(
            discover_window(&config, &client).await,
            Some(16_384),
            "discovery follows the client's endpoint, not the empty config field"
        );
    }
}
