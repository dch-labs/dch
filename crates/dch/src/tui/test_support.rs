#![cfg(test)]

//! Fixtures shared by the tui module's tests: driver and
//! transcript scaffolding, the local SSE provider stand-in,
//! and the small builders the pins reuse.

use dch_config::ApiType;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dch_tui::Submission;
use dch_tui::TuiMessage;
use dch_tui::TuiObserverState;
use tokio::sync::mpsc;

use crate::tui::driver::drive_submissions;
use crate::tui::transcript::TranscriptHandle;
use crate::tui::transcript::TranscriptWorker;

/// Register the todo nudge the way `run_tui_session` does.
///
/// The driver tests that speak of nudges build their runners
/// through this. Production's echo is a no-op — the reminder
/// surfaces nowhere a user reads — so the tests' echo writes the
/// steered lane instead: it makes the offer observable to pins
/// that count rows, a stand-in the production registration no
/// longer needs. The leave-to-extend keeps the production shape —
/// a shutdown flag the session never raises — and the decline
/// lane's own pins raise their flags through
/// [`with_gated_nudge`].
pub(crate) fn with_nudge<'a>(
    builder: dch_loop::RunnerBuilder<'a>,
    state: &TuiObserverState,
) -> dch_loop::RunnerBuilder<'a> {
    with_gated_nudge(builder, state, Arc::new(AtomicBool::new(false)))
}

/// Register the todo nudge with a leave-to-extend the test controls.
///
/// The gate is the production predicate's own shape — an
/// `Arc<AtomicBool>` read with `!load` — so the decline lane's
/// tests answer from the same closure the session registration
/// builds, over a flag they raise at the interleaving under test.
pub(crate) fn with_gated_nudge<'a>(
    builder: dch_loop::RunnerBuilder<'a>,
    state: &TuiObserverState,
    gate: Arc<AtomicBool>,
) -> dch_loop::RunnerBuilder<'a> {
    let echo_state = state.clone();
    builder.with_todo_nudge(
        move |reminder| {
            echo_state.push_steered(reminder);
            echo_state.render_notify.notify(1);
        },
        move || !gate.load(Ordering::SeqCst),
    )
}

/// Queue a prompt the way the display does, then hand its id out.
///
/// The pair is the whole contract between the two halves: the entry
/// lands in the shared queue before its id travels, because a driver
/// that claimed an id naming nothing would have nothing to run. Every
/// driver test seeds through here rather than sending a bare payload the
/// display could never have produced.
pub(crate) fn queue_prompt(
    state: &TuiObserverState,
    tx: &mpsc::UnboundedSender<Submission>,
    text: &str,
) -> u64 {
    let id = state
        .queued
        .lock()
        .expect("queue lock")
        .push(text.to_string());
    tx.send(Submission::Run { id })
        .expect("driver channel open");
    id
}

/// Tell the driver the prompt `id` was corrected after it went out.
///
/// The panel's commit does this when the entry it was saving had
/// already been claimed: the text has no queue slot left to go back to,
/// so it travels with the id of the turn it replaces.
pub(crate) fn correct_prompt(tx: &mpsc::UnboundedSender<Submission>, id: u64, text: &str) {
    tx.send(Submission::Correct {
        id,
        text: text.to_string(),
    })
    .expect("driver channel open");
}

/// What waits in the shared queue, as `(id, text)` pairs.
///
/// The read a test needs after a run: whether a prompt is still
/// waiting, and which text it waits with.
pub(crate) fn queued_prompts(state: &TuiObserverState) -> Vec<(u64, String)> {
    let queue = state.queued.lock().expect("queue lock");
    (0..queue.len())
        .filter_map(|index| queue.get(index))
        .map(|entry| (entry.id, entry.text.clone()))
        .collect()
}

/// A transcript handle parked on a throwaway directory.
///
/// The drive tests need the driver's fifth argument without
/// caring where its snapshots land; the worker behind this
/// handle writes into a tempdir and parks on drop.
pub(crate) fn parked_transcript() -> TranscriptHandle {
    let dir = tempfile::tempdir().expect("transcript tempdir");
    let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
        uuid::Uuid::new_v4(),
        "m".to_string(),
        dir.keep(),
    ));
    let (handle, _worker) = TranscriptWorker::spawn(saver);
    handle
}

/// Spawn the driver over a parked transcript.
///
/// The rig every driver test stands up, one line instead of
/// seven; no test asserts on the save lane, so the transcript is
/// parked in every spawn.
pub(crate) fn spawn_driver(
    runner: dch_loop::Runner,
    receiver: mpsc::UnboundedReceiver<Submission>,
    state: TuiObserverState,
    shutting_down: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(drive_submissions(
        runner,
        receiver,
        state,
        shutting_down,
        parked_transcript(),
    ))
}

