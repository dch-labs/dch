//! JSON-RPC client for a language-server process over stdio.
//!
//! Speaks the LSP wire format — `Content-Length`-framed JSON-RPC over the
//! child's piped stdin/stdout — for the two operations the tool needs:
//! hover and go-to-definition. Only the client side; the server is the
//! spawned language-server binary.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use loopctl::tool::ToolError;
use lsp_types::GotoDefinitionParams;
use lsp_types::GotoDefinitionResponse;
use lsp_types::Hover;
use lsp_types::HoverParams;
use lsp_types::PartialResultParams;
use lsp_types::Position;
use lsp_types::TextDocumentIdentifier;
use lsp_types::TextDocumentPositionParams;
use lsp_types::WorkDoneProgressParams;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStderr;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use url::Url;

use super::servers::LspServerConfig;

/// Why a request failed, split by what it says about the pooled server.
///
/// The split is operational, not cosmetic: a caller that reuses a pooled
/// client may only discard it for [`RequestError::Transport`] — the
/// exchange broke, so the stream may be desynced or the process dead.
/// A [`RequestError::Server`] means the transport delivered a
/// well-formed reply, so the pooled client stays serviceable no matter
/// how wrong the reply was.
#[derive(Debug)]
pub(crate) enum RequestError {
    /// The exchange could not complete; the pooled client is suspect.
    ///
    /// Write, read, framing, UTF-8, and JSON-parse failures, timeouts,
    /// and id mismatches (a desynced stream) all land here — the caller
    /// should evict the client so the next call cold-starts.
    Transport(ToolError),

    /// The server answered with a JSON-RPC `error` member, or a
    /// precondition failed before anything reached the wire.
    ///
    /// The transport is healthy; evicting here would throw away a live
    /// server (and its indexing) over an ordinary per-request reply.
    Server(ToolError),
}

impl RequestError {
    /// The tool-facing error, regardless of class.
    ///
    /// Both variants carry the full error; this flattens them for
    /// surfacing to the model once the eviction decision — which must
    /// look at the variant, not the flattened error — has been made.
    pub(crate) fn into_error(self) -> ToolError {
        match self {
            Self::Transport(e) | Self::Server(e) => e,
        }
    }
}

/// The cap on a single incoming message's declared `Content-Length`.
///
/// The header is server-controlled; without a cap, a corrupt or hostile
/// frame (`Content-Length: 4000000000`) allocates gigabytes before any I/O
/// happens. The messages this client exchanges — initialize, hover, and
/// definition replies — are a few kilobytes, so `16 MiB` sits far above
/// any real payload while bounding allocation.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Per-request timeout budget, in seconds.
///
/// Reading is the unbounded part of an exchange — a wedged server simply
/// never answers — so every request and notification write is bounded by
/// this budget and surfaces as a typed error instead of parking the tool,
/// and every same-root caller queued behind it, forever. An atomic rather
/// than a constant so tests can tighten it without waiting real tens of
/// seconds.
static REQUEST_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(30);

/// Why a language server failed to start.
///
/// The missing-binary case is its own variant so callers classify it from
/// the OS error kind rather than by matching message text, which is
/// spelled differently across platforms.
#[derive(Debug)]
pub(crate) enum SpawnError {
    /// The server binary was not found on `PATH`.
    ///
    /// Carries the command name so the caller can name the binary to
    /// install in its install hint.
    BinaryMissing {
        /// The command that could not be spawned.
        ///
        /// The exact `command` from [`LspServerConfig`], e.g.
        /// `rust-analyzer`.
        command: String,
    },

    /// The server failed to start or initialize for any other reason.
    ///
    /// Wraps the tool-facing error unchanged, including pipe and handshake
    /// failures that happen after a successful spawn.
    Failed(ToolError),
}

