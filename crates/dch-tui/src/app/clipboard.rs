//! The clipboard's plumbing: the host transports the copy servo
//! walks in order, their process bounds, and the verdicts each
//! attempt reports.

use super::copies::copy_via_transports;
use super::selection::take_locked;
use ratatui::text::Line;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long all of one copy's process transports may run in total.
///
/// One clock covers the configured command, the platform helper, and
/// the environment-named transports alike, so a hanging helper costs
/// at most this much before the chain kills it and reports.
pub(crate) const COPY_DEADLINE: Duration = Duration::from_millis(1500);

/// The poll step the bounded transport wait sleeps between checks.
///
/// Fine enough that a fast helper's exit is noticed within one step
/// of finishing, coarse enough that the poll thread never competes
/// with the render loop for the core.
const COPY_POLL_STEP: Duration = Duration::from_millis(25);

/// How long a finished transport's writer gets to land its write.
///
/// A well-behaved helper's write completes milliseconds after its
/// exit; the grace covers only that drain, so a writer parked on a
/// pipe some escaped descendant holds is abandoned instead of gating
/// the verdict — the thread leaves whenever the pipe finally closes.
const WRITER_GRACE: Duration = Duration::from_millis(250);

/// What a copy path knows synchronously about a delivery.
///
/// The funnel's immediate phase answers with one of these, drawn from
/// the same vocabulary the drained verdict uses, so the two phases can
/// only agree or upgrade — never contradict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAnswer {
    /// A transport confirmed delivery synchronously.
    ///
    /// Earns the plain `selection copied` claim on the spot; the
    /// injected test recorders report this.
    Verified,

    /// The escape's bytes left, and confirmation is still pending.
    ///
    /// Transports are in flight, so the honest immediate wording is
    /// the unconfirmed one until a verdict lands.
    Unconfirmed,

    /// Nothing reported anything.
    ///
    /// No bytes left and nothing schedulable — the wording names the
    /// escape hatch.
    Failed,
}

/// The synchronous answer once the escape write is known.
///
/// An escape write carries no acceptance signal — the terminal may
/// silently ignore it — so even a successful write is only
/// unconfirmed. With process transports scheduled the answer is
/// unconfirmed too: confirmation is theirs to deliver as a verdict.
/// Only a transport that exits successfully can verify.
#[must_use]
pub(crate) fn transport_answer(escape_written: bool, transports_scheduled: bool) -> CopyAnswer {
    match (escape_written, transports_scheduled) {
        (false, false) => CopyAnswer::Failed,
        _ => CopyAnswer::Unconfirmed,
    }
}

/// The definitive result of one off-loaded copy attempt.
///
/// The escape write carries no acceptance signal, so only a process
/// transport that exited successfully counts as confirmation; the
/// frame drain maps these facts onto the notice row's wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CopyVerdict {
    /// Which copy attempt this verdict belongs to.
    ///
    /// One monotonic counter per app stamps every copy at the funnel,
    /// so overlapping attempts drain in attempt order — a late verdict
    /// from an older copy cannot overwrite a newer one.
    pub(crate) attempt: u64,

    /// Whether the OSC 52 escape's bytes left stdout.
    ///
    /// True means the handoff happened, never that the receiving
    /// terminal accepted it — the honest ceiling of an escape.
    pub(crate) escape_written: bool,

    /// Whether any process transport exited successfully.
    ///
    /// A transport that launched, consumed the text, and exited zero
    /// is the only delivery the chain can actually confirm.
    pub(crate) verified: bool,
}

/// The shared slot off-loaded copies report their verdicts into.
///
/// One slot per app: the default copier pushes from its background
/// task and the frame drain empties it every frame, so nothing
/// accumulates. Pushes land in whatever order the transports finish;
/// the drain posts the newest attempt's verdict, and only when no
/// newer copy already holds the notice row.
pub(crate) type CopyOutcomes = Arc<std::sync::Mutex<Vec<CopyVerdict>>>;

/// The seam a completed selection's text goes to.
///
/// Receives the copy's attempt id beside the text — the same id any
/// later verdict lands under — and answers synchronously with what
/// the copy path knows; the default answers from the transport chain
/// and a test recorder from its own script.
pub(crate) type SelectionCopier = Box<dyn Fn(u64, &str) -> CopyAnswer>;

/// The app's whole copy pipeline: the seam a copy goes through, the
/// slot its off-loaded verdicts land in, and the bookkeeping that
/// keeps each attempt's id and its notice in step.
///
/// One value per app, built at construction with the configured copy
/// command baked into the default copier; a host or test replaces the
/// seam through [`TuiApp::set_selection_copier`]. Everything else the
/// copy paths need — the monotonic attempt counter and the
/// last-posted watermark — lives here so the funnel, the frame drain,
/// and a reconfigured command all read one state.
pub(crate) struct CopyState {
    /// Where a completed selection's text goes.
    ///
    /// Receives the copy's attempt id beside the text — the same id any
    /// later verdict lands under — and answers synchronously with what
    /// the copy path knows; the default answers from the transport chain
    /// and a test recorder from its own script.
    pub(crate) copier: SelectionCopier,

    /// Verdicts from off-loaded copy transports, awaiting their notice.
    ///
    /// The default copier pushes here from its background task and
    /// pings the render wake; the frame drain turns each verdict into
    /// the definitive notice wording. Test-installed recorders report
    /// synchronously and never touch it.
    pub(crate) copy_outcomes: CopyOutcomes,

