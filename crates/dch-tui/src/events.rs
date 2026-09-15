//! The terminal event source.
//!
//! [`TerminalEvents`] owns the one thread that reads the terminal
//! and hands every event to the app's event loop over a channel —
//! the blocking-read path is used because crossterm's async
//! `EventStream` does not reliably wake a parked tokio runtime, and
//! a missed wakeup surfaces as input latency until something else
//! stirs the loop.

use crossterm::event::Event;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// The terminal's events, read on a dedicated thread.
///
/// Constructing spawns the reader; it blocks in
/// `crossterm::event::read()` for the process's whole lifetime and
/// forwards each event into the channel. The async [`recv`](Self::recv)
/// feeds a `select!` arm directly, and [`poll`](Self::poll) drains
/// whatever has already arrived so a burst of input applies to the
/// state before the frame that answers it — a fast typist or a
/// touchpad's wheel momentum renders once, not once per event.
///
/// The thread ends with the channel: when the receiver drops, the
/// next send fails and the loop exits.
pub struct TerminalEvents {
    /// The receiving half of the reader thread's channel.
    ///
    /// Owned here so the source is the single face of terminal
    /// input: the app loop awaits and polls through it, and
    /// dropping the source is what ends the reader thread.
    receiver: UnboundedReceiver<Event>,
}

impl TerminalEvents {
    /// Spawn the reader thread and return the event source.
    ///
    /// The one construction path — the thread starts reading the
    /// terminal immediately and lives until the returned source
    /// drops, so an owner that stops selecting on events also stops
    /// the reader.
    #[must_use]
    pub fn spawn() -> Self {
        let (sender, receiver) = unbounded_channel();
        std::thread::spawn(move || {
            while let Ok(event) = crossterm::event::read() {
                if sender.send(event).is_err() {
                    break;
                }
            }
        });
        Self { receiver }
    }

    /// Await the next event.
    ///
    /// The channel's end — the reader thread gone — reports as
    /// `None`, which the caller treats as a terminal exit.
    pub async fn recv(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }

    /// Take one already-arrived event without waiting.
    ///
    /// `None` when nothing new has landed since the last take; safe
    /// to call in a loop to drain a burst.
    pub fn poll(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }
}
