//! The terminal event source.
//!
//! [`TerminalEvents`] owns the one thread that reads the terminal
//! and hands every event — or the read failure that ends the
//! stream — to the app's event loop over a channel. Bounded
//! `poll()` waits drive the reader rather than a blocking
//! `read()`: crossterm's async `EventStream` does not reliably
//! wake a parked tokio runtime (a missed wakeup surfaces as input
//! latency), and a blocking read cannot be interrupted on drop.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::Event;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// How long the reader waits for input before re-checking the
/// shutdown flag.
///
/// The wait hides entirely behind the channel — the app loop is
/// woken by the send, not by the poll — so the value trades only
/// how promptly the thread notices a drop, never input latency.
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// How long a quiesced reader sleeps between re-checking the release
/// flag — short enough that resuming feels instant, long enough that
/// the parked loop is invisible to the scheduler.
const PAUSE_SLEEP: Duration = Duration::from_millis(2);

/// The pause handshake that lends stdin to one asker.
///
/// The reader thread normally owns the terminal's input: it sits in a
/// bounded poll and forwards every event. A mid-session background
/// query (the OSC 11 exchange behind a theme switch) must read the
/// terminal's reply itself, and polling against the reader races two
/// ways — the reader swallows the reply's escape bytes into the
/// composer as keystrokes, or the asker's deadline reads a lost race
/// as a silent terminal. So the asker parks the reader first:
/// [`request`](Self::request) the pause, then block on
/// [`wait_paused`](Self::wait_paused) until the reader acknowledges it
/// is parked — that acknowledgement is the proof stdin is
/// uncontested — then query, then [`release`](Self::release).
///
/// Nothing is lost while parked: keystrokes that arrive during the
/// pause sit in the terminal's kernel buffer and are delivered when
/// the reader resumes, and the one exchange that does read stdin
/// hands a stolen first byte back to the caller — so the handshake
/// costs latency (bounded by one poll wait plus the query's own
/// deadline), never input.
pub struct InputQuiesce {
    /// The asker's request: set before waiting for the acknowledgement,
    /// cleared on release.
    pause: AtomicBool,

    /// The reader's acknowledgement: set once the reader is parked and
    /// reading nothing, cleared as it resumes.
    paused: AtomicBool,
}

impl InputQuiesce {
    /// A handshake in its resting state: no request, no acknowledgement.
    ///
    /// The constructor the host calls once at startup: both flags down,
    /// so the reader thread polls freely until the first terminal query
    /// asks it to pause.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pause: AtomicBool::new(false),
            paused: AtomicBool::new(false),
        }
    }

    /// Ask the reader to park. This only makes the request; the
    /// acknowledgement arrives on [`wait_paused`](Self::wait_paused).
    pub fn request(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }

    /// Wait until the reader acknowledges it is parked, or `timeout`
    /// expires. `false` means stdin is still contested — no reader
    /// acknowledged — and the caller must not query.
    pub fn wait_paused(&self, timeout: Duration) -> bool {
        let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
            return self.paused.load(Ordering::SeqCst);
        };
        while std::time::Instant::now() < deadline {
            if self.paused.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(PAUSE_SLEEP);
        }
        self.paused.load(Ordering::SeqCst)
    }

    /// Let the reader resume. Keystrokes that arrived while it was
    /// parked are its to deliver the moment it polls again.
    pub fn release(&self) {
        self.pause.store(false, Ordering::SeqCst);
    }
}

impl Default for InputQuiesce {
    fn default() -> Self {
        Self::new()
    }
}

/// Hold stdin for one asker: acknowledge the park and sleep until the
/// request clears or shutdown demands the thread back.
///
/// The reader thread's park step, alone: the acknowledgement is the
/// asker's proof stdin is uncontested, and the bounded naps keep a
/// release prompt without spinning. It returns with the
/// acknowledgement withdrawn, leaving the caller to poll again or
/// exit as its own flags decide.
fn park_while_requested(quiesce: &InputQuiesce, shutdown: &AtomicBool) {
    quiesce.paused.store(true, Ordering::SeqCst);
    while quiesce.pause.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(PAUSE_SLEEP);
    }
    quiesce.paused.store(false, Ordering::SeqCst);
}

/// The terminal's events, read on a dedicated thread.
///
/// Constructing spawns the reader; it polls the terminal in
/// bounded waits for the source's whole lifetime and forwards each
/// event into the channel. The async [`recv`](Self::recv) feeds a
/// `select!` arm directly, and [`poll`](Self::poll) drains
/// whatever has already arrived so a burst of input applies to the
/// state before the frame that answers it — a fast typist or a
/// touchpad's wheel momentum renders once, not once per event.
///
/// A read failure is delivered as an `Err` item and ends the
/// stream: the caller learns the terminal broke instead of seeing
/// a clean close. Dropping the source sets the shutdown flag, and
/// the reader exits at its next wait boundary; a dropped receiver
/// also ends it, via the failed send.
pub struct TerminalEvents {
    /// The receiving half of the reader thread's channel.
    ///
    /// Owned here so the source is the single face of terminal
    /// input: the app loop awaits and polls through it, and
    /// dropping the source is what ends the reader thread.
    receiver: UnboundedReceiver<Result<Event, io::Error>>,

