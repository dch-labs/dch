//! Best-effort discovery of a server's per-request context window.
//!
//! Two questions are asked, in order, and the first answer wins. The
//! llama.cpp family's `/props` document states the server's whole
//! budget (`n_ctx`) while each request actually gets the share over
//! `total_slots` — a `--parallel N` server advertises the whole and
//! serves the slice — so that document comes first. The servers
//! without one publish each model's window on the OpenAI-compatible
//! model listing instead: vLLM and its kin spell it `max_model_len`,
//! llama.cpp-style servers repeat it under `meta.n_ctx`, and the
//! session's model is matched by its listing id. The probe is
//! deliberately quiet about failure — a refused connection or an
//! unexpected shape means "no answer", and the caller falls back to
//! the configured window, the table of documented cloud-model
//! windows, or the default. Only loopback endpoints are
//! asked at all — both routes are the local server's own — and the
//! requests carry no credential: the session's key is for the
//! provider's API, not for this probe.

use std::time::Duration;

use dch_config::ApiConfig;
use dch_config::ApiType;
use loopctl::api::ApiClient;

/// How long each discovery question waits for its answer.
///
/// Discovery rides the runner's construction; a server slow to
/// answer must not hold a session hostage, so the wait stays short
/// and a timeout reads as no answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The share of a discovered window the usage-less family may
/// spend, in hundredths of it.
///
/// The discovered window is the server's own tokenization; the
/// session's token estimates are the heuristic counter's, which
/// under-counts real tokens by roughly a quarter (the calibration
/// that turned a true 262,144-token window into a configured
/// 210,000). Where usage reports come back the counter is trued up
/// against the real count, so the drift self-corrects and the
/// discovered window stands. Ollama's client never parses usage, so
/// nothing anchors its counter and the drift runs one-way; its
/// discovered window is held back by this margin so the compaction
/// trigger sits under the window's honest edge instead of past it.
/// An explicitly configured `context_window` is the user's own
/// calibration and is never scaled, whatever the family.
const DISCOVERED_WINDOW_DRIFT_PERCENT: u64 = 80;

/// Scale a discovered window by the drift margin, in whole tokens.
///
/// Pure so the scaling is pinnable without a server; hundredths
/// arithmetic keeps it exact for every u64 window a server can name.
pub(crate) fn scale_for_drift(window: u64) -> u64 {
    (window / 100)
        .saturating_mul(DISCOVERED_WINDOW_DRIFT_PERCENT)
        .saturating_add((window % 100).saturating_mul(DISCOVERED_WINDOW_DRIFT_PERCENT) / 100)
}

/// Ask the server this session's client resolved for the window one
/// request may carry, and the table where the server has none.
///
/// The endpoint comes from `client` rather than from the configuration:
/// the profiled providers take theirs from a provider profile or an
/// environment variable the config never carries, so only the built
/// client knows where the session actually goes — and the probe has to
/// ask that same server, the one the conversation talks to. The model
/// name rides along the same way: the listing route keys its entries
/// by model id, and the wrong entry's window would mis-size the
/// session. When the server has no answer, the built-in table of
/// documented cloud-model windows is consulted next — the cloud
/// providers publish nothing on the wire, so their own pages are the
/// only source left. Returns `None` when neither has an answer,
/// leaving the caller on its configured window. What the answer may
/// honestly stand at differs by family, because what stands behind
/// the compaction counter differs: a family whose turns carry usage
/// anchors that counter to the server's own count each turn, so its
/// window is honest as it stands and rides whole — the self-hosted
/// family asks its server for usage, and a server that omits it
/// anyway keeps a named exposure rather than a silent one. Ollama's
/// client parses no usage at all, so its counter stays a local
/// estimate that runs light of the real count, and comparing that
/// estimate against the raw window spends the drift as headroom it
/// does not have: an Ollama probe answer is held back by the margin
/// [`scale_for_drift`] applies. The table's fallback rides whole for
/// every family — it documents cloud models whose providers report
/// usage in turn.
pub(crate) async fn discover_window(config: &ApiConfig, client: &dyn ApiClient) -> Option<u64> {
    let model = client.model();
    let probed = probe_context_window(config, &client.base_url(), &model).await;
    match (probed, config.api_type) {
        (Some(window), ApiType::Ollama) => Some(scale_for_drift(window)),
        (Some(window), _) => Some(window),
        (None, _) => crate::known_windows::known_model_window(&model),
    }
}