/// One incoming message, classified by its JSON-RPC role.
///
/// A response carries an `id` and no `method`; a server request carries
/// both; a notification carries only a `method`. Misreading a server
/// request as a response desynchronizes the stream — the request's id
/// fails the pending match and the real response stays buffered — so the
/// roles are distinguished before anything is handed to the caller.
enum Incoming {
    /// A response to one of this client's requests.
    ///
    /// Carries the parsed id, result, and error members; the pending
    /// request's id is matched against it by the caller.
    Response(LspResponse),

    /// A request the server directed at the client.
    ///
    /// The id is the raw JSON value so any id shape is echoed faithfully
    /// in the reply.
    ServerRequest {
        /// The id to echo back in the reply.
        ///
        /// Copied verbatim from the incoming message rather than narrowed
        /// to a number, since JSON-RPC permits string ids.
        id: Value,

        /// The method the server asked the client to run.
        ///
        /// Logged when the request is declined; the client implements
        /// none of the server-side methods.
        method: String,
    },

    /// A notification — no id, nothing to answer.
    ///
    /// Progress and log messages arrive this way; they are consumed so
    /// the next read lands on a real response.
    Notification,
}

/// Classify one parsed message by its JSON-RPC role.
///
/// A message with a `method` member is a notification or a server request
/// regardless of what else it carries; only method-less, id-bearing
/// messages are responses. A message with neither member is not valid
/// JSON-RPC and classifies as a response with id `0`, which fails the
/// pending id match loudly rather than being skipped silently.
fn classify_incoming(json: &Value) -> Incoming {
    if let Some(method) = json.get("method").and_then(Value::as_str) {
        return match json.get("id") {
            Some(id) => Incoming::ServerRequest {
                id: id.clone(),
                method: method.to_string(),
            },
            None => Incoming::Notification,
        };
    }
    Incoming::Response(LspResponse {
        id: json
            .get("id")
            .and_then(Value::as_u64)
            .map_or(0, |id| u32::try_from(id).unwrap_or(0)),
        result: json.get("result").cloned(),
        error: json.get("error").and_then(|error| {
            Some(LspError {
                code: i32::try_from(error.get("code").and_then(Value::as_i64)?).unwrap_or(0),
                message: error.get("message").and_then(Value::as_str)?.to_string(),
            })
        }),
    })
}

/// The current per-request timeout budget as a `Duration`.
///
/// Read fresh at each request so a test tightening
/// [`REQUEST_TIMEOUT_SECS`] takes effect immediately.
fn request_timeout() -> Duration {
    Duration::from_secs(REQUEST_TIMEOUT_SECS.load(Ordering::SeqCst))
}

/// One JSON-RPC response read off the wire.
///
/// The parsed body of a single framed message that carries a request id.
/// Notifications never materialize as this type — they are consumed and
/// dropped while reading — so a response is always the answer to some
/// outstanding request.
struct LspResponse {
    /// The request id the response answers.
    ///
    /// Matched against the id the client sent; a mismatch means the
    /// framing desynchronized and fails the request.
    id: u32,

    /// The `result` member, when present.
    ///
    /// Its shape depends on the method being answered; callers deserialize
    /// it into the type that method promises, treating a deserialize
    /// failure as "no answer" rather than an error.
    result: Option<Value>,

    /// The `error` member, when the call failed.
    ///
    /// Carries the server's code and message so a rejected request
    /// surfaces the server's own diagnosis in the tool's output.
    error: Option<LspError>,
}

/// The `error` member of a failed JSON-RPC response.
///
/// Kept as plain data rather than an `lsp_types` error type because the
/// client reads responses as raw JSON; only these two members ever reach
/// a tool-facing error message.
struct LspError {
    /// The server's numeric error code.
    ///
    /// Standard JSON-RPC codes (for example `-32601`, method not found)
    /// or a server-defined code; reported verbatim next to the message.
    code: i32,

    /// The server's human-readable message.
    ///
    /// Surfaced unchanged in the tool's error output so the server's own
    /// explanation of the failure reaches the model.
    message: String,
}