    /// The reader's shutdown signal, set on drop.
    ///
    /// Shared with the thread so a source dropped while the reader
    /// waits between events still stops it within one poll wait.
    shutdown: Arc<AtomicBool>,

    /// The pause handshake, shared with the reader thread.
    ///
    /// A mid-session background query parks the reader through this
    /// handle ([`TerminalEvents::quiesce_handle`]) so its reply cannot
    /// be stolen; nothing else reads stdin while the pause is
    /// acknowledged.
    quiesce: Arc<InputQuiesce>,
}

impl TerminalEvents {
    /// Spawn the reader thread and return the event source.
    ///
    /// The one construction path — the thread starts polling the
    /// terminal immediately and lives until the returned source
    /// drops, so an owner that stops selecting on events also
    /// stops the reader.
    #[must_use]
    pub fn spawn() -> Self {
        let (sender, receiver) = unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let quiesce = Arc::new(InputQuiesce::new());
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_quiesce = Arc::clone(&quiesce);
        std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                // A lent stdin: acknowledge the park and hold off
                // polling until released (or shut down) — the asker
                // owns the terminal's bytes in between.
                if thread_quiesce.pause.load(Ordering::SeqCst) {
                    park_while_requested(&thread_quiesce, &thread_shutdown);
                    continue;
                }
                let ready = match crossterm::event::poll(SHUTDOWN_POLL) {
                    Ok(ready) => ready,
                    Err(err) => return report_failure(&sender, err),
                };
                if !ready {
                    continue;
                }
                let event = match crossterm::event::read() {
                    Ok(event) => event,
                    Err(err) => return report_failure(&sender, err),
                };
                if sender.send(Ok(event)).is_err() {
                    break;
                }
            }
        });
        Self {
            receiver,
            shutdown,
            quiesce,
        }
    }

    /// The pause handle for lending stdin to one asker.
    ///
    /// The app wires this into the command layer at startup, so a
    /// theme switch can park the reader before asking the terminal
    /// anything — see [`InputQuiesce`].
    #[must_use]
    pub fn quiesce_handle(&self) -> Arc<InputQuiesce> {
        Arc::clone(&self.quiesce)
    }

    /// Await the next event or read failure.
    ///
    /// The channel's end — the reader thread gone without an error
    /// — reports as `None`, which the caller treats as a terminal
    /// exit; an `Err` item is a read failure the caller must
    /// propagate.
    pub async fn recv(&mut self) -> Option<Result<Event, io::Error>> {
        self.receiver.recv().await
    }

    /// Take one already-arrived event without waiting.
    ///
    /// `None` when nothing new has landed since the last take; safe
    /// to call in a loop to drain a burst.
    pub fn poll(&mut self) -> Option<Result<Event, io::Error>> {
        self.receiver.try_recv().ok()
    }
}

/// Deliver a terminal read failure and end the reader.
///
/// The send's result is deliberately unread — the receiver may
/// already be gone, in which case the channel close carries the
/// same news — so the thread leaves quietly either way.
fn report_failure(
    sender: &tokio::sync::mpsc::UnboundedSender<Result<Event, io::Error>>,
    err: io::Error,
) {
    drop(sender.send(Err(err)));
}

impl Drop for TerminalEvents {
    /// Signal the reader to stop.
    ///
    /// The thread notices at its next poll boundary; the failed
    /// send that follows a receiver drop is the belt to these
    /// braces.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::missing_panics_doc)]
    use super::*;

    #[test]
    fn a_readerless_quiesce_declines_within_its_deadline() {
        // A quiesce with no reader behind it can never be acknowledged:
        // the asker must read that as "stdin is still contested" and
        // decline to query, never as a green light.
        let quiesce = InputQuiesce::new();
        quiesce.request();
        let acked = quiesce.wait_paused(Duration::from_millis(10));
        quiesce.release();
        assert!(
            !acked,
            "no reader means no acknowledgement; the caller defers instead of querying"
        );
    }

    #[test]
    fn a_parked_reader_acknowledges_the_pause_and_resumes() {
        // The handshake a live reader performs, driven through the
        // reader's own park step: the request parks it, the
        // acknowledgement is the caller's proof stdin is uncontested,
        // and the release lets it resume — the whole exchange is what
        // a theme switch does around its one background query.
        let quiesce = Arc::new(InputQuiesce::new());
        quiesce.request();
        let reader_quiesce = Arc::clone(&quiesce);
        let running = Arc::new(AtomicBool::new(false));
        let reader_running = Arc::clone(&running);
        let reader = std::thread::spawn(move || {
            park_while_requested(&reader_quiesce, &reader_running);
        });
        let acked = quiesce.wait_paused(Duration::from_millis(500));
        assert!(acked, "the parked reader acknowledges within the deadline");
        quiesce.release();
        assert!(
            reader.join().is_ok(),
            "the parked reader resumes after the release"
        );
        assert!(
            !quiesce.paused.load(Ordering::SeqCst),
            "the acknowledgement is withdrawn on the way out"
        );
    }
}