/// Ask `config`'s server for the window one request may carry.
///
/// Only the OpenAI-compatible families route here — both discovery
/// questions are the local-server shape — and `base_url` is the
/// effective endpoint root, whose trailing `/v1` is stripped to reach
/// the server itself. The capability document is asked first and the
/// model listing second; the first answer wins. Returns `None` when
/// neither question has one.
pub(crate) async fn probe_context_window(
    config: &ApiConfig,
    base_url: &str,
    model: &str,
) -> Option<u64> {
    if !matches!(
        config.api_type,
        ApiType::OpenAi | ApiType::Ollama | ApiType::SelfHosted
    ) {
        return None;
    }
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    if let Some(window) = props_window(root).await {
        return Some(window);
    }
    listing_window(trimmed, model).await
}

/// Read the llama.cpp family's `/props` document for the per-request
/// share of the server's context.
///
/// `n_ctx` is the server's whole budget; under `--parallel` it is
/// split across `total_slots` slots, and the share is what any single
/// request may carry. A non-loopback root, a refused connection, or a
/// document without the fields is no answer.
async fn props_window(root: &str) -> Option<u64> {
    if !loopback_probe_reachable(root) {
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

/// Read the OpenAI-compatible model listing for the session model's
/// advertised window.
///
/// The servers without a capability document still publish each
/// model's window on the listing route: vLLM and its kin spell it
/// `max_model_len`, llama.cpp-style servers repeat it under
/// `meta.n_ctx`, and the first spelling present wins. The listing
/// keys its entries by model id, so the session's own entry is
/// matched by name — a listing that names one model only is that
/// server's answer whatever it calls it. Like every probe here the
/// request carries no credential, and any failure is no answer.
///
/// The listing's number is taken as already per-request, where the
/// capability document's `n_ctx` is the server's whole budget and is
/// divided by its `total_slots` — an asymmetry the two routes' shapes
/// force: the listing publishes no slot count, so there is nothing to
/// divide by and no way to tell a `--parallel` llama.cpp serving the
/// whole budget under `meta.n_ctx` from one serving its per-request
/// share. A slots-sharing server that answers only the listing can
/// therefore over-report the window by the slot factor — the compaction
/// threshold sits later than the honest budget, and nothing shaves the
/// answer back. The per-request assumption is the right default (vLLM
/// and Ollama mean it exactly that way); the `--parallel` listing-only
/// shape is the one configuration it can misread, and naming that here
/// is the honest record of the bound.
async fn listing_window(base: &str, model: &str) -> Option<u64> {
    let base = base.trim_end_matches('/');
    let listing = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    if !loopback_probe_reachable(&listing) {
        tracing::debug!(
            listing,
            "endpoint is not loopback; skipping the model-listing probe"
        );
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(&listing).send().await.ok()?;
    if !response.status().is_success() {
        tracing::debug!(status = %response.status(), "no model listing; keeping the configured window");
        return None;
    }
    let document: serde_json::Value = response.json().await.ok()?;
    let entries = document.get("data")?.as_array()?;
    let (sole, siblings) = entries.split_first()?;
    let entry = entries
        .iter()
        .find(|entry| {
            entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id == model)
        })
        .or_else(|| siblings.is_empty().then_some(sole))?;
    let advertised = entry
        .get("max_model_len")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            entry
                .pointer("/meta/n_ctx")
                .and_then(serde_json::Value::as_u64)
        })?;
    (advertised > 0).then_some(advertised)
}

