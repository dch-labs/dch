//! Fixtures shared by the runner tree's test modules: an offline config,
//! a sample runner context, a canned-SSE provider endpoint with the configs
//! that point a runner at it, and an observer that records a run's events.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use dch_config::DchConfig;
use dch_tools::RunnerContext;

/// A bare runner context whose working directory is `cwd`.
///
/// Fresh from the constructor, so it holds only what every context starts
/// with: the directory made absolute and normalized, an empty todo list, an
/// empty question slot, and the default resolve policy. The pipeline seams
/// are driven without a builder, and this is the context such a dispatch
/// reads.
pub(super) fn sample_context(cwd: &str) -> Arc<RunnerContext> {
    Arc::new(RunnerContext::new(PathBuf::from(cwd)))
}

/// A default config whose provider no request can reach.
///
/// Everything is the default except the API section, which names the OpenAI
/// type, a dummy key, the model `test-model`, and `example.invalid` as the
/// base URL — a reserved name that never resolves. A runner built from it
/// goes through the real `build` and still cannot leave the machine: the
/// config for the pins of construction and of the seams between runs, where
/// no provider call is ever meant to happen.
pub(super) fn offline_config() -> DchConfig {
    use dch_config::ApiConfig;
    use dch_config::ApiType;
    let mut config = DchConfig::default();
    config.api = ApiConfig {
        api_type: ApiType::OpenAi,
        base_url: "https://example.invalid".to_string(),
        api_key: Some("dummy".to_string()),
        model: "test-model".to_string(),
        ..ApiConfig::default()
    };
    config
}

/// A canned SSE turn in which the model answers with `text` and stops.
///
/// OpenAI's streaming shape: the first `data:` chunk carries the text as a
/// content delta, the second carries the `stop` finish reason beside an
/// empty delta, and the `[DONE]` sentinel closes the stream. A run served
/// this body finishes its turn with no tool call — the shortest complete
/// turn a conversation can hold.
pub(super) fn sse_text_turn(text: &str) -> String {
    [
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": {"content": text}, "finish_reason": null}]
        }),
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": null, "finish_reason": "stop"}]
        }),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect()
}

/// A canned SSE turn in which the model calls `Read` on `note.txt`.
///
/// The one-call turn the sequencing pins start from: served it, a run
/// dispatches a single read, and the request it sends next carries the tool
/// result for a test to find.
pub(super) fn sse_read_tool_call_turn() -> String {
    sse_tool_call_turn("Read", &serde_json::json!({"path": "note.txt"}))
}

/// A canned turn carrying several complete tool calls at once.
///
/// Emits every call fully formed in a single delta (distinct indices
/// and ids, arguments complete), then the `tool_calls` stop — the
/// wire shape of a model that batches independent calls into one
/// response instead of spending a round-trip per call.
pub(super) fn sse_batched_tool_calls_turn(calls: &[(&str, &serde_json::Value)]) -> String {
    let entries: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (tool, args))| {
            serde_json::json!({
                "index": i,
                "id": format!("call_{}", i + 1),
                "function": {"name": tool, "arguments": args.to_string()}
            })
        })
        .collect();
    [
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": {"tool_calls": entries}, "finish_reason": null}]
        }),
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": null, "finish_reason": "tool_calls"}]
        }),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect()
}

/// A canned SSE turn in which the model calls `tool` once with `args`.
///
/// Streamed the way the client assembles it: the first `data:` chunk
/// announces the call under index `0` and the id `call_1` with empty
/// arguments, the second carries the arguments, and the third carries the
/// `tool_calls` finish reason ahead of the `[DONE]` sentinel. A run served
/// this turn dispatches one call; the request that follows is answered by
/// the next response in the server's queue.
pub(super) fn sse_tool_call_turn(tool: &str, args: &serde_json::Value) -> String {
    let args = args.to_string();
    [
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1",
                "function": {"name": tool, "arguments": ""}
            }]}, "finish_reason": null}]
        }),
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "function": {"arguments": args}
            }]}, "finish_reason": null}]
        }),
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": null, "finish_reason": "tool_calls"}]
        }),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect()
}

/// A one-text-part Gemini streaming turn that finishes with STOP.
///
/// Gemini's SSE shape: each `data:` line is a complete JSON object
/// carrying `candidates[0].content.parts[]` fragments, and the
/// terminal chunk carries `finishReason` plus `usageMetadata`. No
/// `[DONE]` sentinel exists on this wire — the stream simply ends.
pub(super) fn gemini_sse_text_turn(text: &str) -> String {
    let chunks = [
        serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": text}], "role": "model"}
            }]
        }),
        serde_json::json!({
            "candidates": [{"finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 6}
        }),
    ];
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body
}