/// A client bound to one live language-server process.
///
/// Requests are strictly sequential: each `send_request` writes the framed
/// request and reads the next framed response, matching it by id. That is
/// sufficient for the tool's one-operation-per-call shape and keeps the
/// framing code free of a response router.
pub struct LspClient {
    /// The server process, retained so its pipes stay open and so `Drop`
    /// can kill it.
    process: Child,

    /// The server's piped stdin — requests are written here.
    ///
    /// Owned by the client so the pipe stays open for the server's
    /// lifetime; dropping the client closes it, which a well-behaved
    /// server reads as shutdown.
    stdin: ChildStdin,

    /// The server's piped stdout — responses are read here.
    ///
    /// Buffered because the wire format is line-framed headers followed
    /// by an exact-length body; both are read through the buffer.
    stdout: BufReader<ChildStdout>,

    /// The next request id; ids start at 1 and increment per request.
    ///
    /// Atomic so every request gets a distinct id without borrowing the
    /// client; responses are matched against the sent id on read.
    request_id: AtomicU32,

    /// Documents published to this server, by URI string, with their
    /// version counter.
    ///
    /// The first open publishes the text via `didOpen`; every later call
    /// for the same document syncs the current text via a full-text
    /// `didChange` at the next version, so the server never answers a
    /// query from text the caller has since replaced.
    documents: HashMap<String, u32>,
}