/// Whether a discovery probe may dial `root` at all.
///
/// Both discovery routes — the capability document and the model
/// listing — are the local server's own; a cloud endpoint serves
/// neither, and probing there would spend an extra request on a host
/// that never asked. The gate parses
/// `root` with the same URL parser the dialing client uses, so it
/// judges the host the request would actually reach: in an authority
/// like `localhost:@remote.example`, `remote.example` is the host and
/// the loopback-looking prefix is mere userinfo. A parsed host of
/// `localhost` or a loopback address — `127/8`, or `::1` in its
/// bracketed URL spelling — qualifies; a DNS label may begin with
/// `127.` and resolve anywhere, so names are never probeable on the
/// strength of spelling. A root that will not parse has no host to
/// vouch for it, and is never dialed.
fn loopback_probe_reachable(root: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(root) else {
        return false;
    };
    let Some(bracketed) = url.host_str() else {
        return false;
    };
    let host = bracketed
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(bracketed);
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

    #[test]
    fn a_discovered_window_is_scaled_by_the_drift_margin() {
        // The discovered number is the server's tokenization; the
        // session's estimates are the heuristic counter's, which runs
        // about a quarter light. A real session sat at "83%" of
        // a discovered 262,144 while its true prompt was past the
        // ceiling and the model died mid-thought — the drift margin,
        // not headroom. Scaling keeps the percentage honest; the
        // calibration constant matches the hand-tuned 262,144 to
        // 210,000 the drift was first measured by.
        assert_eq!(scale_for_drift(262_144), 209_715);
        assert_eq!(scale_for_drift(0), 0, "a window of nothing stays nothing");
        assert_eq!(
            scale_for_drift(u64::MAX),
            u64::MAX / 100 * 80 + u64::MAX % 100 * 80 / 100,
            "the scaling stays exact rather than overflowing"
        );
    }

    #[tokio::test]
    async fn ollama_holds_the_drift_margin_and_the_usage_reporting_families_ride_whole() {
        // The margin belongs to the family whose counter nothing
        // anchors: the ollama client disables usage parsing by
        // construction, so its estimate runs light of the real count
        // and the raw window would spend that drift as headroom. The
        // families whose turns carry usage — the self-hosted client
        // asks its server for it — anchor the counter to the server's
        // own number each turn, and a margin there would
        // double-discount: the 1M-window model whose bar read 800k.
        let (port, _) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"endpoint-only","max_model_len":16384}]}"#
                .to_string(),
        ]);
        let ollama = probe_config(ApiType::Ollama);
        let client = EndpointOnlyClient {
            url: format!("http://127.0.0.1:{port}/v1"),
            model: "endpoint-only".to_string(),
        };
        assert_eq!(
            discover_window(&ollama, &client).await,
            Some(scale_for_drift(16_384)),
            "ollama holds the drift margin: no usage ever anchors its counter"
        );
        let port = serve_once(r#"{"n_ctx": 16384}"#.to_string());
        let selfhosted = probe_config(ApiType::SelfHosted);
        let client = EndpointOnlyClient {
            url: format!("http://127.0.0.1:{port}/v1"),
            model: "endpoint-only".to_string(),
        };
        assert_eq!(
            discover_window(&selfhosted, &client).await,
            Some(16_384),
            "the self-hosted client asks its server for usage — its \
             window is honest as it stands, and a margin would \
             double-discount it"
        );
        // A third server for the reporting family's question.
        let port = serve_once(r#"{"n_ctx": 16384}"#.to_string());
        let openai = probe_config(ApiType::OpenAi);
        let client = EndpointOnlyClient {
            url: format!("http://127.0.0.1:{port}/v1"),
            model: "endpoint-only".to_string(),
        };
        assert_eq!(
            discover_window(&openai, &client).await,
            Some(16_384),
            "the reporting family rides its reported usage unscaled"
        );
    }

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

    /// Serve each body on its own connection, in order, recording
    /// every request line, then stop.
    ///
    /// The multi-question twin of [`serve_once`]: a probe that asks a
    /// server two routes — a miss followed by a hit — needs a listener
    /// that answers more than once, and the recorded request lines pin
    /// which route each answer served.
    fn serve_bodies(bodies: Vec<String>) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
        let port = listener
            .local_addr()
            .expect("bound socket has an address")
            .port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for body in bodies {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buffer = [0u8; 4096];
                    if let Ok(read) = std::io::Read::read(&mut stream, &mut buffer)
                        && let Ok(mut sink) = sink.lock()
                        && let Some(received) = buffer.get(..read)
                    {
                        let request = String::from_utf8_lossy(received);
                        if let Some(line) = request.lines().next() {
                            sink.push(line.to_string());
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    drop(stream.write_all(head.as_bytes()));
                    drop(stream.write_all(body.as_bytes()));
                }
            }
        });
        (port, seen)
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

        /// The model the session's client settled on — the listing
        /// route and the documented-model table both key by it.
        model: String,
    }

    impl ApiClient for EndpointOnlyClient {
        fn model(&self) -> String {
            self.model.clone()
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
            probe_context_window(
                &config,
                &format!("http://127.0.0.1:{port}/v1"),
                "test-model"
            )
            .await,
            Some(65_536),
            "the per-request share is the advertised total over the slots"
        );
    }

    #[tokio::test]
    async fn a_missing_slot_count_reads_as_one_slot() {
        let port = serve_once(r#"{"n_ctx": 8192}"#.to_string());
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}"), "test-model").await,
            Some(8_192)
        );
    }

    #[tokio::test]
    async fn a_failed_probe_yields_nothing() {
        let config = probe_config(ApiType::Ollama);
        assert_eq!(
            probe_context_window(&config, "http://127.0.0.1:1/v1", "test-model").await,
            None,
            "a refused connection is no answer"
        );
        let port = serve_once(r#"{"unrelated": true}"#.to_string());
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}"), "test-model").await,
            None,
            "a document without the fields is no answer"
        );
    }

    #[tokio::test]
    async fn a_non_openai_compatible_family_never_probes() {
        let config = probe_config(ApiType::Anthropic);
        assert_eq!(
            probe_context_window(&config, "http://127.0.0.1:1/v1", "test-model").await,
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
            probe_context_window(
                &config,
                &format!("http://127.0.0.1:{port}/v1"),
                "test-model"
            )
            .await,
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
    fn discovery_gates_on_loopback_roots() {
        for root in [
            "http://localhost:8080",
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080",
            "http://127.42.0.9",
            "http://[::1]:9000",
            // The URL parser case-folds domain hosts, so the gate
            // judges the same host the dialing client would reach.
            "http://LOCALHOST:8080",
        ] {
            assert!(
                loopback_probe_reachable(root),
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
            // Userinfo disguises: the parser puts `remote.example`
            // behind the `@`, and it is the host the client would
            // dial — a loopback-looking userinfo must not vouch for
            // a remote host.
            "http://localhost:@remote.example",
            "http://[::1]@remote.example",
        ] {
            assert!(
                !loopback_probe_reachable(root),
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
            probe_context_window(&config, "https://api.openai.com/v1", "test-model").await,
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
            model: "endpoint-only".to_string(),
        };
        assert_eq!(
            discover_window(&config, &client).await,
            Some(scale_for_drift(16_384)),
            "discovery follows the client's endpoint, not the empty config \
             field — and the family's answer carries its drift margin, \
             like every window it discovers"
        );
    }

    #[tokio::test]
    async fn the_listing_rides_its_number_whole_where_props_divides() {
        // The asymmetry the two routes' shapes force: the capability
        // document carries the server's whole budget and its slot
        // count, so the share is budget over slots; the listing
        // publishes no slot count, so its number is taken as already
        // per-request — the honest default for vLLM and Ollama, and
        // the one shape it can misread (a `--parallel` llama.cpp
        // answering only the listing) is named in the route's doc.
        // This pin holds the intended half: a listing that repeats a
        // slots-shared `n_ctx` under `meta.n_ctx` rides whole — no
        // phantom division, no silent scaling.
        let (port, _) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"m","meta":{"n_ctx":65536}}]}"#.to_string(),
        ]);
        let config = probe_config(ApiType::Ollama);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}/v1"), "m").await,
            Some(65_536),
            "the listing's number is per-request already — nothing \
             divides it, whatever the capability route would have done"
        );
    }

    #[tokio::test]
    async fn the_model_listing_answers_when_props_does_not() {
        // The vLLM-shaped server has no /props document at all; its
        // model listing is the only place the window is published.
        let (port, _) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"test-model","max_model_len":262144,"meta":{"n_ctx":262144}}]}"#
                .to_string(),
        ]);
        let config = probe_config(ApiType::Ollama);
        assert_eq!(
            probe_context_window(
                &config,
                &format!("http://127.0.0.1:{port}/v1"),
                "test-model"
            )
            .await,
            Some(262_144),
            "the listing answers where the capability document does not"
        );
    }

    #[tokio::test]
    async fn the_listing_prefers_max_model_len_and_falls_back_to_meta_n_ctx() {
        let (port, _) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"m","max_model_len":131072,"meta":{"n_ctx":262144}}]}"#
                .to_string(),
        ]);
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}/v1"), "m").await,
            Some(131_072),
            "max_model_len is the listing's own spelling of the window"
        );

        let (port, _) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"m","meta":{"n_ctx":4096}}]}"#.to_string(),
        ]);
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}/v1"), "m").await,
            Some(4_096),
            "a llama.cpp-style meta.n_ctx still answers"
        );
    }

    #[tokio::test]
    async fn the_listing_matches_the_session_model_by_id() {
        let listing = r#"{"object":"list","data":[
            {"id":"other-model","max_model_len":111},
            {"id":"test-model","max_model_len":222}
        ]}"#
        .to_string();
        let (port, _) = serve_bodies(vec![r#"{"unrelated": true}"#.to_string(), listing]);
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(
                &config,
                &format!("http://127.0.0.1:{port}/v1"),
                "test-model"
            )
            .await,
            Some(222),
            "the session's own entry answers, not the first listed"
        );

        let listing = r#"{"object":"list","data":[
            {"id":"other-model","max_model_len":111},
            {"id":"another-model","max_model_len":222}
        ]}"#
        .to_string();
        let (port, _) = serve_bodies(vec![r#"{"unrelated": true}"#.to_string(), listing]);
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(
                &config,
                &format!("http://127.0.0.1:{port}/v1"),
                "test-model"
            )
            .await,
            None,
            "an unlisted model with siblings to choose from is no answer"
        );
    }

    #[tokio::test]
    async fn a_sole_listing_entry_answers_for_any_model_name() {
        let (port, _) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"whatever","max_model_len":8192}]}"#.to_string(),
        ]);
        let config = probe_config(ApiType::Ollama);
        assert_eq!(
            probe_context_window(
                &config,
                &format!("http://127.0.0.1:{port}/v1"),
                "test-model"
            )
            .await,
            Some(8_192),
            "a one-model server is serving that model under any name"
        );
    }

    #[tokio::test]
    async fn the_listing_is_asked_at_the_v1_route_when_the_base_has_none() {
        let (port, seen) = serve_bodies(vec![
            r#"{"unrelated": true}"#.to_string(),
            r#"{"object":"list","data":[{"id":"test-model","max_model_len":16384}]}"#.to_string(),
        ]);
        let config = probe_config(ApiType::OpenAi);
        assert_eq!(
            probe_context_window(&config, &format!("http://127.0.0.1:{port}"), "test-model").await,
            Some(16_384),
            "the listing route is derived from the base, /v1 and all"
        );
        let requests = seen.lock().expect("capture lock").clone();
        assert!(
            requests.iter().any(|line| line.contains("/v1/models")),
            "the listing is asked at the versioned route: {requests:?}"
        );
    }

    #[tokio::test]
    async fn a_known_cloud_model_answers_when_no_server_does() {
        // The cloud providers publish no window on the wire; the
        // documented-model table answers for them where the probe
        // cannot — even for a family the live questions never serve.
        let config = probe_config(ApiType::Anthropic);
        let client = EndpointOnlyClient {
            url: "https://api.z.ai/api/anthropic".to_string(),
            model: "glm-5.3".to_string(),
        };
        assert_eq!(
            discover_window(&config, &client).await,
            Some(1_000_000),
            "the table answers for a provider that publishes nothing, \
             unscaled - this family reports usage and calibrates"
        );
        // The table rides whole for the usage-less family too: it
        // documents cloud models whose providers report usage, so a
        // margin applied to its answers would double-discount them.
        // The endpoint here is remote, so the probe answers nothing
        // and the table is the only witness — deterministically, by
        // the loopback gate, without a dial.
        let config = probe_config(ApiType::Ollama);
        let client = EndpointOnlyClient {
            url: "https://ollama.internal.example".to_string(),
            model: "glm-5.3".to_string(),
        };
        assert_eq!(
            discover_window(&config, &client).await,
            Some(1_000_000),
            "the table's answer rides whole whatever the family asks for it"
        );
    }

    #[tokio::test]
    async fn a_live_answer_beats_the_known_model_table() {
        // A server that publishes the session model's window is the
        // better witness — a local deployment may size the same model
        // differently than its cloud parent — so the probe's answer
        // outranks the table's.
        let port = serve_once(r#"{"n_ctx": 16384}"#.to_string());
        let config = probe_config(ApiType::OpenAi);
        let client = EndpointOnlyClient {
            url: format!("http://127.0.0.1:{port}/v1"),
            model: "glm-5.3".to_string(),
        };
        assert_eq!(
            discover_window(&config, &client).await,
            Some(16_384),
            "the live server's answer outranks the table's, unscaled - \
             this family reports usage and calibrates"
        );
    }
}