/// A canned-SSE provider endpoint on an ephemeral local port.
///
/// Serves one response body per accepted connection (in order), records
/// each request body, and optionally delays before answering so a run can
/// be cancelled while in flight.
pub(super) struct SseServer {
    /// The local port the server listens on.
    ///
    /// Fixed before the constructor returns, so a wire config built against
    /// it addresses this server and nothing else.
    pub(super) port: u16,

    /// Every request received so far, as the client wrote it.
    ///
    /// Appended in arrival order by the accept loop and read through
    /// [`recorded_requests`]. It sits behind a mutex and an `Arc` because
    /// the loop that appends runs on a task of its own.
    pub(super) requests: Arc<Mutex<Vec<String>>>,
}

/// One canned HTTP response: status code plus body.
///
/// The server consumes these one per connection, in order, so a
/// test scripts a whole multi-request conversation — including
/// its failures — by stacking them.
#[derive(Clone)]
pub(super) struct CannedResponse {
    /// HTTP status the server answers with.
    ///
    /// Any code the provider client can receive; non-2xx bodies
    /// become provider errors the run surfaces.
    pub(super) status: u16,
    /// Response body bytes.
    ///
    /// Verbatim on the wire — for SSE responses, the full
    /// `data:`-event stream the client parses.
    pub(super) body: String,
}

impl SseServer {
    /// Start a server answering with `responses` in order, without delay.
    ///
    /// Each connection draws the next body as a `200` in
    /// `text/event-stream`; a connection arriving once the list is spent
    /// receives a bare `500`. The common case for a scripted conversation,
    /// and the plainest way into
    /// [`start_canned_with_completion`](Self::start_canned_with_completion),
    /// which says what the server does with each connection.
    pub(super) async fn start(responses: Vec<String>) -> Self {
        Self::start_with_delay(responses, std::time::Duration::ZERO).await
    }

    /// Start a server answering with `responses` in order, without delay.
    ///
    /// The status-carrying form of [`Self::start`]: each connection draws
    /// the next canned response, status and body together, so a script can
    /// put a failure in among its turns.
    pub(super) async fn start_canned(responses: Vec<CannedResponse>) -> Self {
        Self::start_canned_with_delay(responses, std::time::Duration::ZERO).await
    }

    /// Start a server that waits `delay` before answering each request.
    ///
    /// Otherwise [`Self::start`]: the bodies in order, each as a `200`. The
    /// pause opens the window a test needs to act on a run that is still in
    /// flight — cancel it, or push guidance into it — before the answer
    /// lands.
    pub(super) async fn start_with_delay(
        responses: Vec<String>,
        delay: std::time::Duration,
    ) -> Self {
        let canned = responses
            .into_iter()
            .map(|body| CannedResponse { status: 200, body })
            .collect();
        Self::start_canned_with_delay(canned, delay).await
    }

    /// Start a server with canned responses that waits `delay` per request.
    ///
    /// [`Self::start_canned`] with the pause of [`Self::start_with_delay`]:
    /// statuses and bodies in order, each one held back for `delay`. The
    /// constructors without a completion pass through here, a zero delay
    /// being no pause at all.
    pub(super) async fn start_canned_with_delay(
        responses: Vec<CannedResponse>,
        delay: std::time::Duration,
    ) -> Self {
        Self::start_canned_with_completion(responses, delay, None).await
    }

    /// Serve `streamed` to streaming requests and `completion` to the rest.
    ///
    /// Compaction summarizers issue plain JSON completion calls alongside
    /// the conversation's SSE turns; serving one fixed body to every
    /// non-streaming request lets a test drive a compacting session
    /// without knowing how many summarizer calls a pass makes — the
    /// recorded requests still show each one.
    pub(super) async fn start_with_completion(streamed: Vec<String>, completion: String) -> Self {
        let canned = streamed
            .into_iter()
            .map(|body| CannedResponse { status: 200, body })
            .collect();
        Self::start_canned_with_completion(
            canned,
            std::time::Duration::ZERO,
            Some(CannedResponse {
                status: 200,
                body: completion,
            }),
        )
        .await
    }

