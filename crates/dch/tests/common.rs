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
///
/// The scripted stream is a queue of these: each arriving connection
/// takes the head entry whole — its hold first, then its bytes — so a
/// scenario spells a provider's turns out as a list. When the queue
/// runs dry the server answers 500 and closes, which is how a
/// scripted stream ends.
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

    /// How long the server sleeps before answering with this body.
    ///
    /// Zero for an immediate answer; a positive hold keeps the
    /// request in flight for scenarios that need to catch the binary
    /// mid-turn — a kill or a cancel landing before the response
    /// arrives.
    delay: std::time::Duration,
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
                    delay: std::time::Duration::ZERO,
                })
                .collect(),
            false,
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
                    delay,
                })
                .collect(),
            false,
        )
    }

    /// Start an SSE server where every scripted body carries its own
    /// pre-answer hold.
    ///
    /// For scenarios that mix pacing: early turns answer instantly
    /// while one later turn is held long enough for a signal — a
    /// kill or a cancel — to land before the response arrives. The
    /// uniform-delay twin cannot express that; its hold applies to
    /// every answer alike.
    pub fn sse_scheduled(bodies: Vec<(String, std::time::Duration)>) -> Self {
        Self::start(
            bodies
                .into_iter()
                .map(|(body, delay)| CannedResponse {
                    content_type: "text/event-stream".to_string(),
                    body,
                    delay,
                })
                .collect(),
            false,
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
                delay: std::time::Duration::ZERO,
            }],
            true,
        )
    }

    /// Start an SSE server whose non-stream requests are answered with
    /// one fixed JSON completion instead of the scripted queue.
    ///
    /// For strict-compactor scenarios: the conversation turns parse
    /// their SSE bodies from the queue while the summarizer's
    /// non-stream call lands a body it can actually parse — the queue
    /// is never spent on a compactor connection, so the script stays
    /// aligned with the turns it was written for.
    pub fn sse_with_nonstream(bodies: Vec<String>, nonstream: String) -> Self {
        Self::start_with_nonstream(
            bodies
                .into_iter()
                .map(|body| CannedResponse {
                    content_type: "text/event-stream".to_string(),
                    body,
                    delay: std::time::Duration::ZERO,
                })
                .collect(),
            CannedResponse {
                content_type: "application/json".to_string(),
                body: nonstream,
                delay: std::time::Duration::ZERO,
            },
        )
    }

    fn start(responses: Vec<CannedResponse>, repeat_last: bool) -> Self {
        Self::start_split(responses, repeat_last, None)
    }

    fn start_with_nonstream(responses: Vec<CannedResponse>, nonstream: CannedResponse) -> Self {
        Self::start_split(responses, false, Some(nonstream))
    }

    fn start_split(
        responses: Vec<CannedResponse>,
        repeat_last: bool,
        nonstream: Option<CannedResponse>,
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
                let mut logged = String::new();
                let mut naming = false;
                if let Some(request) = read_request(&mut stream) {
                    logged.clone_from(&request);
                    naming = request.contains(dch_loop::NAME_SYSTEM_PROMPT);
                    if !naming && let Ok(mut log) = recorded.lock() {
                        log.push(request);
                    }
                }
                if naming {
                    let body = serde_json::json!({
                        "id": "title", "model": "test-model",
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": "Canned Session Title"
                            },
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(head.as_bytes()).ok();
                    stream.write_all(body.as_bytes()).ok();
                    continue;
                }
                if logged.contains("\"stream\":false")
                    && let Some(response) = nonstream.as_ref()
                {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        response.content_type,
                        response.body.len()
                    );
                    if response.delay > std::time::Duration::ZERO {
                        std::thread::sleep(response.delay);
                    }
                    stream.write_all(head.as_bytes()).ok();
                    stream.write_all(response.body.as_bytes()).ok();
                    continue;
                }
                let served = queue.front().map(|response| {
                    if response.delay > std::time::Duration::ZERO {
                        std::thread::sleep(response.delay);
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
    ///
    /// The listener took an ephemeral port at start-up, so a scenario
    /// never chooses one; the value is what the sandbox's written
    /// config already points `[api]` at, kept public for the rare
    /// scenario that wants to address the endpoint itself.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The request bodies received so far, in arrival order.
    ///
    /// A snapshot clone under the lock, so a scenario can read it
    /// while the server thread keeps accepting; the order is the
    /// order the connections arrived in, which is the order the
    /// scripted turns ran.
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

/// Resolve the `dch` binary a scenario drives from an optional override.
///
/// Pure on purpose: scenarios run in parallel inside one test process,
/// so a test that mutated the environment to check the override would
/// bleed into concurrently spawning scenarios. `None` yields the
/// binary cargo built alongside the test — the debug-profile one under
/// `cargo test` — and `Some` yields exactly the given path, which is
/// how `make release-check` drives the acceptance suite against the
/// freshly built release binary. An override must be an absolute path:
/// a test's working directory is the package root, not the directory
/// the variable was set from.
pub fn resolve_binary(override_path: Option<std::ffi::OsString>) -> PathBuf {
    override_path.map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dch")), PathBuf::from)
}

/// The binary this process's spawners drive.
///
/// Reads the `DCH_BIN` override, if any, once per spawn and hands it
/// to [`resolve_binary`]; see that function for the contract.
pub fn dch_binary() -> PathBuf {
    resolve_binary(std::env::var_os("DCH_BIN"))
}

/// An isolated environment for one acceptance scenario.
///
/// Owns the canned provider endpoint, a throwaway HOME (so session
/// storage and any config-derived state land nowhere real), and a
/// workdir the spawned binary runs in — tools read and write there, so
/// their effects are the assertions. The config file written at
/// construction points `[api]` at the canned endpoint; every spawner
/// passes it, so the binary never reads a user config.
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
    ///
    /// Each body becomes one `text/event-stream` response the canned
    /// server hands out connection by connection, in the order given
    /// — the shape every provider-turn scenario stands on: the bodies
    /// are the model's turns, and the run ends when the script runs
    /// dry.
    pub fn sse(bodies: Vec<String>) -> Self {
        Self::with_server(CannedServer::sse(bodies))
    }

    /// Build the sandbox around an already-started server.
    ///
    /// The constructor every builder funnels through: it allocates
    /// the isolated home and workdir, then writes the config file
    /// that points `[api]` at the given endpoint, so every spawn from
    /// this sandbox talks to the script and nothing else.
    pub fn with_server(server: CannedServer) -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let workdir = tempfile::tempdir().expect("workdir tempdir");
        let config_path = home.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[api]\napi_type = \"openai\"\nbase_url = \"http://127.0.0.1:{}\"\
                 \napi_key = \"dummy\"\nmodel = \"test-model\"\nrequest_timeout_secs = 30\n\
                 context_window = 200000\n",
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
    ///
    /// The same tempdir every spawner passes as the child's current
    /// directory: relative paths a scenario writes before the run and
    /// the filesystem effects it asserts on after both resolve here.
    pub fn workdir(&self) -> &Path {
        self.workdir.path()
    }

    /// The sessions directory under the isolated HOME.
    ///
    /// The path the spawned binary saves sessions into —
    /// `.dch/sessions` under the sandbox's home — so a scenario finds
    /// its save files without hard-coding the layout the binary
    /// settles on.
    pub fn sessions_dir(&self) -> PathBuf {
        self.home.path().join(".dch").join("sessions")
    }

    /// The recorded provider request bodies so far.
    ///
    /// The sandbox's window onto [`CannedServer::requests`]: what the
    /// binary sent the scripted model, in arrival order, for
    /// scenarios that assert on the wire rather than the screen.
    pub fn requests(&self) -> Vec<String> {
        self.server.requests()
    }

    /// Append `extra` to the sandbox's config file.
    ///
    /// Scenarios that need runner-side knobs — a compactor selection,
    /// a small context window — write them here after construction,
    /// as more `[section]` TOML the binary reads alongside the
    /// endpoint wiring already present.
    pub fn extend_config(&self, extra: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.config_path)
            .expect("config opens for append");
        file.write_all(extra.as_bytes()).expect("config extended");
    }

    /// Build a sandbox around a delayed scripted stream with extra
    /// `[api]` and `[runner]` lines.
    ///
    /// The delayed twin of [`sse_configured`](Self::sse_configured):
    /// the server sleeps `delay` before every answer, so a scenario
    /// can catch the interface mid-flight — a compaction pass holding
    /// its notice row — on the server's own clock instead of racing
    /// the binary's timing. As with the plain twin, no window is
    /// declared here: `api_extra` carries one.
    pub fn sse_delayed_configured(
        bodies: Vec<String>,
        delay: std::time::Duration,
        api_extra: &str,
        runner_extra: &str,
    ) -> Self {
        let server = CannedServer::sse_delayed(bodies, delay);
        let home = tempfile::tempdir().expect("home tempdir");
        let workdir = tempfile::tempdir().expect("workdir tempdir");
        let config_path = home.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[api]\napi_type = \"openai\"\nbase_url = \"http://127.0.0.1:{}\"\
                 \napi_key = \"dummy\"\nmodel = \"test-model\"\nrequest_timeout_secs = 30\n\
                 {api_extra}\n[runner]\n{runner_extra}\n",
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

    /// Build a delayed sandbox whose non-stream requests land one
    /// fixed JSON completion.
    ///
    /// The strict-compactor twin of
    /// [`sse_delayed_configured`](Self::sse_delayed_configured): the
    /// hold applies to the summarizer's call too, so a compaction pass
    /// spans the delay and the live lane paints while it runs.
    pub fn sse_delayed_nonstream_configured(
        bodies: Vec<String>,
        delay: std::time::Duration,
        nonstream: String,
        api_extra: &str,
        runner_extra: &str,
    ) -> Self {
        let server = CannedServer::start_with_nonstream(
            bodies
                .into_iter()
                .map(|body| CannedResponse {
                    content_type: "text/event-stream".to_string(),
                    body,
                    delay,
                })
                .collect(),
            CannedResponse {
                content_type: "application/json".to_string(),
                body: nonstream,
                delay,
            },
        );
        Self::configured(server, api_extra, runner_extra)
    }

    /// Build a sandbox whose config carries extra `[api]` and
    /// `[runner]` lines.
    ///
    /// The sandbox's own `[api]` table ends with `api_extra` and a
    /// `[runner]` table follows with `runner_extra`, so a scenario
    /// tunes knobs TOML refuses to define twice — the context window,
    /// the compactor selection — without reopening a table. Unlike
    /// the plain sandbox, no window is declared here: `api_extra`
    /// carries one, keeping the spawned binary from probing the
    /// scripted endpoint and shifting its responses.
    pub fn sse_configured(bodies: Vec<String>, api_extra: &str, runner_extra: &str) -> Self {
        Self::configured(CannedServer::sse(bodies), api_extra, runner_extra)
    }

    /// The scheduled twin of [`sse_configured`](Self::sse_configured).
    ///
    /// Every body carries its own pre-answer hold, so a scenario can
    /// pace its early turns instantly while one later turn is held
    /// for a signal to land — against the same tuned config knobs the
    /// plain twin takes.
    pub fn sse_scheduled_configured(
        bodies: Vec<(String, std::time::Duration)>,
        api_extra: &str,
        runner_extra: &str,
    ) -> Self {
        Self::configured(CannedServer::sse_scheduled(bodies), api_extra, runner_extra)
    }

    /// The scheduled twin of
    /// [`sse_delayed_nonstream_configured`](Self::sse_delayed_nonstream_configured).
    ///
    /// Every scripted SSE body keeps its own hold while the
    /// summarizer's non-stream call lands the fixed JSON completion
    /// instantly — a compaction the resume assertions need to have
    /// already finished.
    pub fn sse_scheduled_nonstream_configured(
        bodies: Vec<(String, std::time::Duration)>,
        nonstream: String,
        api_extra: &str,
        runner_extra: &str,
    ) -> Self {
        let server = CannedServer::start_with_nonstream(
            bodies
                .into_iter()
                .map(|(body, delay)| CannedResponse {
                    content_type: "text/event-stream".to_string(),
                    body,
                    delay,
                })
                .collect(),
            CannedResponse {
                content_type: "application/json".to_string(),
                body: nonstream,
                delay: std::time::Duration::ZERO,
            },
        );
        Self::configured(server, api_extra, runner_extra)
    }

    /// Assemble the sandbox around a started server and tuned config
    /// tables.
    ///
    /// The shared body of the configured constructors: the endpoint
    /// arrives already started, the config file is written with the
    /// caller's extra lines, and no default window is declared.
    fn configured(server: CannedServer, api_extra: &str, runner_extra: &str) -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let workdir = tempfile::tempdir().expect("workdir tempdir");
        let config_path = home.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[api]\napi_type = \"openai\"\nbase_url = \"http://127.0.0.1:{}\"\
                 \napi_key = \"dummy\"\nmodel = \"test-model\"\nrequest_timeout_secs = 30\n\
                 {api_extra}\n[runner]\n{runner_extra}\n",
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

    /// Run the real binary with `extra` in the child's environment.
    ///
    /// The route for a variable a scenario needs the *binary* to see —
    /// `RUST_LOG` for the logging pins. Setting it on this process
    /// instead is not an option the harness supports: the write races
    /// every sibling test's reads of the same environment — `DCH_BIN` at
    /// each spawn through [`dch_binary`], `TMPDIR` inside the
    /// `tempfile::tempdir()` calls that build each sandbox — and these
    /// tests run on many threads with no lock to hold across them. A
    /// variable passed here reaches only the child this call starts.
    pub fn dch_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.dch_args_env(args, None, extra)
    }

    fn dch_args(&self, args: &[&str], stdin: Option<&str>) -> Output {
        self.dch_args_env(args, stdin, &[])
    }

    fn dch_args_env(&self, args: &[&str], stdin: Option<&str>, extra: &[(&str, &str)]) -> Output {
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
        for (key, value) in extra {
            command.env(key, value);
        }
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
        self.dch_pty_in_env(args, &[])
    }

    /// Run the real binary on a pseudoterminal under an injected
    /// environment.
    ///
    /// The same seat [`Self::dch_pty`] builds, with the environment a
    /// scenario needs to stand the binary in: a multiplexer's
    /// spellings — `TMUX`, a `tmux-…` terminal type — for pins over
    /// the launch-time decisions the shell reads from its
    /// environment. The helper's own multiplexer spellings are
    /// removed first, so a scenario that injects none stands the
    /// binary in a direct terminal whatever shell the suite runs in;
    /// an entry whose key the base already carries overrides it, so a
    /// scenario naming `TERM` decides the terminal type the binary
    /// sees.
    pub fn dch_pty_in_env(&self, args: &[&str], env: &[(&str, &str)]) -> PtySession {
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
        for spelling in ["TMUX", "STY", "ZELLIJ"] {
            command.env_remove(spelling);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .expect("dch spawns on the pty");
        let reader = pty.master.try_clone_reader().expect("pty reader clones");
        let writer = pty.master.take_writer().expect("pty writer taken");
        PtySession::new(child, pty.master, reader, writer)
    }
}

/// A live pseudoterminal session with the binary on the other end.
///
/// A background thread drains the PTY into a shared buffer so waits
/// never block on reads; [`Self::expect`] polls that buffer for a
/// needle, [`Self::send`] writes keystrokes, [`Self::finish`] reaps
/// the child under a deadline, and the background writers — the input
/// flood, the query-answer watcher — take the master's side lane so a
/// blocked background write never holds the keystroke lock.
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
    /// Everything a scenario types — prompts, answers, the quit
    /// chord — goes through here as raw bytes, behind the lock that
    /// keeps one keystroke whole. The background writers owe this
    /// lock nothing: they write through the master's side lane, so a
    /// background write blocked on a full input buffer cannot stall
    /// the scenario's next keystroke.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,

    /// The control side of the pseudoterminal.
    ///
    /// The descriptor the side lanes duplicate belongs to it, and it
    /// keeps the pty's control side open for the session's life — a
    /// lane may outlive the master through its own duplicate, but the
    /// session never closes the master a running lane duplicated from.
    master: Box<dyn portable_pty::MasterPty + Send>,

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
    /// waits are doing; the child, master, and writer halves stay
    /// with the session for reaping, side lanes, and keystrokes.
    fn new(
        child: Box<dyn portable_pty::Child + Send>,
        master: Box<dyn portable_pty::MasterPty + Send>,
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
            writer: Arc::new(Mutex::new(writer)),
            screen,
            master,
        }
    }

    /// Answer the binary's background query like a terminal that
    /// implements it, the moment the query leaves the pane.
    ///
    /// The startup exchange gives the reply a window of a few hundred
    /// milliseconds; a scenario that waited for the screen before
    /// replying would always miss it. This parks a watcher on the
    /// stream instead: when `OSC 11 ; ?` appears, the watcher writes
    /// the reply — `rgb` in the wire's own `rrrr/gggg/bbbb` spelling —
    /// from a second writer on the same pty, racing the query the way a
    /// real terminal does. One reply per query for `lifetime`, then the
    /// watcher stands down.
    pub fn answer_background_query(&self, rgb: &str, lifetime: std::time::Duration) {
        let screen = Arc::clone(&self.screen);
        let mut lane = self.side_write_lane();
        let reply = format!("\x1b]11;rgb:{rgb}\x1b\\").into_bytes();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now()
                .checked_add(lifetime)
                .expect("deadline computable");
            let mut answered = 0_usize;
            while std::time::Instant::now() < deadline {
                let seen = screen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .matches("\x1b]11;?")
                    .count();
                if seen > answered {
                    if let Err(err) = lane.write_all(&reply) {
                        tracing::warn!(error = %err, "background reply could not be written");
                    } else if let Err(err) = lane.flush() {
                        tracing::warn!(error = %err, "background reply could not be flushed");
                    }
                    answered = seen;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
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
    ///
    /// A clone of the shared buffer the background reader thread
    /// appends to, taken under its lock, so the call never blocks on
    /// the terminal itself. The bytes are the raw stream — escapes
    /// included — so a pin over the wire's own shapes reads this;
    /// word-level expectations go through [`expect`](Self::expect),
    /// which strips the escapes and matches the words in order.
    pub fn snapshot(&self) -> String {
        self.screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Write raw bytes to the terminal — keystrokes for the app.
    ///
    /// One `write_all` and a flush under the keystroke lock, so the
    /// bytes arrive as a burst the reader cannot split from a
    /// keystroke's own tail. A scenario typing a command and its
    /// return sends them as two calls, the way a keyboard delivers
    /// them, and the flood lane writes through its own side path — a
    /// flood blocked on a full input buffer can never stall this one.
    pub fn send(&mut self, bytes: &[u8]) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.write_all(bytes).expect("keystrokes written");
        writer.flush().expect("keystrokes flushed");
    }

    /// Hold the keystroke lane for as long as the guard lives.
    ///
    /// A fixture for pins over the background writers' discipline:
    /// with the lane held nothing the scenario types can proceed, so
    /// a flood that still runs to its deadline proves it owes the
    /// lane nothing.
    pub fn keystroke_lane(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The write lane a background thread of this session writes
    /// through.
    ///
    /// The master's own descriptor wherever the platform names one,
    /// duplicated into the lane so the blocking a full input buffer
    /// imposes stalls the lane's thread alone; the keystroke lane
    /// itself where no descriptor exists, the only writer that
    /// platform offers. [`flood_input`](Self::flood_input) and the
    /// [`answer_background_query`](Self::answer_background_query)
    /// watcher both write here.
    fn side_write_lane(&self) -> SideLane {
        self.descriptor_lane()
            .unwrap_or_else(|| SideLane::Keystrokes(Arc::clone(&self.writer)))
    }

    /// The master's descriptor as an owned write lane, where the
    /// platform names one.
    ///
    /// `None` when the master exposes no raw descriptor, leaving the
    /// keystroke lane as the only write lane.
    #[cfg(unix)]
    fn descriptor_lane(&self) -> Option<SideLane> {
        let fd = self.master.as_raw_fd()?;
        // SAFETY: the descriptor belongs to `self.master` and is only
        // borrowed for the duplication — the wrapper never closes it,
        // and the lane that returns owns its own duplicate.
        let borrowed = std::mem::ManuallyDrop::new(unsafe {
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd)
        });
        borrowed.try_clone().ok().map(SideLane::Descriptor)
    }

    /// The descriptor lane's absence off unix, where the master
    /// names no raw descriptor.
    ///
    /// The side lane falls back to the keystroke writer there, the
    /// one write lane that platform carries.
    #[cfg(not(unix))]
    fn descriptor_lane(&self) -> Option<SideLane> {
        None
    }

    /// Type `bytes` at the session continuously for `duration`, from a
    /// thread of its own.
    ///
    /// The counterpart of a key held down: the stream never pauses
    /// long enough for a reader to catch the input empty, which is
    /// what a launch-time window that stands down on waiting input
    /// needs to be driven through — a paced trickle always leaves the
    /// gap one final drain can win. The writes pace themselves by the
    /// pty alone — `write_all` blocks while the child's input is full
    /// and resumes as it drains — so the stream outpaces the session
    /// by no more than the kernel's buffer; the deadline bounds the
    /// loop, not a blocked write, and the thread finishes only once
    /// its last write drains. The lane is the session's side lane,
    /// never the keystroke lock: a flood blocked on a full input
    /// buffer owes the scenario's next keystroke nothing. The bytes
    /// are the scenario's to choose — something the session consumes
    /// without editing the composer or asking for a frame — and the
    /// returned handle lets a scenario wait the flood out before its
    /// next keystroke, so a submit never races the backlog.
    pub fn flood_input(
        &self,
        bytes: &[u8],
        duration: std::time::Duration,
    ) -> std::thread::JoinHandle<()> {
        let mut lane = self.side_write_lane();
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now()
                .checked_add(duration)
                .expect("deadline computable");
            while std::time::Instant::now() < deadline {
                if lane.write_all(&bytes).is_err() || lane.flush().is_err() {
                    return;
                }
            }
        })
    }

    /// The child's process id, for tests that deliver signals.
    ///
    /// The signal scenarios aim their `kill` at the running session
    /// the way a user's terminal would, so they need the id while the
    /// child is live; it stays valid until the session's reaping
    /// takes the child back.
    pub fn pid(&self) -> u32 {
        self.child.process_id().expect("child reports its pid")
    }

    /// Reap without signaling: the exit status once the child is dead.
    ///
    /// The signal-delivery scenarios own the kill side and must observe
    /// the status the signal path produced, so unlike
    /// [`reap_within`](PtySession::reap_within) this never kills a
    /// lingering child.
    pub fn try_reap(&mut self) -> Option<portable_pty::ExitStatus> {
        self.child.try_wait().expect("child is waitable")
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

    /// Cancel the run in flight the way a user does: two Esc presses
    /// with a beat between them.
    ///
    /// Two escape bytes in one write parse as a single key at the
    /// terminal layer, so the presses go out separately — the same
    /// rhythm a keyboard delivers.
    pub fn cancel_run(&mut self) {
        self.send(b"\x1b");
        std::thread::sleep(std::time::Duration::from_millis(150));
        self.send(b"\x1b");
    }

    /// Quit the TUI and reap it: send Ctrl-C, keep sending while the
    /// child lives, and return its exit status.
    ///
    /// The exit chord is stateful — a press that cancels a run or
    /// clears a draft consumes itself, and the app's own hint says to
    /// press again — so the driver repeats the press on a cadence until
    /// the process is gone rather than guessing which press lands.
    pub fn quit_and_finish(mut self, timeout: std::time::Duration) -> portable_pty::ExitStatus {
        self.quit_until_gone(timeout)
    }

    /// Quit the TUI, reap it, and hand back what the terminal showed.
    ///
    /// The session's exit lines print *after* the terminal is restored,
    /// so a scenario asserting them needs the drained buffer rather than
    /// the exit status alone. Reaping does not mean the bytes arrived:
    /// the reader thread is still moving the child's last output into
    /// the screen, so the wait is settled through [`settle_screen`] and
    /// is best-effort — capped at [`SCREEN_STABLE_CAP`], returning what
    /// arrived if a process keeps writing past it. Strip the result with
    /// [`strip_ansi`] before matching words.
    pub fn quit_and_read(
        mut self,
        timeout: std::time::Duration,
    ) -> (portable_pty::ExitStatus, String) {
        let status = self.quit_until_gone(timeout);
        let screen = settle_screen(&|| self.snapshot(), SCREEN_STABLE_CAP, SCREEN_POLL);
        (status, screen)
    }

    /// Press the exit chord on a cadence until the child is gone.
    ///
    /// Shared by [`Self::quit_and_finish`] and [`Self::quit_and_read`];
    /// see the former for why the press is repeated rather than guessed.
    fn quit_until_gone(&mut self, timeout: std::time::Duration) -> portable_pty::ExitStatus {
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

/// A write lane to the session's pty that a background thread owns.
///
/// The descriptor form carries its own duplicate of the master's
/// file descriptor: it writes the same input stream the keystroke
/// writer does, without sharing its lock, so a write that blocks on a
/// full input buffer blocks its thread alone. The keystroke form is
/// the fallback where the platform names no descriptor — the shared
/// writer behind its mutex, with the sharing that implies.
enum SideLane {
    /// The master's descriptor, duplicated; owned and closed by the
    /// lane alone.
    Descriptor(std::fs::File),

    /// The keystroke writer, for platforms without a raw master
    /// descriptor to duplicate.
    Keystrokes(Arc<Mutex<Box<dyn Write + Send>>>),
}

impl Write for SideLane {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            SideLane::Descriptor(file) => file.write(buf),
            SideLane::Keystrokes(writer) => writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            SideLane::Descriptor(file) => file.flush(),
            SideLane::Keystrokes(writer) => writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flush(),
        }
    }
}

/// How long a settled screen must hold still before it counts as done.
///
/// The ceiling of the wait behind `settle_screen`: a reaped child's
/// last bytes usually land in milliseconds, and anything still
/// arriving after half a second is returned as far as it got rather
/// than gating the scenario.
pub const SCREEN_STABLE_CAP: std::time::Duration = std::time::Duration::from_millis(500);

/// The gap between two reads of the screen buffer.
///
/// The cadence `settle_screen` re-reads at: long enough that a
/// growing buffer has a beat to grow on, short enough that the
/// stable verdict arrives promptly after the last byte.
pub const SCREEN_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// The number of unchanged reads that settle the screen.
///
/// Two, because one unchanged read proves nothing on its own — two
/// reads with no sleep between them agree just as readily on an empty
/// buffer, which is exactly how a late trailing line would slip past
/// a single-read verdict.
const STABLE_READS: usize = 2;

/// Read until the screen stops growing, then hand it back.
///
/// A reaped child is not a drained one: the reader thread is still
/// moving its last bytes into the buffer when the process is gone, so
/// the exit lines a scenario asserts on can still be in flight. Every
/// read is taken after `poll` has passed and the buffer must come back
/// unchanged `STABLE_READS` times running to count as settled — two
/// reads with no sleep between them agree just as readily on an empty
/// buffer, which is how a late trailing line gets missed. The wait is
/// capped at `cap` and best-effort: output still arriving when it
/// expires is returned as far as it got.
pub fn settle_screen(
    read: &dyn Fn() -> String,
    cap: std::time::Duration,
    poll: std::time::Duration,
) -> String {
    let mut screen = read();
    let Some(deadline) = std::time::Instant::now().checked_add(cap) else {
        return screen;
    };
    let mut unchanged = 0usize;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(poll);
        let next = read();
        if next == screen {
            unchanged = unchanged.saturating_add(1);
            if unchanged >= STABLE_READS {
                return next;
            }
        } else {
            unchanged = 0;
            screen = next;
        }
    }
    screen
}

/// Strip ANSI escape sequences from a raw terminal stream.
///
/// Covers the sequences a ratatui/crossterm TUI emits: CSI parameter
/// strings (cursor moves, colors, mode switches, each ending in a byte
/// from `@` to `~`) and OSC strings (each ending at BEL or the `ESC \`
/// string terminator — the terminal answers its color queries with the
/// latter). Anything the stripper leaves is printable text.
pub fn strip_ansi(raw: &str) -> String {
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

/// The first index `needle` occurs at inside `haystack`, if it does.
///
/// Slices have no stable subslice search, and the HTTP framing this
/// module strips is all fixed byte sequences, so one tiny finder
/// serves every split.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// How long any discovery probe may spend connecting or reading.
///
/// The probes back the smoke suite's model discovery against
/// endpoints nobody controls: one that accepts a connection but never
/// answers must not hold discovery hostage behind it, so every probe
/// fails on this bound and discovery moves on to its next candidate.
const PROBE_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// One bounded connection to `host:port`, trying every address the
/// host resolves to.
///
/// A name can list an unreachable address ahead of a reachable one —
/// `localhost` with its `IPv6` entry first is the ordinary case — so
/// each candidate gets its own connect timeout and the first that
/// answers wins. `None` means every attempt failed or the name
/// resolved to nothing at all.
fn connect_bounded(host: &str, port: u16) -> Option<TcpStream> {
    let addresses: Vec<std::net::SocketAddr> = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
        Err(_) => std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
            .ok()?
            .collect(),
    };
    addresses
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, PROBE_BOUND).ok())
}