    /// The monotonic id source for copy attempts.
    ///
    /// One counter per app: every copy — scheduled or settled on the
    /// spot — takes its id here, so the immediate notice and any
    /// later verdict for the same copy carry the same number.
    pub(crate) copy_attempts: Arc<std::sync::atomic::AtomicU64>,

    /// The attempt whose wording last took the notice row.
    ///
    /// Zero until a copy posts. A drained verdict older than this is
    /// spent news about a copy the user has already walked past, so
    /// the drain drops it instead of overwriting a newer copy.
    pub(crate) posted_copy_attempt: u64,
}

impl CopyState {
    /// Build the pipeline with its default transport copier.
    ///
    /// The outcomes slot, the attempt counter, and the wake the
    /// background transports ping are allocated here and shared with
    /// the closure, so a copy scheduled through the seam and the
    /// verdict drained by the frame read the same state however often
    /// the seam itself is later replaced.
    pub(crate) fn new(custom: Option<String>, wake: Arc<event_listener::Event>) -> Self {
        let copy_outcomes: CopyOutcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let copier_outcomes = Arc::clone(&copy_outcomes);
        let copy_attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let copier_attempts = Arc::clone(&copy_attempts);
        let copier: SelectionCopier = Box::new(move |attempt, text| {
            copy_via_transports(
                custom.as_deref(),
                attempt,
                text,
                &copier_attempts,
                &copier_outcomes,
                &wake,
            )
        });
        Self {
            copier,
            copy_outcomes,
            copy_attempts,
            posted_copy_attempt: 0,
        }
    }

    /// Take the newest verdict a frame may still post, if one stands.
    ///
    /// Drains the shared slot and keeps only the newest attempt's
    /// verdict — and only when it is not older than the wording the
    /// notice row already carries, so a late verdict from an older
    /// copy cannot overwrite a newer one. The watermark stamps here,
    /// making the return the one verdict the caller may word a notice
    /// from.
    pub(crate) fn drain_newest_verdict(&mut self) -> Option<CopyVerdict> {
        let verdicts = take_locked(&self.copy_outcomes);
        let latest = verdicts.into_iter().max_by_key(|verdict| verdict.attempt)?;
        if latest.attempt < self.posted_copy_attempt {
            return None;
        }
        self.posted_copy_attempt = latest.attempt;
        Some(latest)
    }
}

/// Take the next copy-attempt id from the shared counter.
///
/// One counter per app, shared with the copier: the read-modify-write
/// is a single atomic step, so copies stamping concurrently always
/// take distinct ids, and the first stamp is 1.
pub(crate) fn next_attempt(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1)
}

/// The configured copy command, blank values dropped.
///
/// A blank or whitespace-only `display.copy_command` is the one-input
/// case of the chain's unset discipline: `sh -c ""` would exit
/// successfully and count as a delivery that delivered nothing.
pub(crate) fn configured_copy_command(custom: Option<&str>) -> Option<&str> {
    custom.filter(|command| !command.trim().is_empty())
}

/// Which host clipboard transports the environment makes possible.
///
/// Snapshotted from the process environment once per copy; a helper
/// that is installed but has no server to talk to (a Wayland tool
/// with no Wayland session) is ruled out by its missing variable, so
/// it never runs just to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClipboardEnv {
    /// Whether `$TMUX` is set: inside tmux, `load-buffer -w` reaches
    /// tmux's own paste buffer and asks the outer terminal for its
    /// clipboard.
    in_tmux: bool,

    /// Whether `$WAYLAND_DISPLAY` is set: a Wayland session can take
    /// the copy through `wl-copy`.
    wayland: bool,

    /// Whether `$DISPLAY` is set: an X session can take the copy
    /// through `xclip` or `xsel`.
    x11: bool,
}

/// A host-side clipboard transport, in attempt order.
///
/// One spawned helper that takes the text on stdin; the variants name
/// both the program and the order the chain tries them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostTransport {
    /// tmux's buffer, pushed onward to the system clipboard.
    ///
    /// `load-buffer -w -` reads the text on stdin — `set-buffer`
    /// would take `-` as its literal data argument and drop the piped
    /// text — fills tmux's paste buffer, and forwards to the attached
    /// terminal's clipboard, covering both paste paths.
    Tmux,

    /// The Wayland clipboard helper.
    ///
    /// `wl-copy` owns the Wayland clipboard selection directly.
    WlCopy,

    /// The X11 clipboard helper, clipboard selection.
    ///
    /// `xclip -selection clipboard` targets the selection a paste
    /// reads by default.
    Xclip,

    /// The X11 clipboard helper, alternate spelling.
    ///
    /// `xsel --clipboard --input` covers hosts that ship xsel rather
    /// than xclip.
    Xsel,
}

impl HostTransport {
    /// The spawned command that carries this transport.
    ///
    /// Built with its fixed arguments; the caller pipes the text into
    /// its stdin and waits out the exit.
    fn command(self) -> std::process::Command {
        match self {
            Self::Tmux => {
                let mut command = std::process::Command::new("tmux");
                command.args(["load-buffer", "-w", "-"]);
                command
            }
            Self::WlCopy => std::process::Command::new("wl-copy"),
            Self::Xclip => {
                let mut command = std::process::Command::new("xclip");
                command.args(["-selection", "clipboard"]);
                command
            }
            Self::Xsel => {
                let mut command = std::process::Command::new("xsel");
                command.args(["--clipboard", "--input"]);
                command
            }
        }
    }
}