    /// Serve `streamed` to streaming requests; refuse every other one.
    ///
    /// The mirror of [`Self::start_with_completion`]: each
    /// non-streaming request — a compaction summarizer's call — draws a
    /// 500, so a test can pin the fallback floor carrying the pass when
    /// the summarizer endpoint is down.
    pub(super) async fn start_with_failing_completion(streamed: Vec<String>) -> Self {
        let canned = streamed
            .into_iter()
            .map(|body| CannedResponse { status: 200, body })
            .collect();
        Self::start_canned_with_completion(
            canned,
            std::time::Duration::ZERO,
            Some(CannedResponse {
                status: 500,
                body: "{\"error\":{\"message\":\"summarizer endpoint unavailable\"}}".to_string(),
            }),
        )
        .await
    }

    /// Start the server every other constructor delegates to.
    ///
    /// Binds an ephemeral local port before returning, so the caller can
    /// build a config against it before any request is sent. The accept
    /// loop then serves one connection at a time: it records the request,
    /// waits `delay`, and answers. When `completion` is given, a request
    /// whose body carries `"stream":false` is answered with it; every other
    /// request draws the next entry of `responses`, or a bare `500` once
    /// the queue is spent. A `200` is served as `text/event-stream`, any
    /// other status as `application/json`, and every response closes the
    /// connection.
    ///
    /// # Panics
    ///
    /// Panics when no ephemeral local port can be bound — fixture setup
    /// with no failure mode a test could react to.
    pub(super) async fn start_canned_with_completion(
        responses: Vec<CannedResponse>,
        delay: std::time::Duration,
        completion: Option<CannedResponse>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind");
        let port = listener
            .local_addr()
            .expect("bound socket has an address")
            .port();
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let mut queue: std::collections::VecDeque<CannedResponse> = responses.into();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut request_text = None;
                if let (Some(req), Ok(mut recorded)) =
                    (read_http_request(&mut sock).await, task_requests.lock())
                {
                    recorded.push(req.clone());
                    request_text = Some(req);
                }
                if delay > std::time::Duration::ZERO {
                    tokio::time::sleep(delay).await;
                }
                let canned = match (&completion, request_text.as_deref()) {
                    (Some(completion), Some(req)) if req.contains("\"stream\":false") => {
                        Some(completion.clone())
                    }
                    _ => queue.pop_front(),
                };
                let bytes = canned.map_or_else(
                    || {
                        "HTTP/1.1 500 No canned response\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                            .to_string()
                    },
                    |r| {
                        let content_type = if r.status == 200 {
                            "text/event-stream"
                        } else {
                            "application/json"
                        };
                        format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            r.status,
                            reason_phrase(r.status),
                            r.body.len(),
                            r.body
                        )
                    },
                );
                let _ = sock.write_all(bytes.as_bytes()).await;
            }
        });
        Self { port, requests }
    }
}

/// Reason phrase for the statuses the canned server emits.
///
/// The hand-written HTTP response needs one per status line;
/// keeping the table local avoids an http-crate dependency just
/// to spell "Unauthorized" in a fixture.
pub(super) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Read one HTTP request off `sock`, headers and body together.
///
/// Reads until the blank line that ends the headers, takes the body's
/// length from `Content-Length`, and reads on until that many body bytes are
/// in hand. What comes back is the request as the provider client wrote it,
/// so a pin can read the headers and the JSON alike. Returns `None` when
/// the peer closes before the headers end or a read fails; a body the peer
/// cut short is returned as received.
pub(super) async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let total = header_end + 4 + content_length;
    while buf.len() < total {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The offset of the blank line that ends a request's headers.
///
/// Looks for the `\r\n\r\n` HTTP puts between headers and body and returns
/// where that sequence starts, so the caller can slice the header block off
/// the front of `buf`. Returns `None` when the sequence is not there yet —
/// the state of a request whose headers have not arrived in full.
pub(super) fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// An observer that records the lifecycle events it receives, in order.
///
/// Gives the ordering pins something deterministic to read: each
/// callback appends its name, and the finished run's sequence is
/// asserted against the expected lifecycle order.
pub(super) struct RecordingObserver {
    /// The lifecycle events received so far, oldest first.
    ///
    /// Names only, each the caller's own `'static` string, appended by
    /// [`Self::record`] and copied out by [`Self::snapshot`].
    pub(super) events: Mutex<Vec<&'static str>>,
}

impl RecordingObserver {
    /// An observer that has recorded nothing.
    ///
    /// The record grows only as the loop calls back, so a fresh observer
    /// registered on a builder holds exactly the events of the runs it
    /// observed.
    pub(super) fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Append `event` to the record.
    ///
    /// The one write every callback makes. A record lock left poisoned by a
    /// panic elsewhere drops the event instead of carrying that panic into
    /// the loop.
    pub(super) fn record(&self, event: &'static str) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    /// The events recorded so far, in the order they arrived.
    ///
    /// A copy, so a pin can assert on the sequence while the run goes on
    /// appending. A poisoned record lock reads as an empty record.
    pub(super) fn snapshot(&self) -> Vec<&'static str> {
        self.events.lock().map_or_default(|events| events.clone())
    }
}

impl loopctl::observer::LoopObserver for RecordingObserver {
    fn name(&self) -> &'static str {
        "recording"
    }
    fn on_run_start(&self, _: &loopctl::observer::RunStartContext) {
        self.record("run_start");
    }
    fn on_turn_start(&self, _: &loopctl::observer::TurnStartContext) {
        self.record("turn_start");
    }
    fn on_response(&self, _: &loopctl::observer::ResponseContext) {
        self.record("response");
    }
    fn on_turn_end(&self, _: &loopctl::observer::TurnEndContext) {
        self.record("turn_end");
    }
    fn on_run_end(&self, _: &loopctl::observer::RunEndContext) {
        self.record("run_end");
    }
    fn on_compaction(&self, _: &loopctl::observer::CompactedContext) {
        self.record("compaction");
    }
    fn on_compaction_failed(&self, _: &loopctl::observer::CompactionFailedContext) {
        self.record("compaction_failed");
    }
    fn on_pre_compaction(&self, _: &loopctl::observer::PreCompactionContext) {
        self.record("pre_compaction");
    }
}