/// A config whose provider endpoint refuses connections on the
/// loopback interface.
///
/// A real run then fails fast without touching the network.
pub(crate) fn unreachable_config() -> dch_config::DchConfig {
    let mut config = dch_config::DchConfig::default();
    config.api.api_type = ApiType::OpenAi;
    config.api.base_url = "http://127.0.0.1:1".to_string();
    config.api.api_key = Some("dummy".to_string());
    config.api.model = "test-model".to_string();
    config
}

/// Build a real runner against the unreachable endpoint.
///
/// Construction is offline — the builder only assembles the
/// provider client; the first request, and with it the failure,
/// happens inside `run`.
pub(crate) async fn unreachable_runner() -> dch_loop::Runner {
    let config = unreachable_config();
    let dir = tempfile::tempdir().expect("a temp workdir");
    dch_loop::Runner::builder(&config, dir.path())
        .build()
        .await
        .expect("offline runner construction")
}

/// A driver shutdown flag in the given state.
///
/// Reads as the driver's loop condition, so a `true` flag makes
/// the pins below exercise the quitting path without any
/// terminal or user input.
pub(crate) fn shutdown_flag(set: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(set))
}

/// A saver rooted at a throwaway directory plus its coordinates.
///
/// The writer pins never touch the real sessions root: the
/// saver's base is the temp dir, returned alongside it (kept
/// alive by the caller) and the session id its file lands under.
pub(crate) fn worker_saver() -> (
    Arc<crate::session::SessionSaver>,
    tempfile::TempDir,
    uuid::Uuid,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = uuid::Uuid::new_v4();
    let saver = Arc::new(crate::session::SessionSaver::with_base_dir(
        id,
        "worker-model".to_string(),
        dir.path().to_path_buf(),
    ));
    (saver, dir, id)
}

/// Read a worker-written transcript back from disk.
///
/// Parses the envelope directly rather than going through the
/// saver, so a pin proves what is actually in the file — not
/// what the saver's own reader would reconstruct from it.
pub(crate) fn saved_messages(dir: &std::path::Path, id: uuid::Uuid) -> Vec<TuiMessage> {
    let path = dir.join(id.to_string()).join("session.json");
    let json = std::fs::read_to_string(path).expect("the transcript file");
    let envelope: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    serde_json::from_value(
        envelope
            .get("messages")
            .expect("the messages field")
            .clone(),
    )
    .expect("the messages parse back")
}

/// Read a worker-written transcript's token totals back.
///
/// Same direct-envelope parse as [`saved_messages`], so a pin
/// proves what the file itself records.
pub(crate) fn saved_tokens(dir: &std::path::Path, id: uuid::Uuid) -> crate::session::SessionTokens {
    let path = dir.join(id.to_string()).join("session.json");
    let json = std::fs::read_to_string(path).expect("the transcript file");
    let envelope: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    serde_json::from_value(envelope.get("tokens").expect("the tokens field").clone())
        .expect("the tokens parse back")
}

/// Holds a thread at a chosen point until released.
///
/// The gated-spawn pins park the writer on this between its
/// predicate check and its wait — the state a lost notification
/// lives in — for exactly as long as the test needs.
pub(crate) struct Gate {
    open: std::sync::Mutex<bool>,
    signal: std::sync::Condvar,
    parked: std::sync::atomic::AtomicBool,
}

