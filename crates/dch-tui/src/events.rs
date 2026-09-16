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
        let thread_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
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
        Self { receiver, shutdown }
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