/// Read the process environment into a [`ClipboardEnv`].
///
/// A variable that is unset or empty disables its transport: the
/// helper would otherwise run against a display or server that does
/// not exist.
pub(crate) fn clipboard_env() -> ClipboardEnv {
    ClipboardEnv {
        in_tmux: non_empty("TMUX"),
        wayland: non_empty("WAYLAND_DISPLAY") || wayland_socket_present(),
        x11: non_empty("DISPLAY"),
    }
}

/// Whether a Wayland compositor's socket is discoverable without
/// `$WAYLAND_DISPLAY`.
///
/// A tmux server launched from a text console carries no graphical
/// variables into its panes even while a Wayland session runs
/// elsewhere, and a terminal that ignores OSC 52 then turns the
/// tmux-buffer transport into a dead end. libwayland falls back to
/// the conventional `wayland-0` socket under `$XDG_RUNTIME_DIR`, so
/// that socket sitting there — a real socket, the endpoint `wl-copy`
/// itself opens — is evidence enough that the helper can reach a
/// compositor; a lock file or a later socket names no endpoint the
/// helper would use, and an ordinary file at the name is not a
/// socket at all.
pub(crate) fn wayland_socket_present() -> bool {
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    wayland_socket_in(std::path::Path::new(&runtime_dir))
}

/// The directory scan behind [`wayland_socket_present`].
///
/// Pure over the directory so the pin can drive it with a fixture;
/// only the default `wayland-0` socket counts — the endpoint
/// `wl-copy` opens when no display is named — and on unix it must be
/// a socket file, read through a symlink when one carries the name
/// (a compositor stack that links the conventional name to its own
/// instance socket still names the endpoint the helper opens, while
/// the link itself reports as a link), not an ordinary file or a
/// lock that happens to carry the name. A dangling link names no
/// endpoint at all and counts as little.
pub(crate) fn wayland_socket_in(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            if entry.file_name().to_string_lossy() != "wayland-0" {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt as _;
                std::fs::metadata(entry.path()).is_ok_and(|meta| meta.file_type().is_socket())
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    })
}

/// Whether `name` is set to a non-empty value.
///
/// Empty-valued variables count as unset: an empty `$DISPLAY`, for
/// instance, names no X server to reach.
pub(crate) fn non_empty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The host transports worth attempting for `env`, in order.
///
/// Selection is pure over the snapshot so the pins can drive it
/// without touching the process environment; the order only matters
/// for spawn cost, since every attempt carries the same text.
pub(crate) fn host_transports(env: ClipboardEnv) -> Vec<HostTransport> {
    let mut transports = Vec::new();
    if env.in_tmux {
        transports.push(HostTransport::Tmux);
    }
    if env.wayland {
        transports.push(HostTransport::WlCopy);
    }
    if env.x11 {
        transports.push(HostTransport::Xclip);
        transports.push(HostTransport::Xsel);
    }
    transports
}

/// The process transports one copy's chain holds, in run order.
///
/// Split from [`run_process_transports`] so a caller can prepare the
/// chain once and hand it to the ordered lane whole, and so the pins
/// can drive the runner with a chain of their own — nothing ambient
/// spawns beside the commands a test names.
pub(crate) fn transport_commands(custom: Option<&str>) -> Vec<std::process::Command> {
    let mut commands = Vec::new();
    if let Some(command_line) = custom {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(command_line);
        commands.push(command);
    }
    if cfg!(target_os = "macos") {
        commands.push(std::process::Command::new("pbcopy"));
    }
    for transport in host_transports(clipboard_env()) {
        commands.push(transport.command());
    }
    commands
}

/// Run a prepared transport chain under one shared deadline.
///
/// The clocked loop every copy's transports walk, over a chain the
/// caller assembled — the production jobs hand it the environment's
/// own chain through [`transport_commands`], and the ordered lane
/// hands it whichever chain the fenced copy carried, so both share
/// one bounded runner. Every transport checks the remaining budget
/// before spawning — a helper that consumed the full budget leaves
/// nothing for the rest to spend, so the chain stops rather than
/// spawning past the deadline. The chain is consumed: a command
/// carries its stdio setup, so each runs exactly once, in the order
/// it was assembled. Returns whether any transport exited
/// successfully with its stdin fully written; an empty chain returns
/// false without spawning anything.
pub(crate) fn run_command_chain(
    commands: Vec<std::process::Command>,
    text: &str,
    deadline: Duration,
) -> bool {
    let start = Instant::now();
    let mut verified = false;
    let run = |command: std::process::Command| {
        if start.elapsed() >= deadline {
            return false;
        }
        run_bounded(command, text, start, deadline)
    };
    for command in commands {
        verified |= run(command);
    }
    verified
}