/// A config that points an OpenAI-type runner at the local `port`.
///
/// The base URL is the canned server's, the key a dummy, and `max_turns` the
/// turn budget. The context window is set explicitly to the fallback value,
/// so a build never probes the server for one and the first request a run
/// sends is always the conversation — never a discovery call the scripted
/// responses do not cover.
pub(super) fn wire_config(port: u16, max_turns: usize) -> DchConfig {
    wire_config_for(dch_config::ApiType::OpenAi, port, max_turns)
}

/// A config that points a Gemini-type runner at the local `port`.
///
/// The twin of [`wire_config`] for the Gemini wire: the same dummy key,
/// model, explicit context window, and turn budget, with only the API type
/// changed. Paired with [`gemini_sse_text_turn`], it drives one conversation
/// over the request and stream shapes that provider uses.
pub(super) fn gemini_wire_config(port: u16, max_turns: usize) -> DchConfig {
    wire_config_for(dch_config::ApiType::Gemini, port, max_turns)
}

/// The constructor behind the wire configs: `api_type` at the local `port`.
///
/// Nothing beyond what a scripted run needs: a dummy key, the model
/// `test-model`, a ten-second request timeout, the fallback context window
/// set explicitly, and `max_turns` as the turn budget. Every other setting
/// is the default.
pub(super) fn wire_config_for(
    api_type: dch_config::ApiType,
    port: u16,
    max_turns: usize,
) -> DchConfig {
    use dch_config::ApiConfig;
    let mut config = DchConfig::default();
    config.api = ApiConfig {
        api_type,
        base_url: format!("http://127.0.0.1:{port}"),
        api_key: Some("dummy".to_string()),
        model: "test-model".to_string(),
        request_timeout_secs: 10,
        ..ApiConfig::default()
    };
    config.api.context_window = Some(dch_config::FALLBACK_CONTEXT_WINDOW);
    config.runner.max_turns = max_turns;
    config
}

/// Every request the server has recorded, in arrival order.
///
/// A copy of the stored request texts — each one headers and body as the
/// client wrote it — for the pins that count round-trips, read the JSON of
/// one request, or scan them all for a wording. A poisoned record lock reads
/// as no requests.
pub(super) fn recorded_requests(server: &SseServer) -> Vec<String> {
    server.requests.lock().map_or_default(|reqs| reqs.clone())
}

/// The roles of every message in a recorded request whose text
/// contains `needle`.
///
/// Reads the request's JSON body, so a pin can assert not only
/// that a wording reached the provider but in whose name it
/// traveled.
pub(super) fn message_roles(request: &str, needle: &str) -> Vec<String> {
    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    parsed
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map_or_default(|messages| {
            messages
                .iter()
                .filter(|message| {
                    message
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|content| content.contains(needle))
                })
                .map(|message| {
                    message
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
}
