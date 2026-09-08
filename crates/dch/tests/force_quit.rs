//! Interrupt behavior of the real binary at the process level.
//!
//! Two scenarios against the built `dch` headless. The first spawns it
//! against a local server that never answers, then repeats SIGINTs until
//! the run decides: either interrupt path may win the race — the
//! cooperative cancel unwinding `run` (writing its done-file on the way
//! out) or the forced exit (whose hook writes the done-file first) — and
//! both must leave the child dead with exit code 130 and a done-file
//! behind, never a lingering process or a signal-default death. The
//! second holds the child in its construction phase (a stdin prompt that
//! never completes) and pins the startup handler's path: the hook runs
//! and the process exits 130, instead of dying on the default
//! disposition with no marker for a polling orchestrator. The
//! first-vs-repeat classification itself is pinned by the signal
//! module's bridge-loop unit tests; these pin the process contracts
//! around them. Children run with captured stderr so an unexpected exit
//! is reported with the binary's own explanation.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::io::Read as _;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bind a server that accepts one connection, swallows the request, and
/// never responds.
///
/// The child's request therefore hangs mid-run until interrupted, which
/// is the state both interrupt paths are exercised from; the park keeps
/// the socket open for the child's whole short life.
fn holding_server() -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
    let port = listener.local_addr().expect("bound address").port();
    let connected = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&connected);
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("child connects");
        let mut sink = [0u8; 1024];
        let _read = stream.read(&mut sink);
        flag.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(120));
    });
    (port, connected)
}

/// Kills the child on drop so a failed assert cannot leak a live
/// `dch` process hanging on its server until the request timeout.
struct KillOnDrop<'a>(&'a mut Child);

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        self.0.kill().ok();
    }
}

impl std::ops::Deref for KillOnDrop<'_> {
    type Target = Child;

    fn deref(&self) -> &Child {
        self.0
    }
}

impl std::ops::DerefMut for KillOnDrop<'_> {
    fn deref_mut(&mut self) -> &mut Child {
        self.0
    }
}

/// The child's stderr, drained in the background as it is produced.
///
/// An early exit is only diagnosable with the binary's own explanation
/// (a config parse error, a usage error, a panic), so the reader thread
/// keeps the pipe empty and the capture snapshots whatever has arrived
/// whenever a failure wants to report it.
struct StderrCapture {
    /// Text read so far; the reader thread appends as bytes arrive.
    text: Arc<std::sync::Mutex<String>>,
}

impl StderrCapture {
    /// Take the child's piped stderr and drain it on a background thread.
    fn spawn(child: &mut Child) -> Self {
        let text = Arc::new(std::sync::Mutex::new(String::new()));
        let Some(mut stderr) = child.stderr.take() else {
            return Self {
                text: Arc::clone(&text),
            };
        };
        let sink = Arc::clone(&text);
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(read) = stderr.read(&mut buf) {
                if read == 0 {
                    break;
                }
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_str(&String::from_utf8_lossy(
                        buf.get(..read).unwrap_or_default(),
                    ));
            }
        });
        Self { text }
    }

    /// The stderr text captured so far.
    fn so_far(&self) -> String {
        self.text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn wait_until(child: &mut Child, stderr: &StderrCapture, what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10))
        .expect("deadline computable");
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        if let Some(status) = child.try_wait().expect("child is waitable") {
            panic!(
                "the child exited before {what}: {status}\nchild stderr:\n{}",
                stderr.so_far()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Repeat SIGINT while the outcome is undecided, then return the child's
/// exit status once it is dead.
///
/// The repeat is what makes the force path reachable. Both terminal
/// paths write the done-file immediately before exiting, so once it
/// exists the run is decided and signaling stops: a kill past that point
/// could only land on teardown, where delivery is no longer guaranteed.
fn force_until_exit(child: &mut Child, done_path: &Path) -> std::process::ExitStatus {
    let pid = child.id().cast_signed();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(15))
        .expect("deadline computable");
    loop {
        assert!(
            Instant::now() < deadline,
            "the child never exited after the repeated interrupts"
        );
        if !done_path.exists() {
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
        for _ in 0..10 {
            if child.try_wait().expect("child is waitable").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Some(status) = child.try_wait().expect("child is waitable") {
            return status;
        }
    }
}

#[test]
fn interrupts_exit_130_and_leave_a_done_file() {
    let (port, connected) = holding_server();
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("config.toml");
    let done_path = dir.path().join("done.json");
    std::fs::write(
        &config_path,
        format!(
            "[api]\napi_type = \"openai\"\nbase_url = \"http://127.0.0.1:{port}\"\
             \napi_key = \"dummy\"\nmodel = \"test-model\"\nrequest_timeout_secs = 30\n"
        ),
    )
    .expect("config written");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dch"))
        .arg("--headless")
        .arg("hello")
        .arg("--config")
        .arg(&config_path)
        .arg("--done-file")
        .arg(&done_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("child spawned");
    let stderr = StderrCapture::spawn(&mut child);
    let mut child = KillOnDrop(&mut child);

    wait_until(&mut child, &stderr, "the child to reach the server", || {
        connected.load(Ordering::SeqCst)
    });

    let status = force_until_exit(&mut child, &done_path);
    assert_eq!(
        status.code(),
        Some(130),
        "an interrupted run must exit 130, not {status}\nchild stderr:\n{}",
        stderr.so_far()
    );
    assert!(
        done_path.exists(),
        "every terminal path — force quit included — writes the done-file"
    );
}

/// A signal during the construction phase — here a stdin prompt that never
/// completes — must take the startup handler's path: the done-file hook
/// runs and the process exits 130, instead of dying on the default
/// disposition with no marker for a polling orchestrator.
#[test]
fn signal_during_startup_writes_the_done_file_and_exits_130() {
    let dir = tempfile::tempdir().expect("temp dir");
    let done_path = dir.path().join("done.json");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dch"))
        .arg("--headless")
        .arg("--done-file")
        .arg(&done_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("child spawned");
    let stderr = StderrCapture::spawn(&mut child);
    let mut child = KillOnDrop(&mut child);

    // No config file: prompt resolution reads the held-open stdin pipe and
    // blocks, keeping the child in its construction phase.
    //
    // The generous grace covers process start to listener registration;
    // a signal landing inside the (millisecond-scale) pre-registration
    // window would still kill by default disposition, which is the
    // residual flake this test cannot remove from outside.
    std::thread::sleep(Duration::from_millis(1500));
    if let Some(status) = child.try_wait().expect("child is waitable") {
        panic!(
            "the child exited before the startup signal — the prompt no \
             longer blocks: {status}\nchild stderr:\n{}",
            stderr.so_far()
        );
    }
    let pid = child.id().cast_signed();
    unsafe { libc::kill(pid, libc::SIGTERM) };

    let deadline = Instant::now()
        .checked_add(Duration::from_secs(15))
        .expect("deadline computable");
    loop {
        if let Some(status) = child.try_wait().expect("child is waitable") {
            assert_eq!(
                status.code(),
                Some(130),
                "a startup signal must exit 130, not {status}\nchild stderr:\n{}",
                stderr.so_far()
            );
            assert!(
                done_path.exists(),
                "the startup handler writes the done-file before exiting"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the child never exited after the startup signal"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