impl LspClient {
    /// Start the configured server and complete the LSP initialization
    /// handshake for `root_uri`.
    ///
    /// The server is spawned with piped stdin/stdout/stderr; `root_uri`
    /// becomes the handshake's `rootUri`, so the server indexes the
    /// workspace this client is pooled under. Server stderr is drained
    /// into `tracing` (see [`drain_stderr`]) so diagnostics are recoverable
    /// from logs without ever reaching a foreground display.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError::BinaryMissing`] when the server binary is not
    /// on `PATH`, and [`SpawnError::Failed`] when the process cannot be
    /// spawned otherwise, its pipes cannot be taken, or the initialization
    /// exchange fails — including a server that answers with a position
    /// encoding other than UTF-8.
    pub async fn start(config: &LspServerConfig, root_uri: &Url) -> Result<Self, SpawnError> {
        let mut process = Command::new(&config.command)
            .args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SpawnError::BinaryMissing {
                        command: config.command.clone(),
                    }
                } else {
                    SpawnError::Failed(ToolError::Execution(format!(
                        "Failed to start LSP server '{}': {e}",
                        config.command
                    )))
                }
            })?;
        let stdin = process.stdin.take().ok_or_else(|| {
            SpawnError::Failed(ToolError::Execution(
                "Failed to get stdin for LSP server".to_string(),
            ))
        })?;
        let stdout = process.stdout.take().ok_or_else(|| {
            SpawnError::Failed(ToolError::Execution(
                "Failed to get stdout for LSP server".to_string(),
            ))
        })?;
        if let Some(stderr) = process.stderr.take() {
            tokio::spawn(drain_stderr(stderr));
        }
        let mut client = Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicU32::new(1),
            documents: HashMap::new(),
        };
        client
            .initialize(root_uri)
            .await
            .map_err(|e| SpawnError::Failed(e.into_error()))?;
        Ok(client)
    }

    /// Send a JSON-RPC request and read its response.
    ///
    /// The write and the read run under the per-request timeout budget (see
    /// [`REQUEST_TIMEOUT_SECS`]); any server requests that arrive while
    /// waiting are answered with an empty result and skipped, and
    /// notifications are consumed and dropped.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::Transport`] on any write, read, parse,
    /// timeout, or id-mismatch failure — the pooled client is suspect —
    /// and [`RequestError::Server`] when the response carries an error
    /// member, which leaves the client serviceable.
    async fn send_request(&mut self, method: String, params: Value) -> Result<Value, RequestError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": &method,
            "params": params
        });
        let exchange = async {
            write_framed(&mut self.stdin, &request)
                .await
                .map_err(RequestError::Transport)?;
            self.read_response().await.map_err(RequestError::Transport)
        };
        let response = match tokio::time::timeout(request_timeout(), exchange).await {
            Ok(outcome) => outcome?,
            Err(_) => {
                return Err(RequestError::Transport(ToolError::Execution(format!(
                    "LSP request '{method}' timed out after {} seconds",
                    REQUEST_TIMEOUT_SECS.load(Ordering::SeqCst)
                ))));
            }
        };
        if response.id != id {
            return Err(RequestError::Transport(ToolError::Execution(format!(
                "Response ID mismatch: expected {id}, got {}",
                response.id
            ))));
        }
        response.result.ok_or_else(|| {
            RequestError::Server(ToolError::Execution(match response.error {
                Some(e) => format!("LSP error: {} - {}", e.code, e.message),
                None => "LSP returned error with no message".to_string(),
            }))
        })
    }

    /// Read the framed JSON-RPC response for the pending request.
    ///
    /// The server interleaves notifications and its own requests with
    /// responses, so notifications are consumed and dropped and server
    /// requests are declined with an empty result (the client implements
    /// no server-side methods) until the response arrives.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] on header, framing, or JSON
    /// failures, and when declining a server request fails to write.
    async fn read_response(&mut self) -> Result<LspResponse, ToolError> {
        loop {
            match self.read_message().await? {
                Incoming::Notification => {}
                Incoming::ServerRequest { id, method } => {
                    tracing::debug!(target: "dch_tools::lsp", "declining server request '{method}'");
                    let reply = json!({"jsonrpc": "2.0", "id": id, "result": null});
                    write_framed(&mut self.stdin, &reply).await?;
                }
                Incoming::Response(response) => return Ok(response),
            }
        }
    }

    /// Read one framed message and classify it by JSON-RPC role.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] on header, framing, or JSON
    /// failures.
    async fn read_message(&mut self) -> Result<Incoming, ToolError> {
        let mut header_line = String::new();
        self.stdout
            .read_line(&mut header_line)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read LSP header: {e}")))?;
        let content_length = parse_content_length(&header_line)?;
        let mut blank_line = String::new();
        self.stdout
            .read_line(&mut blank_line)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read LSP blank line: {e}")))?;
        let mut buffer = vec![0u8; content_length];
        self.stdout
            .read_exact(&mut buffer)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read LSP response body: {e}")))?;
        let message_str = String::from_utf8(buffer).map_err(|e| {
            ToolError::Execution(format!("Failed to parse LSP response as UTF-8: {e}"))
        })?;
        let json: Value = serde_json::from_str(&message_str)
            .map_err(|e| ToolError::Execution(format!("Failed to parse LSP response JSON: {e}")))?;
        Ok(classify_incoming(&json))
    }

    /// Publish `url`'s content as an in-memory document, or sync it.
    ///
    /// Servers only answer position queries for documents they know, so
    /// the tool publishes each file's text before its first query. The
    /// first call sends `didOpen`; every later call for the same document
    /// sends a full-text `didChange` at the next version with the text as
    /// it stands now, so the server never answers from a version the
    /// caller has since replaced on disk.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::Transport`] when the notification write
    /// fails — a notification has no reply, so only the write can fail.
    pub async fn open_document(&mut self, url: &Url, text: &str) -> Result<(), RequestError> {
        let version = if let Some(open) = self.documents.get(url.as_str()) {
            let version = open.saturating_add(1);
            self.did_change(url, text, version).await?;
            version
        } else {
            self.did_open(url, text).await?;
            1
        };
        self.documents.insert(url.as_str().to_string(), version);
        Ok(())
    }

    /// Send the `textDocument/didOpen` notification for a first open.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::Transport`] when the write fails.
    async fn did_open(&mut self, url: &Url, text: &str) -> Result<(), RequestError> {
        let params = json!({
            "textDocument": {
                "uri": url.as_str(),
                "languageId": "rust",
                "version": 1,
                "text": text
            }
        });
        self.send_notification("textDocument/didOpen".to_string(), params)
            .await
            .map_err(RequestError::Transport)
    }

    /// Send a full-text `textDocument/didChange` for an open document.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::Transport`] when the write fails.
    async fn did_change(
        &mut self,
        url: &Url,
        text: &str,
        version: u32,
    ) -> Result<(), RequestError> {
        let params = json!({
            "textDocument": {
                "uri": url.as_str(),
                "version": version
            },
            "contentChanges": [{"text": text}]
        });
        self.send_notification("textDocument/didChange".to_string(), params)
            .await
            .map_err(RequestError::Transport)
    }

    /// Complete the LSP initialization handshake.
    ///
    /// Declares the client's hover capability, confirms `root_uri` as the
    /// workspace root, and negotiates UTF-8 position encoding — the client
    /// converts model-facing character offsets itself and refuses to run
    /// against a server that answers with any other encoding. Then it
    /// delivers the `initialized` notification that lets the server begin
    /// background work.
    ///
    /// The handshake disables the server's workspace code execution —
    /// build scripts, proc-macro expansion, and check-on-save — so a
    /// read-only query never runs the workspace's `build.rs` or
    /// proc-macro code. The accepted tradeoff: hover and definition
    /// fidelity degrades in proc-macro-heavy crates, since macro-generated
    /// code is not expanded.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError`] when either half of the handshake fails or
    /// the server's chosen position encoding is not UTF-8.
    async fn initialize(&mut self, root_uri: &Url) -> Result<(), RequestError> {
        let init_params = json!({
            "processId": null,
            "rootUri": root_uri.as_str(),
            "capabilities": {
                "general": {"positionEncodings": ["utf-8"]},
                "textDocument": {
                    "hover": {"contentFormat": ["markdown", "plaintext"]}
                }
            },
            "initializationOptions": {
                "cargo": {"buildScripts": {"enable": false}},
                "procMacro": {"enable": false},
                "checkOnSave": {"enable": false}
            }
        });
        let result = self
            .send_request("initialize".to_string(), init_params)
            .await?;
        confirm_utf8_positions(&result).map_err(RequestError::Server)?;
        self.send_notification("initialized".to_string(), json!({}))
            .await
            .map_err(RequestError::Transport)?;
        Ok(())
    }

    /// Send a JSON-RPC notification (no response expected).
    ///
    /// The write runs under the per-request timeout budget — a wedged
    /// server whose pipe buffer is full turns even a notification write
    /// into an unbounded wait.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when the write fails or times out.
    async fn send_notification(&mut self, method: String, params: Value) -> Result<(), ToolError> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": &method,
            "params": params
        });
        let write = write_framed(&mut self.stdin, &notification);
        match tokio::time::timeout(request_timeout(), write).await {
            Ok(outcome) => outcome,
            Err(_) => Err(ToolError::Execution(format!(
                "LSP notification '{method}' timed out after {} seconds",
                REQUEST_TIMEOUT_SECS.load(Ordering::SeqCst)
            ))),
        }
    }

    /// Hover information for a wire position in `url`.
    ///
    /// `position` is 0-indexed and expressed in the position encoding
    /// negotiated at initialize — UTF-8 byte offsets; the caller converts
    /// from its own units. A `None` result means the server had nothing at
    /// the position.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError`] — [`RequestError::Transport`] when the
    /// request cannot complete, [`RequestError::Server`] when the reply
    /// carries a JSON-RPC error member.
    pub async fn hover(
        &mut self,
        url: &Url,
        position: Position,
    ) -> Result<Option<Hover>, RequestError> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: document_uri(url).map_err(RequestError::Server)?,
                },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let result = self
            .send_request("textDocument/hover".to_string(), json!(params))
            .await?;
        Ok(serde_json::from_value(result).ok())
    }

    /// Definition locations for a wire position in `url`.
    ///
    /// As with [`hover`](Self::hover), `position` is 0-indexed in the
    /// negotiated encoding; `None` means no definition was found.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError`] — [`RequestError::Transport`] when the
    /// request cannot complete, [`RequestError::Server`] when the reply
    /// carries a JSON-RPC error member.
    pub async fn goto_definition(
        &mut self,
        url: &Url,
        position: Position,
    ) -> Result<Option<GotoDefinitionResponse>, RequestError> {
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: document_uri(url).map_err(RequestError::Server)?,
                },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let result = self
            .send_request("textDocument/definition".to_string(), json!(params))
            .await?;
        Ok(serde_json::from_value(result).ok())
    }
}

