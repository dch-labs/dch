//! Shared fixtures for the integration suites.
//!
//! One canned-SSE provider endpoint standing in for the model, one
//! isolated HOME standing in for the user's, and spawners for the real
//! `dch` binary against both — every scripted scenario, automated or
//! terminal-driven, is the same shape: script the model's turns as
//! response bodies, run the binary, assert on stdout, the exit code,
//! the session store under the isolated HOME, and the effects in the
//! workdir.

#![allow(dead_code)]

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

/// One canned HTTP response the server hands to the next connection.
struct CannedResponse {
    /// The `Content-Type` header value the response carries.
    ///
    /// The scripted body decides it: `text/event-stream` for provider
    /// turns, anything else for a plain fixture a tool fetches.
    content_type: String,

    /// The response body, verbatim on the wire.
    ///
    /// For SSE responses this is the full `data:`-event stream the
    /// provider client parses, exactly as a scripted model would send
    /// it.
    body: String,
}

/// A canned provider endpoint on an ephemeral local port.
///
/// Serves one response body per accepted connection, in the order they
/// were scripted, and records every request body it receives — the
/// recordings are how tests assert on what the model was shown (tool
/// results, resumed history, block messages) without touching the
/// engine's internals. Each response closes the connection, so the
/// provider client's keep-alive pool never couples two scripted turns
/// to one socket.
pub struct CannedServer {
    /// The port the endpoint listens on.
    ///
    /// Handed to the sandbox's config file so the spawned binary's
    /// provider client dials this server and nothing else.
    port: u16,

    /// Every request body received so far, in arrival order.
    ///
    /// Shared with the accept thread, which appends as requests land;
    /// tests snapshot it to assert on what the model was shown.
    requests: Arc<Mutex<Vec<String>>>,
}

impl CannedServer {
    /// Start a server that answers each connection with one SSE turn
    /// body, in order, and reports run-dry once the script is spent.
    pub fn sse(bodies: Vec<String>) -> Self {
        Self::start(
            bodies
                .into_iter()
                .map(|body| CannedResponse {
                    content_type: "text/event-stream".to_string(),
                    body,
                })
                .collect(),
            false,
            std::time::Duration::ZERO,
        )
    }

    /// Start an SSE server that sleeps `delay` before each answer.
    ///
    /// For scenarios that must catch a run in flight — a cancel landing
    /// mid-stream needs the stream to still be pending when the
    /// interrupt arrives.
    pub fn sse_delayed(bodies: Vec<String>, delay: std::time::Duration) -> Self {
        Self::start(
            bodies
                .into_iter()
                .map(|body| CannedResponse {
                    content_type: "text/event-stream".to_string(),
                    body,
                })
                .collect(),
            false,
            delay,
        )
    }

    /// Start a server that answers every connection with one plain
    /// document, forever.
    ///
    /// For fixtures a tool fetches (a web page) rather than the
    /// provider stream itself; the unbounded repetition distinguishes
    /// it from the scripted SSE server, which runs dry on purpose.
    pub fn plain(content_type: &str, body: &str) -> Self {
        Self::start(
            vec![CannedResponse {
                content_type: content_type.to_string(),
                body: body.to_string(),
            }],
            true,
            std::time::Duration::ZERO,
        )
    }

    fn start(
        responses: Vec<CannedResponse>,
        repeat_last: bool,
        delay: std::time::Duration,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
        let port = listener.local_addr().expect("bound address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            let mut queue = std::collections::VecDeque::from(responses);
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                if let Some(request) = read_request(&mut stream)
                    && let Ok(mut log) = recorded.lock()
                {
                    log.push(request);
                }
                let served = queue.front().map(|response| {
                    if delay > std::time::Duration::ZERO {
                        std::thread::sleep(delay);
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        response.content_type,
                        response.body.len()
                    );
                    stream.write_all(head.as_bytes()).ok();
                    stream.write_all(response.body.as_bytes()).ok();
                });
                if served.is_none() {
                    stream
                        .write_all(
                            b"HTTP/1.1 500 No canned response\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        )
                        .ok();
                }
                if !repeat_last {
                    queue.pop_front();
                }
            }
        });
        Self { port, requests }
    }

    /// The port the canned endpoint listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The request bodies received so far, in arrival order.
    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Read one HTTP request off the stream and return its body.
///
/// Headers are consumed line by line until the blank separator; the
/// `Content-Length` that follows names the body's bytes. A request the
/// test binary never finishes writing yields `None` and the connection
/// is answered with the server's run-dry response.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).ok()?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).to_string())
}