/// One minimal HTTP GET, returning the response body.
///
/// Speaks just enough HTTP for the model-listing endpoints the smoke
/// discovery reads, against plain-`http` endpoints; a non-200 status,
/// a connection failure, or a body that never arrives is a `None` the
/// caller treats as "not listed here". Chunked transfer encoding —
/// what Ollama actually answers with — is stripped before the body
/// returns.
/// Issue one small POST and return its 200 body, dechunked if needed.
///
/// The Ollama control endpoints (`/api/show` among them) take the
/// query as a JSON body rather than on the path, so the discovery
/// probe needs the POST half beside the GET helper.
fn http_post_body(host: &str, port: u16, path: &str, body: &str) -> Option<String> {
    let mut stream = connect_bounded(host, port)?;
    stream.set_read_timeout(Some(PROBE_BOUND)).ok()?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let head_end = find_subslice(&raw, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(raw.get(..head_end)?).to_lowercase();
    let body = raw.get(head_end.checked_add(4)?..)?;
    if !(head.starts_with("http/1.1 200") || head.starts_with("http/1.0 200")) {
        return None;
    }
    if head.contains("transfer-encoding: chunked") {
        Some(String::from_utf8_lossy(&dechunk(body)).to_string())
    } else {
        Some(String::from_utf8_lossy(body).to_string())
    }
}

fn http_get_body(host: &str, port: u16, path: &str) -> Option<String> {
    let mut stream = connect_bounded(host, port)?;
    stream.set_read_timeout(Some(PROBE_BOUND)).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let head_end = find_subslice(&raw, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(raw.get(..head_end)?).to_lowercase();
    let body = raw.get(head_end.checked_add(4)?..)?;
    if !(head.starts_with("http/1.1 200") || head.starts_with("http/1.0 200")) {
        return None;
    }
    if head.contains("transfer-encoding: chunked") {
        Some(String::from_utf8_lossy(&dechunk(body)).to_string())
    } else {
        Some(String::from_utf8_lossy(body).to_string())
    }
}

/// Strip HTTP chunked-transfer framing from a response body.
///
/// Each chunk carries its byte length as a hex line ahead of the data,
/// terminated by a zero-length chunk. The sizes are byte counts, so the
/// strip runs on bytes — a chunk boundary may fall inside a multi-byte
/// character, and only the joined result is decoded.
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = find_subslice(rest, b"\r\n") {
        let size_end = at.saturating_add(2);
        let Some((size_line, remainder)) = rest.get(..at).zip(rest.get(size_end..)) else {
            break;
        };
        let Ok(size) = usize::from_str_radix(String::from_utf8_lossy(size_line).trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let Some((data, tail)) = remainder.split_at_checked(size) else {
            break;
        };
        out.extend_from_slice(data);
        rest = tail.strip_prefix(b"\r\n").unwrap_or(tail);
    }
    out
}

/// Split a plain-`http` base URL into host, port, and path prefix.
///
/// The trailing `/v1` the provider config conventionally carries is
/// dropped, so both `http://host:port` and `http://host:port/v1`
/// address the same root; a URL without a port assumes 80. Anything
/// not starting `http://` is not addressable here.
fn parse_http_base(base_url: &str) -> Option<(String, u16, String)> {
    let rest = base_url.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    let path = path.trim_end_matches('/');
    let prefix = if path.is_empty() || path == "v1" {
        String::new()
    } else {
        format!("/{path}")
    };
    Some((host.to_string(), port, prefix))
}

/// Pull the model identifiers out of one listing shape.
///
/// Ollama's native listing carries them under `models` keyed by
/// `name`; the OpenAI-compatible listing under `data` keyed by `id`.
/// Any other shape reads as an empty list.
fn model_names(listing: &serde_json::Value, array_key: &str, id_key: &str) -> Vec<String> {
    listing
        .get(array_key)
        .and_then(|entries| entries.as_array())
        .map_or_default(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get(id_key))
                .filter_map(|id| id.as_str())
                .map(String::from)
                .collect()
        })
}