impl Drop for LspClient {
    /// Take the server down with the client.
    ///
    /// Best-effort backstop: a client dropped without a clean shutdown
    /// still kills its server process rather than leaking it. Errors are
    /// ignored — there is no caller left to report them to.
    fn drop(&mut self) {
        self.process.start_kill().ok();
    }
}

/// Write one framed JSON-RPC message to the server's stdin.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the write fails.
async fn write_framed(stdin: &mut ChildStdin, message: &Value) -> Result<(), ToolError> {
    let body = message.to_string();
    let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    stdin
        .write_all(framed.as_bytes())
        .await
        .map_err(|e| ToolError::Execution(format!("Failed to write to LSP server: {e}")))
}

/// Parse a `Content-Length: <n>\r\n` header line.
///
/// Values above [`MAX_MESSAGE_BYTES`] are refused before any allocation,
/// so a corrupt or hostile frame cannot commit the process to a
/// server-sized buffer.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the line is not a
/// `Content-Length` header, its value is not a non-negative integer, or it
/// exceeds the message cap.
fn parse_content_length(header_line: &str) -> Result<usize, ToolError> {
    let value = header_line
        .get("Content-Length:".len()..)
        .ok_or_else(|| {
            ToolError::Execution(
                "Invalid LSP header: missing value after Content-Length:".to_string(),
            )
        })?
        .trim();
    let length: usize = value
        .parse()
        .map_err(|e| ToolError::Execution(format!("Invalid Content-Length: {e}")))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(ToolError::Execution(format!(
            "LSP Content-Length {length} exceeds the {MAX_MESSAGE_BYTES}-byte message cap"
        )));
    }
    Ok(length)
}