/// An isolated environment for one acceptance scenario.
///
/// Owns the canned provider endpoint, a throwaway HOME (so session
/// storage and any config-derived state land nowhere real), and a
/// workdir the spawned binary runs in — tools read and write there, so
/// their effects are the assertions. The config file written at
/// construction points `[api]` at the canned endpoint; every spawner
/// passes it, so the binary never reads a user config.
/// Resolve the `dch` binary a scenario drives.
///
/// The default is the binary cargo builds alongside the test — the
/// debug-profile one under `cargo test`. Setting `DCH_BIN` to another
/// binary points every spawner at it, which is how `make release-check`
/// drives the acceptance suite against the freshly built release
/// binary. The value must be an absolute path: a test's working
/// directory is the package root, not the directory the variable was
/// set from.
pub fn dch_binary() -> PathBuf {
    std::env::var_os("DCH_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dch")), PathBuf::from)
}

pub struct Sandbox {
    /// The scripted provider endpoint this scenario runs against.
    ///
    /// Owned so the accept thread outlives every spawned binary — a
    /// two-run scenario (save, then resume) drains the same script.
    server: CannedServer,

    /// The throwaway HOME the binary runs with.
    ///
    /// Doubles as `XDG_CONFIG_HOME`; session storage lands under
    /// `.dch/sessions` here, where a test can read it and nothing real
    /// is touched.
    home: tempfile::TempDir,

    /// The directory the binary and its tools operate in.
    ///
    /// Spawned as the child's working directory, so relative tool paths
    /// and the filesystem effects a scenario asserts on all land here.
    workdir: tempfile::TempDir,

    /// The config file pointing `[api]` at the canned endpoint.
    ///
    /// Written at construction and passed as `--config` by every
    /// spawner, so no scenario ever reads a user config.
    config_path: PathBuf,
}

impl Sandbox {
    /// Build the sandbox around a scripted provider stream.
    pub fn sse(bodies: Vec<String>) -> Self {
        Self::with_server(CannedServer::sse(bodies))
    }

    /// Build the sandbox around an already-started server.
    pub fn with_server(server: CannedServer) -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let workdir = tempfile::tempdir().expect("workdir tempdir");
        let config_path = home.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[api]\napi_type = \"openai\"\nbase_url = \"http://127.0.0.1:{}\"\
                 \napi_key = \"dummy\"\nmodel = \"test-model\"\nrequest_timeout_secs = 30\n",
                server.port()
            ),
        )
        .expect("config written");
        Self {
            server,
            home,
            workdir,
            config_path,
        }
    }

    /// The workdir the spawned binary and its tools operate in.
    pub fn workdir(&self) -> &Path {
        self.workdir.path()
    }

    /// The sessions directory under the isolated HOME.
    pub fn sessions_dir(&self) -> PathBuf {
        self.home.path().join(".dch").join("sessions")
    }

    /// The recorded provider request bodies so far.
    pub fn requests(&self) -> Vec<String> {
        self.server.requests()
    }

    /// Run the real binary to completion with `args` and no stdin.
    ///
    /// The workhorse of the automated scenarios: the child runs with
    /// the sandbox's isolated HOME and workdir, its config pointing at
    /// the scripted endpoint, and stdout/stderr captured for the
    /// caller's assertions.
    pub fn dch(&self, args: &[&str]) -> Output {
        self.dch_args(args, None)
    }

    /// Run the real binary to completion with `args` and a piped stdin.
    ///
    /// Selects the same headless dispatch a shell pipe would, for
    /// scenarios that exercise the stdin path rather than the task
    /// argument.
    pub fn dch_stdin(&self, args: &[&str], piped: &str) -> Output {
        self.dch_args(args, Some(piped))
    }

    fn dch_args(&self, args: &[&str], stdin: Option<&str>) -> Output {
        use std::process::Command;
        let mut command = Command::new(dch_binary());
        command
            .args(args)
            .arg("--config")
            .arg(&self.config_path)
            .current_dir(self.workdir.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("dch spawns");
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("stdin pipe");
            pipe.write_all(text.as_bytes()).expect("stdin written");
        }
        child.wait_with_output().expect("dch runs to completion")
    }

    /// Run the real binary on a 100×40 pseudoterminal and hand back the
    /// session's reader, writer, and child.
    ///
    /// The TUI only starts when no task argument is given and stdin is a
    /// terminal — a PTY is both. Scenarios type into the composer, read
    /// what renders, and watch the child exit.
    pub fn dch_pty(&self, args: &[&str]) -> PtySession {
        use portable_pty::CommandBuilder;
        use portable_pty::PtySize;
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 100,
                ..PtySize::default()
            })
            .expect("pty opens");
        let mut command = CommandBuilder::new(dch_binary());
        command.args(args);
        command.arg("--config");
        command.arg(&self.config_path);
        command.cwd(self.workdir.path());
        command.env("HOME", self.home.path());
        command.env("XDG_CONFIG_HOME", self.home.path());
        command.env("TERM", "xterm-truecolor");
        let child = pty
            .slave
            .spawn_command(command)
            .expect("dch spawns on the pty");
        let reader = pty.master.try_clone_reader().expect("pty reader clones");
        let writer = pty.master.take_writer().expect("pty writer taken");
        PtySession::new(child, reader, writer)
    }
}

