//! Types for the interactive question protocol between tools and the UI.
//!
//! A tool that needs input from the user sends a [`QuestionRequest`] over the
//! runner context's channel; the UI answers via the per-question one-shot
//! channel.

/// A request from a tool to ask the user one or more questions.
///
/// Sent over the runner context's question channel; the UI receives the whole
/// batch so it can render the questions together. Each [`Question`] inside
/// carries its own response one-shot, so the UI answers them independently
/// and the asking tool awaits each.
pub struct QuestionRequest {
    /// The questions to present to the user. Sent as a batch so the UI can
    /// render them together; each carries its own response channel.
    pub questions: Vec<Question>,
}

/// A single question to put to the user.
///
/// One element of a [`QuestionRequest`]. The UI renders the `question` text
/// with the optional `header` chip, lists the `options`, and sends a
/// [`QuestionResponse`] back over `response_tx` once the user answers (or
/// dismisses). `multi_select` governs whether one or several options may be
/// chosen.
pub struct Question {
    /// The question text.
    ///
    /// The full prompt shown to the user. Should be a complete, specific
    /// question ending in `?`; the UI renders it verbatim.
    pub question: String,

    /// Optional short label shown above the question in the UI. Should be at
    /// most ~12 characters so it fits a chip/tag header.
    pub header: Option<String>,

    /// The selectable answers. Should be non-empty; the UI renders one option
    /// per row and returns the chosen [`QuestionOption::label`] values.
    pub options: Vec<QuestionOption>,

    /// Whether multiple options may be selected. When `false` the UI should
    /// return exactly one answer; when `true` it may return several.
    pub multi_select: bool,

    /// One-shot channel carrying the user's answer back to the asking tool.
    /// The tool awaits this; the UI sends exactly one [`QuestionResponse`]
    /// and then the channel closes.
    pub response_tx: tokio::sync::oneshot::Sender<QuestionResponse>,
}

/// One selectable answer for a [`Question`].
///
/// Rendered as a single row in the UI: the `label` is the primary text the
/// user picks, the optional `description` appears beneath it. When selected,
/// the `label` is what flows back in [`QuestionResponse::answers`].
#[derive(Debug, Clone)]
pub struct QuestionOption {
    /// The answer text, shown to the user and echoed back in
    /// [`QuestionResponse::answers`] when selected. This is the value the
    /// asking tool matches against.
    pub label: String,

    /// Optional longer explanation of this answer, shown beneath the label.
    ///
    /// Use it to state the trade-off or consequence of picking this option so
    /// the user can decide without extra context. `None` leaves the label
    /// alone; the UI hides the row.
    pub description: Option<String>,
}

/// The user's answer(s) to a single [`Question`].
///
/// Sent back over the question's `response_tx` one-shot. Carries the original
/// question text (so the asking tool correlates the answer to the question it
/// posed) plus the selected option labels. An empty `answers` vec signals the
/// user dismissed the question without choosing.
#[derive(Debug, Clone)]
pub struct QuestionResponse {
    /// The question text that was answered. Echoed back so the asking tool can
    /// correlate the response to the question it posed.
    pub question: String,

    /// The selected answer labels. Empty if the user dismissed the question,
    /// one entry for single-select, or several when `multi_select` was `true`.
    pub answers: Vec<String>,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn question_request_can_be_built() {
        let (tx, _rx) = tokio::sync::oneshot::channel::<QuestionResponse>();
        let _req = QuestionRequest {
            questions: vec![Question {
                question: "Continue?".to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "Yes".to_string(),
                    description: None,
                }],
                multi_select: false,
                response_tx: tx,
            }],
        };
    }

    #[test]
    fn response_round_trips_through_the_one_shot_channel() {
        // AskUserQuestion (T-38) awaits response_tx; the UI answers on it.
        // Verify the chosen labels come back verbatim and the question text is
        // echoed for correlation.
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<QuestionResponse>();
        let req = QuestionRequest {
            questions: vec![Question {
                question: "Which?".to_string(),
                header: None,
                options: vec![
                    QuestionOption {
                        label: "a".to_string(),
                        description: None,
                    },
                    QuestionOption {
                        label: "b".to_string(),
                        description: None,
                    },
                ],
                multi_select: true,
                response_tx: resp_tx,
            }],
        };
        assert_eq!(req.questions[0].question, "Which?");
        let sender = req
            .questions
            .into_iter()
            .next()
            .expect("one question")
            .response_tx;
        sender
            .send(QuestionResponse {
                question: "Which?".to_string(),
                answers: vec!["a".to_string(), "b".to_string()],
            })
            .expect("receiver alive");
        let resp = resp_rx.blocking_recv().expect("channel not dropped");
        assert_eq!(resp.question, "Which?");
        assert_eq!(resp.answers, vec!["a".to_string(), "b".to_string()]);
    }
}