/// Convert a file `Url` into the `lsp_types` document URI.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the URI cannot be represented.
fn document_uri(url: &Url) -> Result<lsp_types::Uri, ToolError> {
    lsp_types::Uri::from_str(url.as_str())
        .map_err(|e| ToolError::Execution(format!("Invalid URI: {e}")))
}

/// Whether the server accepted UTF-8 as the position encoding.
///
/// The caller converts model-facing character offsets into the negotiated
/// encoding's units and only implements UTF-8, so a server answering with
/// any other encoding — or none, which means the UTF-16 default — is
/// refused at the handshake rather than silently mis-targeting every
/// query on multi-byte lines.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] naming the refused encoding.
fn confirm_utf8_positions(result: &Value) -> Result<(), ToolError> {
    let negotiated = result
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("positionEncoding"))
        .and_then(Value::as_str);
    if negotiated == Some("utf-8") {
        return Ok(());
    }
    Err(ToolError::Execution(format!(
        "Language server requires unsupported position encoding {} (only utf-8 is supported)",
        negotiated.unwrap_or("utf-16")
    )))
}

/// Drain the server's stderr into `tracing`, line by line.
///
/// The pipe must be read or the server blocks once its buffer fills, and
/// the lines must not reach the process's own stderr — a full-screen
/// display would be corrupted by them — so they are logged at debug level
/// under the crate's LSP target. The task ends with the stream when the
/// server exits.
async fn drain_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "dch_tools::lsp", "language server: {line}");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
pub(crate) mod fakes {
    use serde_json::Value;