/// Ask a provider endpoint which models it offers, preferring a coder build.
///
/// Reads Ollama's native `/api/tags` first, then the
/// OpenAI-compatible `/models` listing, both derived from the base
/// URL; a model whose name carries `coder` wins over the rest, and
/// otherwise the first listed model is the answer. `None` means the
/// endpoint offered nothing usable — the smoke case then fails fast
/// and names `DCH_SMOKE_MODEL` as the remedy.
pub fn discover_model(base_url: &str) -> Option<String> {
    let (host, port, prefix) = parse_http_base(base_url)?;
    let ollama = http_get_body(&host, port, &format!("{prefix}/api/tags"))
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .map(|listing| model_names(&listing, "models", "name"));
    let openai = http_get_body(&host, port, &format!("{prefix}/models"))
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .map(|listing| model_names(&listing, "data", "id"));
    let names = ollama.or(openai)?;
    let chats: Vec<String> = names
        .iter()
        .filter(|name| chats_at(&host, port, prefix.as_str(), name.as_str()))
        .cloned()
        .collect();
    if chats.is_empty() {
        return names
            .iter()
            .find(|name| name.contains("coder"))
            .cloned()
            .or_else(|| names.first().cloned());
    }
    chats
        .iter()
        .find(|name| name.contains("coder"))
        .cloned()
        .or_else(|| chats.first().cloned())
}