impl Gate {
    pub(crate) fn closed() -> Self {
        Self {
            open: std::sync::Mutex::new(false),
            signal: std::sync::Condvar::new(),
            parked: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn hold(&self) {
        self.parked.store(true, Ordering::SeqCst);
        let mut open = self.open.lock().expect("the gate lock");
        while !*open {
            open = self.signal.wait(open).expect("the gate wait");
        }
        self.parked.store(false, Ordering::SeqCst);
    }

    pub(crate) fn release(&self) {
        let mut open = self.open.lock().expect("the gate lock");
        *open = true;
        drop(open);
        self.signal.notify_all();
    }

    pub(crate) fn wait_parked(&self) {
        while !self.parked.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
}

/// One canned SSE text turn, framed for the OpenAI streaming wire.
///
/// Two `data:` chunks — the text delta, then the stop — plus the
/// terminal `[DONE]` sentinel, so a run that consumes one body
/// completes a single turn cleanly.
pub(crate) fn sse_text_turn(text: &str) -> String {
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

/// A canned non-streaming completion naming the session.
///
/// What the naming call is answered with when the endpoint routes
/// on the naming marker, in the JSON shape a non-streaming
/// OpenAI reply parses. Numbered by arrival: a race pin reads the
/// number to tell which session's prompt coined the title that
/// landed.
pub(crate) fn canned_title_body(which: usize) -> String {
    serde_json::json!({
        "id": "title", "model": "test-model",
        "choices": [{
            "message": {"role": "assistant",
                "content": format!("Canned Session Title {which}")},
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

/// A canned-SSE provider endpoint on an ephemeral local port.
///
/// Serves the queued bodies one per accepted connection in order,
/// records each request it reads, and sleeps the given delay
/// before answering — long enough that a test can cancel a run
/// while its provider request is provably still in flight.
pub(crate) struct SseServer {
    /// The port the canned endpoint listens on.
    ///
    /// Ephemeral, and bound before the first request is sent, so
    /// the config can point straight at it.
    pub(crate) port: u16,

    /// Every request body the endpoint has read, in order.
    ///
    /// Shared with the accept task; a test reads the count to
    /// prove how many runs actually reached a provider.
    pub(crate) requests: Arc<std::sync::Mutex<Vec<String>>>,

    /// Every naming request body the endpoint has read, in order.
    ///
    /// Kept apart from the turn bodies: the naming side-call is
    /// not a conversation turn, so the turn count stays clean
    /// while a pin reads exactly what the title ask carried.
    naming_requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl SseServer {
    /// Start the endpoint with one body per connection and the delay.
    ///
    /// The accept loop outlives the return; a connection past the
    /// last queued body is answered with a bare 500 so an
    /// unexpected extra run still settles instead of hanging.
    /// Start the endpoint where each response carries its own delay.
    ///
    /// The shape an interleaving needs: some turns answer at once
    /// while one sits held, so a mid-flight event lands against
    /// exactly the run the test names. Naming replies answer at
    /// once.
    pub(crate) async fn start_timed(responses: Vec<(String, std::time::Duration)>) -> Self {
        Self::start_shared(responses, std::time::Duration::ZERO).await
    }

    pub(crate) async fn start_with_delay(
        responses: Vec<String>,
        delay: std::time::Duration,
    ) -> Self {
        Self::start_shared(
            responses.into_iter().map(|body| (body, delay)).collect(),
            std::time::Duration::ZERO,
        )
        .await
    }

    /// Start the endpoint where every naming reply waits out a delay.
    ///
    /// The shape a naming-race pin needs: the turn bodies answer on
    /// their own schedule while each title reply sits held for the
    /// delay, so a test can move the host onto another session
    /// before a session's name lands. Titles are numbered by
    /// arrival, so the pin can tell which prompt's words landed.
    pub(crate) async fn start_with_naming_delay(
        responses: Vec<(String, std::time::Duration)>,
        naming_delay: std::time::Duration,
    ) -> Self {
        Self::start_shared(responses, naming_delay).await
    }

    /// The accept loop the starters share.
    ///
    /// Serves the queued turn bodies one connection at a time, in
    /// order, while the naming arm answers off the loop — a held
    /// title never blocks a turn.
    pub(crate) async fn start_shared(
        responses: Vec<(String, std::time::Duration)>,
        naming_delay: std::time::Duration,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind");
        let port = listener
            .local_addr()
            .expect("bound socket has an address")
            .port();
        let requests: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let naming_requests: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let task_naming = Arc::clone(&naming_requests);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let mut queue: std::collections::VecDeque<(String, std::time::Duration)> =
                responses.into();
            let naming_arrivals = std::sync::atomic::AtomicUsize::new(0);
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_http_request(&mut sock).await;
                if req
                    .as_deref()
                    .is_some_and(|body| body.contains(dch_loop::NAME_SYSTEM_PROMPT))
                {
                    if let (Some(body), Ok(mut recorded)) = (req, task_naming.lock()) {
                        recorded.push(body);
                    }
                    let which = naming_arrivals
                        .fetch_add(1, Ordering::SeqCst)
                        .saturating_add(1);
                    tokio::spawn(async move {
                        use tokio::io::AsyncWriteExt as _;
                        if naming_delay > std::time::Duration::ZERO {
                            tokio::time::sleep(naming_delay).await;
                        }
                        let title = canned_title_body(which);
                        let answer = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{title}",
                            title.len()
                        );
                        let _ = sock.write_all(answer.as_bytes()).await;
                    });
                    continue;
                }
                let body = queue.pop_front();
                if let (Some(req), Ok(mut recorded)) = (req, task_requests.lock()) {
                    recorded.push(req);
                }
                if let Some((_, wait)) = body.as_ref()
                    && *wait > std::time::Duration::ZERO
                {
                    tokio::time::sleep(*wait).await;
                }
                let bytes = body.map_or_else(
                    || {
                        "HTTP/1.1 500 No canned response\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                            .to_string()
                    },
                    |(body, _)| {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    },
                );
                let _ = sock.write_all(bytes.as_bytes()).await;
            }
        });
        Self {
            port,
            requests,
            naming_requests,
        }
    }

    /// How many requests have reached the endpoint so far.
    ///
    /// Each accepted connection records exactly once, so the
    /// count is the number of provider requests runs have made.
    pub(crate) fn request_count(&self) -> usize {
        self.requests.lock().map_or(0, |reqs| reqs.len())
    }

    /// The body of every request received, oldest first.
    ///
    /// The count says how many runs reached the provider; the
    /// bodies say what they carried, which is what a pin needs to
    /// show that a piece of text was *not* among them.
    pub(crate) fn bodies(&self) -> Vec<String> {
        self.requests.lock().map_or_default(|reqs| reqs.clone())
    }

    /// The body of every naming request received, oldest first.
    ///
    /// The naming side-call's wire truth: what prompt the title
    /// ask actually carried, whatever the run that armed it
    /// claimed.
    pub(crate) fn naming_bodies(&self) -> Vec<String> {
        self.naming_requests
            .lock()
            .map_or_default(|reqs| reqs.clone())
    }
}

/// Read one HTTP request off the socket, headers plus body.
///
/// Returns `None` on a truncated read; the caller then records
/// nothing for the connection.
pub(crate) async fn read_http_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
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

/// The offset of the header/body boundary, if the bytes hold one.
///
/// Scans for the blank-line separator the wire framing uses;
/// `None` until the full four-byte sequence has arrived, so a
/// partial header keeps the read loop waiting for more bytes.
pub(crate) fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The prompt echoes the display owes or has landed as rows.
///
/// Read off the graduation stream and the waiting pin together,
/// because an echo's place in the transcript is its place among
/// the replies: a test that checked them separately would pass
/// however they were ordered against everything else. The pin's
/// half is the steers a run accepted but no boundary has read —
/// rows the display owes and holds at its edge until then, among
/// them a nudge's echo, whose harness may hold no observer to
/// fire the boundary that would land it.
pub(crate) fn claimed(state: &TuiObserverState) -> Vec<String> {
    let mut echoes = state
        .graduations
        .lock()
        .expect("graduation lock")
        .iter()
        .filter_map(|graduation| match graduation {
            dch_tui::observer::Graduation::Sent { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    echoes.extend(
        state
            .pending_steers
            .lock()
            .expect("the pending-steer lock")
            .iter()
            .cloned(),
    );
    echoes
}

/// The assistant turns' text blocks across a resolved history.
///
/// Reads the machine snapshot the driver publishes after each run,
/// so a pin can speak of the answers that landed rather than the
/// requests that went out.
pub(crate) fn assistant_texts(messages: &[loopctl::message::Message]) -> Vec<&String> {
    messages
        .iter()
        .filter(|message| matches!(message.role, loopctl::message::Role::Assistant))
        .flat_map(|message| {
            message.parts.iter().filter_map(|part| match part {
                loopctl::message::MessagePart::Text { text } => Some(text),
                _ => None,
            })
        })
        .collect()
}

/// An OpenAI-wire config pointing at the canned endpoint's port.
///
/// The same shape as [`unreachable_config`], but live: a run
/// against it issues a real request the endpoint answers on its
/// own held-back schedule.
pub(crate) fn sse_config(port: u16) -> dch_config::DchConfig {
    dch_config::DchConfig {
        api: dch_config::ApiConfig {
            api_type: ApiType::OpenAi,
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: Some("dummy".to_string()),
            model: "test-model".to_string(),
            request_timeout_secs: 10,
            ..dch_config::ApiConfig::default()
        },
        ..dch_config::DchConfig::default()
    }
}

/// A canned streaming response that ends its turn on a tool call.
///
/// The shape a run needs to span more than one turn: a text-only answer
/// closes the run at once, and compaction is only ever considered
/// between turns, so a single-turn test cannot reach it however large
/// the answer is.
pub(crate) fn sse_tool_call_turn(tool: &str, args: &serde_json::Value) -> String {
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