    use super::LspServerConfig;

    /// One framed JSON-RPC message, with its `Content-Length` computed
    /// from the body's byte length.
    pub(crate) fn frame(body: &Value) -> String {
        let text = body.to_string();
        format!("Content-Length: {}\r\n\r\n{text}", text.len())
    }

    /// A `sh` server script that prints `frames` verbatim, then runs
    /// `tail` (e.g. `sleep 2` to keep the pipes open, or a `cat` capture).
    ///
    /// Returns the script's temp home (keep it alive for the test) and the
    /// config that spawns it. Frames are printed with `printf '%s'` so
    /// nothing in the body is escape-interpreted.
    ///
    /// # Panics
    ///
    /// Panics when the temp directory or the script file cannot be
    /// created — fixture setup with no failure mode a test could react to.
    pub(crate) fn fake_server(
        frames: &[String],
        tail: &str,
    ) -> (tempfile::TempDir, LspServerConfig) {
        let home = tempfile::tempdir().expect("tempdir");
        let script = home.path().join("server.sh");
        let lines: Vec<String> = frames
            .iter()
            .map(|framed| format!("printf '%s' '{framed}'\n"))
            .collect();
        let mut body = lines.concat();
        body.push_str(tail);
        body.push('\n');
        std::fs::write(&script, body).expect("write script");
        let config = LspServerConfig {
            command: "sh".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            file_extensions: Vec::new(),
        };
        (home, config)
    }
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
    use crate::lsp::client::fakes::fake_server;
    use crate::lsp::client::fakes::frame;
    use serde_json::json;

