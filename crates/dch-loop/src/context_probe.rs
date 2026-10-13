//! Best-effort discovery of a server's per-request context window.
//!
//! A `--parallel N` llama.cpp server advertises its whole context
//! (`n_ctx`) while each request actually gets `n_ctx / N`; the probe
//! reads the server's `/props` document and returns the per-request
//! share. It is deliberately quiet about failure — a refused
//! connection or an unexpected shape means "no answer", and the
//! caller falls back to the configured or default window. Only
//! loopback endpoints are asked at all — the document is the local
//! server's own route — and the request carries no credential: the
//! session's key is for the provider's API, not for this probe.

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
    if !props_probe_reachable(root) {
        tracing::debug!(root, "endpoint is not loopback; skipping the /props probe");
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(format!("{root}/props")).send().await.ok()?;
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

/// Whether the `/props` probe may dial `root` at all.
///
/// The capability document is the local llama.cpp-style server's own
/// route; a cloud endpoint does not serve it, and probing there would
/// spend an extra request — and, worse, an authenticated one — on a
/// host that never asked. Only loopback roots qualify: `localhost`
/// or a loopback address literal — `127/8`, or `::1` in its
/// bracketed URL spelling. The gate admits addresses, never names: a
/// DNS label may begin with `127.` and resolve anywhere, so a
/// prefix match would open the probe to hosts the loopback promise
/// excludes.
fn props_probe_reachable(root: &str) -> bool {
    let after_scheme = root
        .strip_prefix("https://")
        .or_else(|| root.strip_prefix("http://"))
        .unwrap_or(root);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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

    /// Serve one JSON body on an ephemeral port, once, keeping the
    /// request bytes.
    ///
    /// The capturing twin of [`serve_once`]: the probe's outbound
    /// request — headers included — is the pin's subject, so the
    /// helper hands back what it read instead of discarding it.
    fn serve_once_capturing(body: String) -> (u16, std::sync::Arc<std::sync::Mutex<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
        let port = listener
            .local_addr()
            .expect("bound socket has an address")
            .port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = std::sync::Arc::clone(&captured);
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 4096];
                if let Ok(read) = std::io::Read::read(&mut stream, &mut buffer)
                    && let Ok(mut sink) = sink.lock()
                    && let Some(received) = buffer.get(..read)
                {
                    sink.push_str(&String::from_utf8_lossy(received));
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                drop(stream.write_all(head.as_bytes()));
                drop(stream.write_all(body.as_bytes()));
            }
        });
        (port, captured)
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
    async fn the_props_probe_never_sends_the_api_key() {
        // `/props` is the local-server route; the probe dials it with
        // no credential at all — an authenticated request to a route
        // the provider does not serve would leak the key to a host
        // that never asked for it.
        let (port, captured) = serve_once_capturing(r#"{"n_ctx": 4096}"#.to_string());
        let mut config = probe_config(ApiType::OpenAi);
        config.api_key = Some("sk-probe-secret".to_string());
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}/v1")).await,
            Some(4_096),
            "the unauthenticated probe still reads the document"
        );
        let request = captured.lock().expect("capture lock").clone();
        assert!(
            !request.to_ascii_lowercase().contains("authorization"),
            "the probe request carries no credential: {request}"
        );
        assert!(
            !request.contains("sk-probe-secret"),
            "the configured key never leaves the process: {request}"
        );
    }

    #[test]
    fn the_props_probe_gates_on_loopback_roots() {
        for root in [
            "http://localhost:8080",
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080",
            "http://127.42.0.9",
            "http://[::1]:9000",
        ] {
            assert!(
                props_probe_reachable(root),
                "a loopback root is probeable: {root}"
            );
        }
        for root in [
            "http://api.openai.com/v1",
            "https://example.com",
            "http://10.1.2.3:8080",
            "http://llama.internal.lan",
            // Lookalike names: a DNS label may begin with `127.` and
            // still resolve anywhere — the gate admits addresses, not
            // names, so none of these is ever probed.
            "http://127.evil.example.com",
            "http://127.0.1.1.evil.com",
        ] {
            assert!(
                !props_probe_reachable(root),
                "a non-loopback root is never probed: {root}"
            );
        }
    }

    #[tokio::test]
    async fn a_remote_root_returns_no_answer_without_dialing() {
        // The gate runs before any request is built: a remote root is
        // no answer without a connection attempt, deterministically —
        // no test may depend on a real network refusing it.
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(&config, "https://api.openai.com/v1").await,
            None,
            "a remote endpoint is never asked for /props"
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
