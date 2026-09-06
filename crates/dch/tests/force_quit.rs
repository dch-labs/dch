//! Interrupt behavior of the real binary at the process level.
//!
//! Spawns the built `dch` headless against a local server that never
//! answers, then delivers two SIGINTs. Either interrupt path may win the
//! race — the cooperative cancel unwinding `run` (writing its done-file on
//! the way out) or the forced exit (whose hook writes the done-file first)
//! — and both must leave the child dead with exit code 130 and a done-file
//! behind, never a lingering process or a signal-default death. The
//! first-vs-repeat classification itself is pinned by the signal module's
//! bridge-loop unit test; this test pins the process contract around it.

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
use std::process::{Command, Stdio};
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

fn wait_for(flag: &AtomicBool, what: &str) {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10))
        .expect("deadline computable");
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
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
        .stderr(Stdio::null())
        .spawn()
        .expect("child spawned");

    wait_for(&connected, "the child to reach the server");

    let pid = child.id().cast_signed();
    unsafe { libc::kill(pid, libc::SIGINT) };
    std::thread::sleep(Duration::from_millis(10));
    unsafe { libc::kill(pid, libc::SIGINT) };

    let deadline = Instant::now()
        .checked_add(Duration::from_secs(15))
        .expect("deadline computable");
    loop {
        if let Some(status) = child.try_wait().expect("child is waitable") {
            assert_eq!(
                status.code(),
                Some(130),
                "an interrupted run must exit 130, not {status}"
            );
            assert!(
                done_path.exists(),
                "every terminal path — force quit included — writes the done-file"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the child never exited after two interrupts"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