/// The single lane process clipboard writes take their turn on.
///
/// Copies overlap freely — a burst of selections schedules several
/// transport jobs at once — but the clipboard is one resource, and
/// writes landing in whatever order the helpers finish would leave an
/// older slow helper's text standing over the newer copy's delivery.
/// The lane orders every process write: each job takes its turn, and
/// a turn granted after a newer copy has started is declined on the
/// spot by [`fenced_process_writes`], so the newest attempt's write is
/// the last write the clipboard takes. A helper that hangs holds its
/// turn no longer than [`COPY_DEADLINE`], so the lane's worst wait is
/// one bounded deadline per copy ahead in line.
static TRANSPORT_LANE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run one copy's transport chain in the lane's order, fenced against
/// newer copies.
///
/// Two checks around one turn: a copy a newer attempt has already
/// superseded declines before it queues for the lane, and a copy that
/// was current when it queued but was superseded while it waited
/// declines the turn the moment the lane grants it — the clipboard
/// never takes a write a newer copy has overtaken. A copy that holds
/// the lane when a newer one starts finishes its own write first; the
/// newer job's write lands after, so the newest text is the last text
/// the clipboard holds. Returns whether the copy verified; a declined
/// copy verifies nothing, having written nothing.
pub(crate) fn fenced_process_writes(
    attempt: u64,
    attempts: &std::sync::atomic::AtomicU64,
    commands: Vec<std::process::Command>,
    text: &str,
) -> bool {
    if attempts.load(std::sync::atomic::Ordering::SeqCst) > attempt {
        return false;
    }
    let turn = TRANSPORT_LANE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if attempts.load(std::sync::atomic::Ordering::SeqCst) > attempt {
        return false;
    }
    let verified = run_command_chain(commands, text, COPY_DEADLINE);
    drop(turn);
    verified
}

/// Spawn one transport and bound its whole lifetime.
///
/// The text reaches the child through a writer thread, so a helper
/// that ignores stdin cannot wedge the chain on a full pipe, and the
/// poll loop kills the child's whole process tree once the deadline
/// passes — the worst case is one bounded wait, never an open-ended
/// one. A copy verifies only when both the child exited successfully
/// inside the deadline and the stdin write completed: a helper that
/// exits zero without consuming its input has not delivered the
/// text. Returns whether the transport verified.
pub(crate) fn run_bounded(
    mut command: std::process::Command,
    text: &str,
    start: Instant,
    deadline: Duration,
) -> bool {
    // The group is what makes the deadline's kill reach descendants
    // that inherited the pipe — killing the child alone strands them.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let mut stdin = child.stdin.take();
    let owned = text.to_string();
    let (writer_done, writer_landed) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;

        if let Some(pipe) = stdin.as_mut() {
            drop(writer_done.send(pipe.write_all(owned.as_bytes())));
        }
    });
    let Some(expiry) = start.checked_add(deadline) else {
        kill_transport_tree(&mut child);
        drop(child.wait());
        wait_writer_bounded(writer, &writer_landed);
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let exited = status.success();
                let wrote = wait_writer_bounded(writer, &writer_landed);
                return exited && wrote;
            }
            Ok(None) => {}
            Err(_) => {
                kill_transport_tree(&mut child);
                drop(child.wait());
                wait_writer_bounded(writer, &writer_landed);
                return false;
            }
        }
        if Instant::now() >= expiry {
            kill_transport_tree(&mut child);
            drop(child.wait());
            wait_writer_bounded(writer, &writer_landed);
            return false;
        }
        std::thread::sleep(COPY_POLL_STEP);
    }
}

/// Kill a transport's whole process tree.
///
/// The child runs as its own process-group leader, so a group signal
/// reaches descendants that inherited the pipe — a backgrounded
/// helper holding stdin dies with its parent and the writer
/// unblocks. Away from Unix the direct child is all that can be
/// reached.
pub(crate) fn kill_transport_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pgid) = i32::try_from(child.id()) {
        // SAFETY: the pgid is the freshly spawned child's own id,
        // not yet reaped, so no pid reuse can retarget the signal;
        // killpg only delivers it.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    drop(child.kill());
}

/// Wait out a transport's writer within its grace, reporting whether
/// the write completed.
///
/// Every well-behaved transport's write lands inside the grace —
/// normally in milliseconds — so an exit status is not reported
/// before the write finished. A writer still parked on a pipe some
/// escaped descendant holds cannot be waited out at all: the grace
/// lapses, the join is dropped, and the thread leaves by itself
/// whenever the pipe finally closes. The verdict is never held
/// hostage to it. Returns whether the write both landed inside the
/// grace and completed without error — `false` for a timeout, an
/// abandoned writer, or an error the pipe reported.
pub(crate) fn wait_writer_bounded(
    writer: std::thread::JoinHandle<()>,
    landed: &std::sync::mpsc::Receiver<std::io::Result<()>>,
) -> bool {
    match landed.recv_timeout(WRITER_GRACE) {
        Ok(Ok(())) => {
            drop(writer.join());
            true
        }
        Ok(Err(_)) => {
            drop(writer.join());
            false
        }
        Err(_) => {
            drop(writer);
            false
        }
    }
}