/// A live pseudoterminal session with the binary on the other end.
///
/// A background thread drains the PTY into a shared buffer so waits
/// never block on reads; [`Self::expect`] polls that buffer for a
/// needle, [`Self::send`] writes keystrokes, and [`Self::finish`]
/// reaps the child under a deadline.
pub struct PtySession {
    /// The binary running on the pseudoterminal.
    ///
    /// Polled by [`Self::finish`](PtySession::finish) and its quit
    /// variant rather than blocking-waited, so a child that will not
    /// exit reports its screen under a deadline instead of hanging the
    /// harness.
    child: Box<dyn portable_pty::Child + Send>,

    /// The keystroke side of the terminal.
    ///
    /// Everything a scenario types — prompts, answers, the quit chord —
    /// goes through here as raw bytes.
    writer: Box<dyn Write + Send>,

    /// Everything the terminal has rendered so far.
    ///
    /// Appended by the background reader thread; waits poll a snapshot
    /// of it rather than reading the stream themselves.
    screen: Arc<Mutex<String>>,
}

impl PtySession {
    /// Assemble the session and start draining the terminal.
    ///
    /// The reader thread owns the PTY's read half for the session's
    /// whole life so the shared buffer keeps growing whatever the
    /// waits are doing; the child and writer halves stay with the
    /// session for reaping and keystrokes.
    fn new(
        child: Box<dyn portable_pty::Child + Send>,
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
    ) -> Self {
        let screen = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&screen);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 4096];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                if let Ok(mut buffer) = sink.lock()
                    && let Some(chunk) = chunk.get(..read)
                {
                    buffer.push_str(&String::from_utf8_lossy(chunk));
                }
            }
        });
        Self {
            child,
            writer,
            screen,
        }
    }

    /// Wait until the rendered output contains `needle`.
    ///
    /// The match runs over the screen with escape sequences stripped
    /// and accepts the needle's words in order — a terminal renderer
    /// emits styled text as separate segments with cursor moves
    /// between them, so a contiguous phrase never appears verbatim on
    /// the wire. Returns a snapshot of everything rendered so far; a
    /// timeout panics with the snapshot so the failure reports what the
    /// screen actually showed.
    pub fn expect(&self, needle: &str, timeout: std::time::Duration) -> String {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .expect("deadline computable");
        loop {
            let snapshot = self.snapshot();
            if screen_contains(&strip_ansi(&snapshot), needle) {
                return snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pty never showed {needle:?}; screen so far:\n{snapshot}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Everything rendered on the terminal so far.
    pub fn snapshot(&self) -> String {
        self.screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Write raw bytes to the terminal — keystrokes for the app.
    pub fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("keystrokes written");
        self.writer.flush().expect("keystrokes flushed");
    }

    /// Reap the child before `timeout` runs out, killing it if it lingers.
    ///
    /// Returns the status the child produced, or [`None`] once the
    /// deadline passes — after killing the child, so a session that
    /// outlives its deadline never leaks a process. [`Self::finish`]
    /// turns the [`None`] into the failure it is, with the final screen
    /// in the panic message.
    pub fn reap_within(
        &mut self,
        timeout: std::time::Duration,
    ) -> Option<portable_pty::ExitStatus> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .expect("deadline computable");
        loop {
            if let Some(status) = self.child.try_wait().expect("child is waitable") {
                return Some(status);
            }
            if std::time::Instant::now() >= deadline {
                // A kill racing the child's own exit loses; a follow-up
                // reap collects whichever status lands.
                if let Err(err) = self.child.kill() {
                    tracing::warn!(error = %err, "lingering pty child could not be killed");
                }
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Reap the child under `timeout`, killing it if it lingers.
    ///
    /// Returns the exit status the child actually produced; a session
    /// that outlives the deadline is killed first and the panic reports
    /// the final screen, since a TUI that will not quit is the failure.
    pub fn finish(mut self, timeout: std::time::Duration) -> portable_pty::ExitStatus {
        self.reap_within(timeout).unwrap_or_else(|| {
            panic!(
                "the child never exited; screen so far:\n{}",
                self.snapshot()
            )
        })
    }

    /// Quit the TUI and reap it: send Ctrl-C, keep sending while the
    /// child lives, and return its exit status.
    ///
    /// The exit chord is stateful — a press that cancels a run or
    /// clears a draft consumes itself, and the app's own hint says to
    /// press again — so the driver repeats the press on a cadence until
    /// the process is gone rather than guessing which press lands.
    pub fn quit_and_finish(mut self, timeout: std::time::Duration) -> portable_pty::ExitStatus {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .expect("deadline computable");
        loop {
            self.send(b"\x03");
            for _ in 0..20 {
                if let Some(status) = self.child.try_wait().expect("child is waitable") {
                    return status;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited on repeated ctrl-c; screen so far:\n{}",
                self.snapshot()
            );
        }
    }
}

/// Strip ANSI escape sequences from a raw terminal stream.
///
/// Covers the sequences a ratatui/crossterm TUI emits: CSI parameter
/// strings (cursor moves, colors, mode switches, each ending in a byte
/// from `@` to `~`) and OSC strings (each ending at BEL or the `ESC \`
/// string terminator — the terminal answers its color queries with the
/// latter). Anything the stripper leaves is printable text.
fn strip_ansi(raw: &str) -> String {
    let mut clean = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(char) = chars.next() {
        if char != '\x1b' {
            clean.push(char);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for inner in chars.by_ref() {
                    if ('@'..='~').contains(&inner) {
                        break;
                    }
                }
            }
            Some(']') => {
                for inner in chars.by_ref() {
                    if inner == '\x07' {
                        break;
                    }
                    if inner == '\x1b' {
                        // The other OSC terminator: ESC \. Consume the
                        // backslash half so it cannot read as text.
                        if let Some('\\') = chars.clone().next() {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    clean
}

/// Whether a stripped screen shows `needle`'s words, in order.
///
/// Each word must appear (case-insensitively) after the previous
/// word's match, which tolerates the renderer's segmentation while
/// still pinning the phrase's content and order.
fn screen_contains(stripped: &str, needle: &str) -> bool {
    let haystack = stripped.to_lowercase();
    let mut search_from = 0;
    for word in needle.split_whitespace() {
        let word = word.to_lowercase();
        match haystack
            .get(search_from..)
            .and_then(|rest| rest.find(&word))
        {
            Some(at) => {
                search_from = search_from.saturating_add(at).saturating_add(word.len());
            }
            None => return false,
        }
    }
    true
}

/// An SSE body carrying one plain text answer.
///
/// Streams the text as one content delta and closes the turn with a
/// `stop` finish reason, then the `[DONE]` sentinel — the shape a
/// model produces when it has nothing more to call and only words to
/// say.
pub fn sse_text_turn(text: &str) -> String {
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

/// An SSE body issuing one tool call with fully-inline arguments.
///
/// Splits the call across two deltas — the function name first, the
/// arguments second — because that is how models actually stream tool
/// calls, and closes with a `tool_calls` finish reason so the engine
/// dispatches before the next scripted turn runs.
pub fn sse_tool_call_turn(tool: &str, args: &serde_json::Value) -> String {
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

/// Whether a rust-analyzer binary is reachable.
///
/// The LSP scenario skips — passes vacuously — where the binary is not
/// installed, which includes the CI image; running it is a local-gate
/// concern.
pub fn rust_analyzer_available() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Initialize a git repository in `dir` with two committed baselines and
/// one staged-but-uncommitted change.
///
/// Two commits make `HEAD~1` resolve, and the staged change is what a
/// submission reports — `git diff` never sees untracked files, so the
/// fixture stages rather than merely writes it.
pub fn git_fixture(dir: &Path) {
    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .status()
            .expect("git spawns");
        assert!(status.success(), "git {args:?} failed: {status}");
    }
    std::fs::write(dir.join("committed.txt"), "baseline\n").expect("baseline written");
    git(dir, &["init", "--initial-branch=main"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "baseline"]);
    std::fs::write(dir.join("second.txt"), "second\n").expect("second written");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "second"]);
    std::fs::write(dir.join("staged.txt"), "staged change\n").expect("staged written");
    git(dir, &["add", "staged.txt"]);
}
