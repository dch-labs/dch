//! The question prompt the TUI renders for the asking tool.
//!
//! The runner's question channel carries the tool crate's request
//! types; this module holds the display's own shape of one pending
//! question — the text, its options, and the one-shot the answer
//! travels back on — so the terminal layer stays independent of the
//! tool crate while the host converts between the two at the seam.

/// One question awaiting the user's answer.
///
/// The host derives these from the asking tool's batched requests and
/// lands them on the app's question channel; the overlay shows one at
/// a time, and the keys answer exactly once by sending the chosen
/// labels on [`reply`](Self::reply) — a send consumes it, which is
/// the prompt's single-answer contract. Dropping a prompt unanswered
/// closes its channel and the asking tool reads a dismissal.
#[derive(Debug)]
pub struct QuestionPrompt {
    /// The full question text, shown verbatim.
    ///
    /// What the model asked, complete and specific; the overlay does
    /// not truncate or reword it.
    pub question: String,

    /// Optional short label shown above the question.
    ///
    /// At most ~12 characters by the tool's schema contract; `None`
    /// hides the chip and the question leads the sheet.
    pub header: Option<String>,

    /// The selectable answers, in display order.
    ///
    /// One row each; the chosen labels flow back on the reply
    /// channel, so the order the user saw is the order the asking
    /// tool matches against.
    pub options: Vec<QuestionChoice>,

    /// Whether several options may be chosen.
    ///
    /// `false` confirms exactly one answer; `true` toggles any number
    /// and confirms the set.
    pub multi_select: bool,

    /// Where the answer goes.
    ///
    /// The asking tool awaits this one-shot; the overlay sends
    /// exactly once — Enter with a selection, or the empty vec for a
    /// dismissal — and then the channel closes.
    pub reply: tokio::sync::oneshot::Sender<Vec<String>>,
}

/// One selectable row of a [`QuestionPrompt`].
///
/// The label is both the display text and the value that flows back;
/// the optional description renders beneath it in a dimmer style so a
/// long explanation never competes with the pickable text.
#[derive(Debug, Clone)]
pub struct QuestionChoice {
    /// The answer text, shown and echoed back when chosen.
    ///
    /// The value the asking tool matches against; short enough to sit
    /// on one row.
    pub label: String,

    /// Optional longer explanation of this answer.
    ///
    /// States the trade-off or consequence so the user can decide
    /// without extra context; `None` hides the description row.
    pub description: Option<String>,
}