/// The empty stand-in for the tool segment a selection never covers.
///
/// Running tools render below the selectable document; the window
/// helpers want four segments, and this one is always empty — a
/// `static` so a returned window can borrow it past the call.
pub(crate) static NO_LINES: [Line<'static>; 0] = [];

#[cfg(test)]
mod tests {

    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_runtime_dir_socket_names_wayland_without_the_variable() {
        // The console-launched tmux shape: no $WAYLAND_DISPLAY, but
        // the compositor's default socket sits in the runtime dir
        // libwayland falls back to. The transport engages on that
        // socket alone: a later socket or a lock names no endpoint
        // `wl-copy` opens, and an ordinary file at the name is not a
        // socket at all — the fallback never launches a helper that
        // cannot reach a compositor.
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("dch-wayland-pin-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("pin dir");
        assert!(
            !wayland_socket_in(&dir),
            "an empty runtime dir offers no wayland"
        );
        std::fs::write(dir.join("wayland-1"), b"").expect("later socket marker");
        std::fs::write(dir.join("wayland-0.lock"), b"").expect("lock marker");
        assert!(
            !wayland_socket_in(&dir),
            "a later name and the lock are not the endpoint wl-copy opens"
        );
        let listener = UnixListener::bind(dir.join("wayland-0")).expect("the default socket binds");
        assert!(
            wayland_socket_in(&dir),
            "the conventional default socket names a reachable compositor"
        );
        drop(listener);
        std::fs::remove_file(dir.join("wayland-0")).expect("socket removed");
        std::fs::write(dir.join("wayland-0"), b"").expect("plain file marker");
        assert!(
            !wayland_socket_in(&dir),
            "an ordinary file at the socket's name is no socket"
        );
        std::fs::remove_file(dir.join("wayland-0")).expect("plain file removed");
        let instance = UnixListener::bind(dir.join("wayland-instance")).expect("instance socket");
        std::os::unix::fs::symlink(dir.join("wayland-instance"), dir.join("wayland-0"))
            .expect("conventional name linked to the instance");
        assert!(
            wayland_socket_in(&dir),
            "a symlink to a live default socket names a reachable compositor"
        );
        drop(instance);
        std::fs::remove_file(dir.join("wayland-instance")).expect("instance removed");
        assert!(
            !wayland_socket_in(&dir),
            "a dangling link at the socket's name names no endpoint"
        );
        std::fs::remove_dir_all(&dir).expect("pin dir cleaned");
    }

    /// A copy's chain over a helper that appends `line` to `out`.
    fn appending_chain(
        out: &std::path::Path,
        line: &str,
        delay: &str,
    ) -> Vec<std::process::Command> {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{delay}printf '%s\\n' {line} >> {}", out.display()));
        vec![command]
    }

    #[test]
    fn a_superseded_copy_cannot_write_over_a_newer_one() {
        // Three copies overlap: the first holds the lane inside a slow
        // helper, the second queues behind it, and the third starts
        // while both wait. The lane orders the writes and the fence
        // declines the second's turn — superseded while it queued —
        // so the clipboard's record ends on the newest copy's text and
        // never carries the overtaken copy's: the slow helper's text
        // lands only before the newest write, never after it.
        let dir = tempfile::tempdir().expect("pin dir");
        let out = dir.path().join("clipboard");
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let slow = {
            let out = out.clone();
            let attempts = Arc::clone(&attempts);
            std::thread::spawn(move || {
                fenced_process_writes(
                    1,
                    &attempts,
                    appending_chain(&out, "one", "sleep 0.4; "),
                    "",
                )
            })
        };
        std::thread::sleep(Duration::from_millis(120));
        attempts.store(2, std::sync::atomic::Ordering::SeqCst);
        let queued = {
            let out = out.clone();
            let attempts = Arc::clone(&attempts);
            std::thread::spawn(move || {
                fenced_process_writes(2, &attempts, appending_chain(&out, "two", ""), "")
            })
        };
        attempts.store(3, std::sync::atomic::Ordering::SeqCst);
        let newest = {
            let out = out.clone();
            let attempts = Arc::clone(&attempts);
            std::thread::spawn(move || {
                fenced_process_writes(3, &attempts, appending_chain(&out, "three", ""), "")
            })
        };
        for job in [slow, queued, newest] {
            job.join().expect("job finishes");
        }
        let record = std::fs::read_to_string(&out).expect("the helpers appended a record");
        let lines: Vec<&str> = record.lines().collect();
        assert!(
            !lines.contains(&"two"),
            "a copy superseded while it queued writes nothing: {record:?}"
        );
        assert_eq!(
            lines.last().copied(),
            Some("three"),
            "the newest copy's text is the last write the clipboard took: {record:?}"
        );
    }

    #[test]
    fn a_copy_superseded_before_it_queues_writes_nothing() {
        // The fast path: a job whose attempt a newer copy already
        // overtook never takes the lane and never spawns a helper.
        let dir = tempfile::tempdir().expect("pin dir");
        let out = dir.path().join("clipboard");
        let attempts = std::sync::atomic::AtomicU64::new(7);
        let verified = fenced_process_writes(3, &attempts, appending_chain(&out, "stale", ""), "");
        assert!(!verified, "a declined copy verifies nothing");
        assert!(
            !out.exists(),
            "a copy superseded before it queued spawned no helper"
        );
    }

    #[test]
    fn an_empty_chain_verifies_nothing() {
        // The chain the runner walks is the caller's: with nothing in
        // it, nothing spawns and nothing verifies.
        let attempts = std::sync::atomic::AtomicU64::new(1);
        assert!(
            !fenced_process_writes(1, &attempts, Vec::new(), ""),
            "an empty chain never verifies"
        );
    }

    #[test]
    fn host_transports_follow_the_environment_snapshot() {
        let none = host_transports(ClipboardEnv {
            in_tmux: false,
            wayland: false,
            x11: false,
        });
        assert!(none.is_empty(), "a headless host offers no transports");

        let tmux_only = host_transports(ClipboardEnv {
            in_tmux: true,
            wayland: false,
            x11: false,
        });
        assert_eq!(tmux_only, [HostTransport::Tmux], "tmux is named first");

        let tmux_command = HostTransport::Tmux.command();
        assert_eq!(tmux_command.get_program(), "tmux");
        assert_eq!(
            tmux_command.get_args().collect::<Vec<_>>(),
            ["load-buffer", "-w", "-"],
            "set-buffer would take `-` as literal data and drop the piped text"
        );

        let wayland_only = host_transports(ClipboardEnv {
            in_tmux: false,
            wayland: true,
            x11: false,
        });
        assert_eq!(
            wayland_only,
            [HostTransport::WlCopy],
            "a wayland session names wl-copy"
        );

        let x11_only = host_transports(ClipboardEnv {
            in_tmux: false,
            wayland: false,
            x11: true,
        });
        assert_eq!(
            x11_only,
            [HostTransport::Xclip, HostTransport::Xsel],
            "an x session tries both spellings in order"
        );

        let everything = host_transports(ClipboardEnv {
            in_tmux: true,
            wayland: true,
            x11: true,
        });
        assert_eq!(
            everything,
            [
                HostTransport::Tmux,
                HostTransport::WlCopy,
                HostTransport::Xclip,
                HostTransport::Xsel
            ],
            "the full environment keeps the attempt order"
        );
    }

    #[test]
    fn a_blank_copy_command_counts_as_unset() {
        assert_eq!(configured_copy_command(None), None, "unset stays unset");
        assert_eq!(configured_copy_command(Some("")), None, "empty is dropped");
        assert_eq!(
            configured_copy_command(Some("  \t ")),
            None,
            "whitespace-only is dropped"
        );
        assert_eq!(
            configured_copy_command(Some("cat > /dev/null")),
            Some("cat > /dev/null"),
            "a real command passes through as authored"
        );
    }

    #[test]
    fn a_fast_copy_command_verifies_within_the_deadline() {
        let began = std::time::Instant::now();
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("cat > /dev/null");
        assert!(
            run_bounded(command, "payload", began, Duration::from_millis(500)),
            "a command that consumes stdin and exits zero verifies"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the wait stays bounded"
        );
    }

    #[test]
    fn a_hanging_copy_command_is_killed_at_the_deadline() {
        let began = std::time::Instant::now();
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("sleep 30");
        assert!(
            !run_bounded(command, "payload", began, Duration::from_millis(150)),
            "a command that outlives the deadline reports no delivery"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the chain returns around the deadline, not the child's lifetime"
        );
    }

    #[test]
    fn a_descendant_holding_stdin_cannot_outlive_the_deadline() {
        let began = std::time::Instant::now();
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("cat > /dev/null & sleep 60");
        assert!(
            !run_bounded(command, "payload", began, Duration::from_millis(200)),
            "a transport whose descendant holds the pipe reports no delivery"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the group kill plus the writer grace bound the whole chain"
        );
    }

    #[test]
    fn concurrent_copies_stamp_one_distinct_attempt_each() {
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let gate = Arc::new(std::sync::Barrier::new(8));
        let stampers: Vec<_> = (0..8)
            .map(|_| {
                let counter = Arc::clone(&counter);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    (0..1000)
                        .map(|_| next_attempt(&counter))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut stamped = Vec::new();
        for stamper in stampers {
            stamped.extend(stamper.join().expect("the stamper survives"));
        }
        stamped.sort_unstable();
        assert_eq!(
            stamped,
            (1..=8000).collect::<Vec<_>>(),
            "overlapping copies never share an attempt id"
        );
    }

    #[test]
    fn the_synchronous_answer_follows_what_is_schedulable() {
        assert_eq!(
            transport_answer(true, false),
            CopyAnswer::Unconfirmed,
            "an escape-only write is unconfirmed — the terminal may ignore it"
        );
        assert_eq!(
            transport_answer(false, false),
            CopyAnswer::Failed,
            "nothing leaving and nothing schedulable fails outright"
        );
        assert_eq!(
            transport_answer(true, true),
            CopyAnswer::Unconfirmed,
            "transports in flight hold the plain claim back"
        );
        assert_eq!(
            transport_answer(false, true),
            CopyAnswer::Unconfirmed,
            "a failed escape still waits on the transports' verdict"
        );
    }

    #[test]
    fn a_helper_that_exits_without_consuming_stdin_does_not_verify() {
        // A helper that exits zero without consuming a payload larger
        // than the pipe buffer has not delivered the text: the write
        // fails with a broken pipe once the child's end closes, so
        // the transport cannot verify even though the exit code was
        // successful.
        let began = std::time::Instant::now();
        let payload = "x".repeat(256 * 1024);
        let command = std::process::Command::new("true");
        assert!(
            !run_bounded(command, &payload, began, Duration::from_millis(2_000)),
            "a zero-exit helper that never read its stdin does not verify"
        );
    }

    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::commands::Command;
    use crate::message::{ContentBlock, TuiMessage};

    use crate::queue::SessionUpdate;
    use std::sync::Arc;

    #[test]
    fn a_selector_cluster_reaches_the_screen_stripped_and_the_copy_source_whole() {
        // The screen and the shared width table must agree: a
        // selector-bearing cluster strips before display, so the cell
        // after the glyph receives the letter the wrap laid beside it —
        // the letter the pre-fix disagreement starved — while the
        // cached line a copy reads keeps the selector the author
        // wrote.
        let mut subject = TuiApp::new(dch_config::DchConfig::default());
        subject.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "done \u{2714}\u{fe0f} shipped".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: Some(10),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| subject.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !rendered.contains('\u{fe0f}'),
            "the selector never reaches the screen"
        );
        let glyph = buffer
            .content
            .iter()
            .position(|cell| cell.symbol() == "\u{2714}")
            .expect("the check glyph renders on its own cell");
        assert_eq!(
            glyph
                .checked_add(1)
                .and_then(|next| buffer.content.get(next))
                .map(ratatui::buffer::Cell::symbol),
            Some(" "),
            "the cluster's measured second cell is a written pad, never elided"
        );
        assert_eq!(
            glyph
                .checked_add(2)
                .and_then(|next| buffer.content.get(next))
                .map(ratatui::buffer::Cell::symbol),
            Some("s"),
            "the letter beside the cluster lands at the column the model laid it"
        );
        let cached: String = subject
            .bake
            .conversation_cache
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.to_string())
            .collect();
        assert!(
            cached.contains('\u{fe0f}'),
            "the copy source keeps the selector the author wrote"
        );
    }

    #[test]
    fn a_late_verdict_from_an_older_copy_cannot_overwrite_a_newer_one() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let mut slot = app.copy.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 2,
            escape_written: true,
            verified: true,
        });
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: false,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "the newest attempt's wording is the one that shows"
        );
    }

    #[test]
    fn an_older_verdict_cannot_overwrite_a_newer_synchronous_copy() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_selection_copier(Box::new(|_attempt, _text| CopyAnswer::Verified));
        app.copy_selection_and_notice("first");
        app.copy_selection_and_notice("second");
        let mut slot = app.copy.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: false,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "the newest synchronous copy keeps its verified wording"
        );
    }

    #[test]
    fn a_verdict_still_upgrades_its_own_immediate_wording() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_selection_copier(Box::new(|_attempt, _text| CopyAnswer::Unconfirmed));
        app.copy_selection_and_notice("pending");
        let mut slot = app.copy.copy_outcomes.lock().expect("the outcomes slot");
        slot.push(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: true,
        });
        drop(slot);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        assert_eq!(
            app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
            Some("selection copied"),
            "the same attempt's settled verdict replaces its immediate wording"
        );
    }

    #[test]
    fn copy_with_no_answer_says_so() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/copy");
        assert!(out.contains("no answer to copy"), "got {out:?}");
    }

    #[test]
    fn a_repair_after_the_verdict_cleared_says_nothing_to_repair() {
        // The round-4 lifecycle case's door half: the verdict repair
        // is keyed to can clear without the command — a `/clear`
        // rotation and a resume into a healthy session both adopt a
        // fresh memory. Against a cleared verdict the door must
        // answer "nothing to repair", because the driver re-seeded
        // the config layer beside the adoption and no posture
        // stands.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let memory = crate::queue::PermissionMemory::default();
        memory.mark_damaged();
        app.set_permission_memory(memory);
        assert!(
            app.permission.permission_memory.damage_standing(),
            "the damage verdict stands before the wholesale adoption"
        );
        // The clear path's motion on the shared memory: the fresh
        // adoption clears the verdict.
        app.permission
            .permission_memory
            .adopt(Vec::new(), Vec::new(), false);

        app.permissions_command(Some("repair"));

        let (notice, _) = app
            .transient_notice
            .as_ref()
            .expect("the repair answers with a notice");
        assert!(
            notice.contains("nothing to repair"),
            "the door answers for the verdict that stands — none: {notice}"
        );
    }

    #[test]
    fn the_switch_verdict_does_not_overwrite_the_engine_s_window() {
        // The display's verdict arm is not the window's authority: the
        // driver rebuilt the engine and stored its actual measure —
        // including any provider-probe discovery the config alone
        // cannot name. The display makes the same profile selection
        // for /config, but does NOT re-store the window; whatever the
        // engine measures is what the bar reads, and the config's
        // figure (the fallback 200k when nothing states one) must not
        // replace it.
        let config = dch_config::DchConfig {
            models: vec![dch_config::ModelProfile {
                model: "wide".to_string(),
                context_window: None,
                ..dch_config::ModelProfile::default()
            }],
            ..dch_config::DchConfig::default()
        };
        let mut app = TuiApp::new(config);
        // The driver stored the engine's probe-discovered figure —
        // say 131072 (a real window the config never states).
        app.state
            .context_window
            .store(131_072, std::sync::atomic::Ordering::SeqCst);
        app.apply_session_update(SessionUpdate::ModelName {
            name: "wide".to_string(),
        });
        let (_, window) = app.context_snapshot();
        assert_eq!(
            window, 131_072,
            "the engine's own measure survives the verdict — the config's fallback does not replace it"
        );
    }

    #[test]
    fn a_model_verdict_persists_the_transcript_at_once() {
        // The switch changes the envelope's model the moment it lands
        // — the saver is already relabeled on the driver's half — so
        // the display fires the save hook immediately: session.json
        // names the new model without waiting for a turn to end.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = Arc::clone(&fired);
        app.set_turn_end_hook(Box::new(move |_, _| {
            sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        app.apply_session_update(SessionUpdate::ModelName {
            name: "switched-model".to_string(),
        });
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the verdict publishes a save at once, not at the next turn end"
        );
        assert_eq!(
            app.config.api.model, "switched-model",
            "and the config carries the verdict"
        );
    }

    #[test]
    fn a_set_copy_command_takes_effect_without_a_restart() {
        // The copier closes over the configured command at
        // construction; the live arm rebuilds it, so a mid-session
        // /set copy_command routes the very next copy through the new
        // command — pinned with a real shell writing the copied text.
        let target = tempfile::NamedTempFile::new().expect("the copy target");
        let path = target.path().to_string_lossy().into_owned();
        let mut app = app();
        app.run_command(Command::Set {
            key: "copy_command".to_string(),
            value: format!("cat > '{path}'"),
        });
        app.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text {
                text: "the answer".to_string(),
            }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
        let _ = command_output(&mut app, "/copy");
        let mut landed = false;
        for _ in 0..40 {
            app.drain_shared_state();
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("the answer")
            {
                landed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            landed,
            "the copy rode the newly configured command, not the one the \
             session started with"
        );
    }

    #[test]
    fn a_model_verdict_persists_the_selection_not_the_expanded_bundle() {
        // The verdict's [api] is the selected profile expanded — model,
        // endpoint, credential. Persisting it would freeze the endpoint
        // into [api] for every profile that names none of its own; the
        // file records the intended selection alone.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dch_config::DchConfig {
            models: vec![dch_config::ModelProfile {
                model: "glm".to_string(),
                base_url: Some("https://profile.example".to_string()),
                ..dch_config::ModelProfile::default()
            }],
            api: dch_config::ApiConfig {
                base_url: "https://base.example".to_string(),
                ..dch_config::ApiConfig::default()
            },
            ..dch_config::DchConfig::default()
        };
        let mut live = file.clone();
        // What the boot selection left in effect:
        live.api.model = "glm".to_string();
        live.api.base_url = "https://profile.example".to_string();
        let mut app = TuiApp::new(live);
        app.set_config_file(file);
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        app.apply_session_update(crate::queue::SessionUpdate::ModelName {
            name: "glm".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the verdict landed");
        let reloaded = dch_config::DchConfig::from_toml_str(&saved).expect("the file reloads");
        assert_eq!(
            reloaded.api.model, "glm",
            "the selection is recorded: {saved}"
        );
        assert_eq!(
            reloaded.api.base_url, "https://base.example",
            "the file's own endpoint stands in [api]: {saved}"
        );
        assert_eq!(
            reloaded.models.len(),
            1,
            "the profile list survives beside the selection: {saved}"
        );
    }

    #[test]
    fn a_model_switch_persists_only_on_the_loops_verdict() {
        // The send is not the switch: the panel writes nothing when the
        // command leaves, and the file lands when the loop's acceptance
        // comes back over the session channel — a refused switch leaves
        // the file describing the loop that kept running.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.set_config_persist_dir(Some(dir.path().to_path_buf()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_submit_tx(tx);
        app.run_command(Command::Model {
            name: "other-model".to_string(),
        });
        assert!(
            !dir.path().join("config.toml").exists(),
            "the send alone writes nothing"
        );
        app.apply_session_update(crate::queue::SessionUpdate::ModelName {
            name: "other-model".to_string(),
        });
        let saved =
            std::fs::read_to_string(dir.path().join("config.toml")).expect("the verdict landed");
        assert!(saved.contains("\"other-model\""), "got {saved}");
    }

    #[test]
    fn a_verdict_reselects_over_the_base_not_the_accreted_live_config() {
        // The no-accretion rule: the boot selection laid the seed entry
        // (its 200_000 window) onto the live config; a later verdict for
        // an entry stating no window must resolve over the user-stated
        // base — whose own window is 64_000 — not over the live bundle,
        // where the seed's 200_000 would silently stand and /config
        // would misreport the very figure the switch's notice prints.
        let file_config = dch_config::DchConfig::from_toml_str(
            r#"
[api]
model = "seed"
context_window = 64_000

[[models]]
model = "seed"
context_window = 200_000

[[models]]
model = "plain"
"#,
        )
        .expect("the fixture parses");
        let mut booted = file_config.clone();
        booted
            .select_model(None)
            .expect("the boot selection lays the seed entry");
        assert_eq!(
            booted.api.context_window,
            Some(200_000),
            "fixture premise: the live config carries the seed's window after boot"
        );
        let mut app = TuiApp::new(booted);
        app.set_config_base(file_config);
        app.apply_session_update(crate::queue::SessionUpdate::ModelName {
            name: "plain".to_string(),
        });
        assert_eq!(app.config.api.model, "plain", "the verdict's model stands");
        assert_eq!(
            app.config.api.context_window,
            Some(64_000),
            "the base's own window is what runs — the seed entry's 200_000 did not accrete"
        );
    }
}