/// Whether the model at `base_url`'s Ollama answers chat turns.
///
/// Asks `/api/show` for the model's capabilities and looks for
/// `"chat"` among them; an unanswered or malformed show reads as not
/// chat-capable, so the caller's fallback chain still gets its turn.
fn chats_at(host: &str, port: u16, prefix: &str, model: &str) -> bool {
    let request = format!("{{\"model\":\"{model}\"}}");
    let Some(body) = http_post_body(host, port, &format!("{prefix}/api/show"), &request) else {
        return false;
    };
    let Ok(shown) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    shown
        .get("capabilities")
        .and_then(|caps| caps.as_array())
        .is_some_and(|caps| {
            caps.iter()
                .any(|cap| cap.as_str() == Some("completion") || cap.as_str() == Some("chat"))
        })
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

/// An SSE body whose turn reasons first, then answers.
///
/// One thinking delta chunk, one content delta chunk, then the stop
/// chunk — the shape whose display the final-answer pins ride on.
pub fn sse_thinking_text_turn(thinking: &str, text: &str) -> String {
    [
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": {"reasoning_content": thinking}, "finish_reason": null}]
        }),
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

/// An SSE body whose turn reasons first, then issues one tool call.
///
/// The marathon shape before a final answer: a thinking delta chunk,
/// the tool-call deltas, and a `tool_calls` finish so the engine
/// dispatches and the loop continues.
pub fn sse_thinking_tool_turn(thinking: &str, tool: &str, args: &serde_json::Value) -> String {
    let args = args.to_string();
    [
        serde_json::json!({
            "id": "c1", "model": "test-model",
            "choices": [{"delta": {"reasoning_content": thinking}, "finish_reason": null}]
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_endpoint_fails_each_probe_within_its_bound() {
        // The endpoint that accepts and never answers: a probe without
        // a read bound waits the socket out and discovery stalls behind
        // it. Both halves of the discovery leg — the listing GET and
        // the capability POST — fail within the probe bound and read
        // as "not listed here" instead.
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
        let port = listener
            .local_addr()
            .expect("a bound socket has an address")
            .port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for _ in 0..2 {
                let Ok((socket, _)) = listener.accept() else {
                    break;
                };
                held.push(socket);
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
        let started = std::time::Instant::now();
        let posted = http_post_body("127.0.0.1", port, "/api/show", "{\"model\":\"x\"}");
        assert!(
            posted.is_none(),
            "a response that never arrives reads as absent"
        );
        let fetched = http_get_body("127.0.0.1", port, "/v1/models");
        assert!(fetched.is_none(), "the GET leg fails the same way");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "each probe fails within its bound rather than waiting the socket out"
        );
    }
}
