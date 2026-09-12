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
///
/// The kill is followed by a wait: the reaped child neither zombies for
/// the rest of the harness run nor holds the stderr pipe open for the
/// capture thread.
struct KillOnDrop<'a>(&'a mut Child);

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
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
    /// Text read so far.
    ///
    /// Appended by the reader thread as chunks arrive, and snapshotted
    /// under the same lock by [`so_far`](Self::so_far).
    text: Arc<std::sync::Mutex<String>>,
}

impl StderrCapture {
    /// Take the child's piped stderr and drain it on a background thread.
    ///
    /// The reader keeps the pipe empty for the child's whole life, so a
    /// chatty child can never fill it and block; whatever has arrived is
    /// available to a failure report at any moment through
    /// [`so_far`](Self::so_far).
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
    ///
    /// A snapshot taken under the capture's lock — the reader thread may
    /// append more the moment this returns.
    fn so_far(&self) -> String {
        self.text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The terminating signal of an exited child, if it died by one.
///
/// A signal death carries no exit code, only the signal — the startup
/// test uses that distinction to recognize the one pre-registration
/// death it tolerates and retries on a fresh child.
fn unix_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
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
/// A kill is only sent to a child proven alive, and a failed one ends
/// the signaling — a reaped child's pid must never be signaled again,
/// since the kernel may have handed it to another process.
fn force_until_exit(
    child: &mut Child,
    done_path: &Path,
    stderr: &StderrCapture,
) -> std::process::ExitStatus {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(15))
        .expect("deadline computable");
    loop {
        assert!(
            Instant::now() < deadline,
            "the child never exited after the repeated interrupts\nchild stderr:\n{}",
            stderr.so_far()
        );
        if !done_path.exists() && child.try_wait().expect("child is waitable").is_none() {
            let pid = child.id().cast_signed();
            let delivered = unsafe { libc::kill(pid, libc::SIGINT) } == 0;
            if delivered {
                for _ in 0..10 {
                    if child.try_wait().expect("child is waitable").is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        if let Some(status) = child.try_wait().expect("child is waitable") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(50));
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

    let status = force_until_exit(&mut child, &done_path, &stderr);
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

#[test]
fn signal_during_startup_writes_the_done_file_and_exits_130() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let done_path = dir.path().join("done.json");
    std::fs::write(&done_path, "stale marker of a previous run").expect("stale marker written");
    std::fs::set_permissions(&done_path, std::fs::Permissions::from_mode(0o600))
        .expect("restrictive mode set");
    let (status, stderr) = startup_signal_outcome(&done_path, dir.path(), false);
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
    let written = std::fs::read_to_string(&done_path).expect("the replaced marker is readable");
    let record: serde_json::Value = serde_json::from_str(&written).expect(
        "the startup handler wrote a fresh done record — the stale marker of a \
        previous run would leave exists() and the mode check passing vacuously",
    );
    assert_eq!(
        record.get("success"),
        Some(&serde_json::json!(false)),
        "a startup cancel records a failure"
    );
    assert_eq!(
        record.get("message"),
        Some(&serde_json::json!("cancelled during startup")),
        "the fresh record carries the startup-cancel message"
    );
    let mode = std::fs::metadata(&done_path)
        .expect("the replaced marker exists")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "the startup hook restores the cleared marker's restrictive mode"
    );
}

#[test]
fn an_immediate_startup_signal_exits_130_with_the_done_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let done_path = dir.path().join("done.json");
    let (status, stderr) = startup_signal_outcome(&done_path, dir.path(), true);
    assert_eq!(
        status.code(),
        Some(130),
        "an immediate signal must still exit 130, not {status}\nchild stderr:\n{}",
        stderr.so_far()
    );
    assert!(
        done_path.exists(),
        "every arm of the retry leaves the done-file behind"
    );
}

/// Drive the startup-signal scenario through its retry loop and return
/// the terminal status with the captured stderr of the deciding attempt.
///
/// Each attempt runs with no config file and an isolated HOME, so prompt
/// resolution reads the held-open stdin pipe and blocks, keeping the
/// child in its construction phase. The grace covers process start to
/// listener registration; a signal landing inside that
/// (millisecond-scale) window still kills by default disposition — the
/// flake this harness can only tolerate from outside. Two distinct
/// races are budgeted separately so a loaded runner cannot spend them
/// on one another: the provoked pre-registration race (the immediate
/// variant signals at spawn; the kill lands after exec but before
/// listener registration — near-certain death, so each death retries
/// on a fresh child from its budget, and the attempt proceeds without
/// the at-spawn signal once the budget is spent) and the slow-start
/// race (registration outlasting the fixed grace, one retry). The
/// immediate variant therefore pins the death-tolerance machinery —
/// provoked default-disposition deaths are retried and the run still
/// ends 130 with a marker — not that the at-spawn signal itself was
/// handled: the parent's kill beats the child's registration by
/// orders of magnitude, so a handled at-spawn exit is not an outcome
/// this harness can reach on purpose.
fn startup_signal_outcome(
    done_path: &Path,
    home: &Path,
    early_signal: bool,
) -> (std::process::ExitStatus, StderrCapture) {
    let mut provoked_retries_left: usize = 3;
    let mut slow_start_retries_left: usize = 1;
    'attempt: loop {
        let mut spawned = Command::new(env!("CARGO_BIN_EXE_dch"))
            .arg("--headless")
            .arg("--done-file")
            .arg(done_path)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("child spawned");
        let stderr = StderrCapture::spawn(&mut spawned);
        let mut child = KillOnDrop(&mut spawned);
        let sent_early = early_signal && provoked_retries_left > 0;
        if sent_early {
            let pid = child.id().cast_signed();
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        std::thread::sleep(Duration::from_millis(1500));
        match child.try_wait().expect("child is waitable") {
            Some(status)
                if unix_signal(status) == Some(libc::SIGTERM)
                    && sent_early
                    && provoked_retries_left > 0 =>
            {
                provoked_retries_left = provoked_retries_left.saturating_sub(1);
                continue;
            }
            Some(status)
                if unix_signal(status) == Some(libc::SIGTERM)
                    && !sent_early
                    && slow_start_retries_left > 0 =>
            {
                slow_start_retries_left = slow_start_retries_left.saturating_sub(1);
                continue;
            }
            // An early signal the child had time to register for is a
            // handled exit, not a broken premise — the deciding status.
            Some(status) if early_signal && status.code() == Some(130) => {
                break 'attempt (status, stderr);
            }
            Some(status) => panic!(
                "the child exited before the startup signal — the prompt no \
                 longer blocks: {status}\nchild stderr:\n{}",
                stderr.so_far()
            ),
            None => {}
        }
        let pid = child.id().cast_signed();
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(15))
            .expect("deadline computable");
        loop {
            match child.try_wait().expect("child is waitable") {
                Some(status)
                    if unix_signal(status) == Some(libc::SIGTERM)
                        && slow_start_retries_left > 0 =>
                {
                    slow_start_retries_left = slow_start_retries_left.saturating_sub(1);
                    break;
                }
                Some(status) => {
                    break 'attempt (status, stderr);
                }
                None => assert!(
                    Instant::now() < deadline,
                    "the child never exited after the startup signal\nchild stderr:\n{}",
                    stderr.so_far()
                ),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