    /// The initialize response every fake server starts with — the client
    /// validates the server's chosen position encoding from it.
    fn init_frame() -> String {
        frame(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"capabilities": {"positionEncoding": "utf-8"}}
        }))
    }

    #[test]
    fn content_length_headers_parse() {
        assert_eq!(parse_content_length("Content-Length: 42\r\n").unwrap(), 42);
        assert_eq!(parse_content_length("Content-Length: 0\r\n").unwrap(), 0);
    }

    #[test]
    fn malformed_headers_are_rejected() {
        assert!(parse_content_length("Content-Type: text/plain\r\n").is_err());
        assert!(parse_content_length("Content-Length: abc\r\n").is_err());
        assert!(parse_content_length("").is_err());
    }

    #[test]
    fn an_oversized_content_length_is_refused() {
        let err = parse_content_length("Content-Length: 16777217\r\n").unwrap_err();
        assert!(
            err.to_string().contains("exceeds"),
            "the server-controlled length must be capped: {err}"
        );
    }

    #[test]
    fn responses_classify_without_a_method_member() {
        let Incoming::Response(response) =
            classify_incoming(&json!({"jsonrpc": "2.0", "id": 7, "result": {"a": 1}}))
        else {
            panic!("an id-bearing, method-less message is a response");
        };
        assert_eq!(response.id, 7);
    }

    #[test]
    fn server_requests_classify_apart_from_responses() {
        let Incoming::ServerRequest { id, method } = classify_incoming(&json!({
            "jsonrpc": "2.0",
            "id": 9000,
            "method": "workspace/configuration"
        })) else {
            panic!("a message with both id and method is a server request");
        };
        assert_eq!(id, json!(9000));
        assert_eq!(method, "workspace/configuration");
    }

    #[test]
    fn notifications_classify_without_an_id() {
        let Incoming::Notification =
            classify_incoming(&json!({"jsonrpc": "2.0", "method": "window/logMessage"}))
        else {
            panic!("a method-only message is a notification");
        };
    }

    #[tokio::test]
    async fn initialize_disables_workspace_code_execution() {
        let capture = tempfile::tempdir().unwrap();
        let log = capture.path().join("wire.log");
        let (home, config) = fake_server(&[init_frame()], &format!("cat > '{}'", log.display()));
        let root = Url::from_file_path(home.path()).unwrap();
        let _client = LspClient::start(&config, &root).await.unwrap();
        for _ in 0..40 {
            let captured = std::fs::read_to_string(&log).unwrap_or_default();
            if captured.contains("initializationOptions") {
                assert!(
                    captured.contains("\"buildScripts\":{\"enable\":false}"),
                    "build scripts must be disabled: {captured}"
                );
                assert!(
                    captured.contains("\"procMacro\":{\"enable\":false}"),
                    "proc macros must be disabled: {captured}"
                );
                assert!(
                    captured.contains("\"checkOnSave\":{\"enable\":false}"),
                    "check-on-save must be disabled: {captured}"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the initialize request never disabled workspace code execution");
    }

    #[tokio::test]
    async fn a_silent_server_times_out_the_request() {
        let _gate = crate::lsp::pool::SPAWN_GATE.lock().await;
        REQUEST_TIMEOUT_SECS.store(1, Ordering::SeqCst);
        let (home, config) = fake_server(&[init_frame()], "sleep 30");
        let root = Url::from_file_path(home.path()).unwrap();
        let mut client = LspClient::start(&config, &root).await.unwrap();
        let started = std::time::Instant::now();
        let bounded = tokio::time::timeout(
            Duration::from_secs(8),
            client.hover(&root, Position::new(0, 0)),
        )
        .await;
        REQUEST_TIMEOUT_SECS.store(30, Ordering::SeqCst);
        let err = bounded
            .expect("hover must resolve inside the test bound")
            .unwrap_err();
        let message = err.into_error().to_string();
        assert!(
            message.contains("timed out"),
            "the timeout must surface as a typed error: {message}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the request must return promptly, not hang: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_server_request_mid_exchange_does_not_desync_the_client() {
        let (home, config) = fake_server(
            &[
                init_frame(),
                frame(&json!({
                    "jsonrpc": "2.0",
                    "id": 9000,
                    "method": "workspace/configuration",
                    "params": {"items": []}
                })),
                frame(&json!({"jsonrpc": "2.0", "id": 2, "result": null})),
            ],
            "sleep 2",
        );
        let root = Url::from_file_path(home.path()).unwrap();
        let mut client = LspClient::start(&config, &root).await.unwrap();
        client.open_document(&root, "fn one() {}\n").await.unwrap();
        let hover = client.hover(&root, Position::new(0, 0)).await.unwrap();
        assert!(hover.is_none());
    }

    #[tokio::test]
    async fn a_reopened_document_sends_a_did_change_with_the_current_text() {
        let capture = tempfile::tempdir().unwrap();
        let log = capture.path().join("wire.log");
        // A foreground `cat` captures the client's writes from the frames
        // onward and holds the pipe open for the rest of the exchange; a
        // backgrounded `cat` would read /dev/null under `sh`.
        let tail = format!("cat > '{}'", log.display());
        let (home, config) = fake_server(
            &[
                init_frame(),
                frame(&json!({"jsonrpc": "2.0", "id": 2, "result": null})),
            ],
            &tail,
        );
        let root = Url::from_file_path(home.path()).unwrap();
        let mut client = LspClient::start(&config, &root).await.unwrap();
        client.open_document(&root, "fn one() {}\n").await.unwrap();
        client.hover(&root, Position::new(0, 0)).await.unwrap();
        client.open_document(&root, "fn two() {}\n").await.unwrap();
        for _ in 0..40 {
            let captured = std::fs::read_to_string(&log).unwrap_or_default();
            if captured.contains("textDocument/didChange") && captured.contains("fn two()") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the reopened document was never synced with a didChange");
    }
}
