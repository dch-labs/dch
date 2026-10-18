//! The `AskUserQuestion` tool — asks the user multiple-choice questions and
//! awaits their answers.

use std::future::Future;
use std::pin::Pin;

use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;

use crate::context::runner_ctx;
use crate::question::Question;
use crate::question::QuestionOption;
use crate::question::QuestionRequest;
use crate::question::QuestionResponse;

/// Ask the user for clarification or input during execution.
///
/// The tool sends one batched [`QuestionRequest`] over the runner context's
/// question channel and awaits each question's response channel, blocking
/// its dispatch until the user answers — so calls are neither read-only
/// (they carry a UX side-effect) nor concurrency-safe (unbounded human
/// wait time; two overlapping asks would stack two overlays). In a
/// non-interactive session, or when no UI is wired to the channel, the
/// call returns a soft error telling the model to proceed without asking.
pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &'static str {
        "AskUserQuestion"
    }

    fn description(&self) -> &'static str {
        "Ask the user for clarification or input during execution. Use this when \
         you need additional information from the user to proceed with a task. \
         The tool will present options to the user and return their selection(s)."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "One or more questions to ask the user. Each \
                                        question can have multiple options for the \
                                        user to choose from.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "The question text to display to the user"
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Optional short header/label for the question (max 12 characters)",
                                    "maxLength": 12
                                },
                                "options": {
                                    "type": "array",
                                    "description": "Options for the user to choose from",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "The display text for this option"
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "Optional description explaining what this option means"
                                            }
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi_select": {
                                    "type": "boolean",
                                    "description": "Whether the user can select multiple options. Defaults to false.",
                                    "default": false
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        )
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let interactive = !ctx.is_non_interactive;
        let question_tx = runner_ctx(ctx).and_then(|rc| {
            rc.question_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        });
        Box::pin(self.call_inner(input, interactive, question_tx))
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl AskTool {
    /// Body of [`Tool::call`].
    ///
    /// Receives everything extracted from the context up front — the
    /// interactivity flag and the cloned question sender — so the future
    /// holds only owned data and no context borrow crosses an await.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] for structurally malformed input.
    /// Every other outcome the model can act on is a soft `is_error` result
    /// rather than an error: non-interactive mode, no UI installed, a
    /// question left unanswered, and a channel whose reader is already
    /// gone — a send that cannot land is that same dismissal, since
    /// no user was ever shown the question. An answer wait also ends on the
    /// asking channel's own liveness: once the receiver is gone no answer
    /// can arrive, and the vanished reader answers with that same dismissal
    /// rather than a hung turn. A host that keeps the receiver alive without
    /// ever polling it keeps the question waiting too — the display being
    /// alive and reading is the contract a wait depends on.
    async fn call_inner(
        &self,
        input: Value,
        interactive: bool,
        question_tx: Option<tokio::sync::mpsc::UnboundedSender<QuestionRequest>>,
    ) -> Result<ToolOutput, ToolError> {
        let parsed = match parse_questions(&input)? {
            ParseOutcome::Parsed(parsed) => parsed,
            ParseOutcome::Soft(output) => return Ok(output),
        };

        if !interactive {
            return Ok(ToolOutput::error_text(
                "Cannot ask questions in non-interactive mode. Provide the \
                 required information directly, or run in interactive mode.",
            ));
        }
        let Some(question_tx) = question_tx else {
            return Ok(ToolOutput::error_text(
                "AskUserQuestion requires an interactive UI, but none is \
                 available (headless mode). Provide the required information \
                 directly.",
            ));
        };

        if question_tx.send(parsed.request).is_err() {
            return Ok(dismissed());
        }

        let total = parsed.responses.len();
        let mut lines = Vec::with_capacity(total);
        for response in parsed.responses {
            let answered = tokio::select! {
                // Priority order is load-bearing: an answer that has
                // arrived and a channel that has closed can be ready
                // in the same poll, and the answer must win — the
                // model's context loses the answer, not the turn.
                biased;
                outcome = response => outcome,
                () = question_tx.closed() => return Ok(unanswered_report(&lines, total)),
            };
            match answered {
                Ok(resp) if !resp.answers.is_empty() => lines.push(format_response(&resp)),
                Ok(_) | Err(_) => {}
            }
        }
        if lines.len() == total {
            return Ok(ToolOutput::text(lines.join("\n\n")));
        }
        Ok(unanswered_report(&lines, total))
    }
}

/// How a batch the user did not complete reads back.
///
/// Nothing answered at all is the plain case, and [`dismissed`] is the
/// whole of it: there is nothing there to keep. Something answered is a
/// different picture — the user chose some rows and passed over others —
/// and reporting only a dismissal would erase the part of the exchange
/// that happened. So what was answered stands in front of the model, and
/// the gap is counted, softly: the ask did not complete, and the model
/// knows how far it got.
fn unanswered_report(lines: &[String], total: usize) -> ToolOutput {
    if lines.is_empty() {
        return dismissed();
    }
    let rest = total.saturating_sub(lines.len());
    let text = format!(
        "{}\n\nThe remaining {rest} of {total} questions went unanswered.",
        lines.join("\n\n")
    );
    ToolOutput::error_text(text)
}

/// The soft answer for every way a question can end without one.
///
/// Every path that leaves a batch with nothing in it — the host's `Esc`
/// on every question, a retired prompt, a reply channel dropped
/// unanswered, a send that could not land, an asking channel whose
/// receiver vanished mid-wait — reads the same to the model however it
/// arrived: nothing was answered, and the sound move is to proceed
/// without asking. A batch answered *partly* reads differently, through
/// [`unanswered_report`]: dismissal is per question, so what the user did
/// answer is kept.
fn dismissed() -> ToolOutput {
    ToolOutput::error_text("The question was dismissed before the user answered.")
}

/// The outcome of parsing the input's `questions` array.
///
/// Separates the two ways a parse can stop before a request is sent: a
/// fully-formed batch ready to go, and a soft rejection the model can fix
/// and retry without the call counting as a failure.
enum ParseOutcome {
    /// Every question parsed; the batch is ready to send.
    ///
    /// Carries the request plus the ordered response receivers so the
    /// send and the await phases share one parse pass — the questions and
    /// their response channels are created together and never rebuilt.
    Parsed(Parsed),

    /// A soft rejection the model can self-correct from.
    ///
    /// Used for an empty `questions` array or a question with no options:
    /// the shape is recognizable, the intent fixable, so the model gets an
    /// `is_error` result rather than a structurally failed call.
    Soft(ToolOutput),
}

/// A parsed request plus the ordered response receivers to await.
///
/// The two halves of one protocol exchange: `request` moves into the
/// channel send, and each receiver in `responses` resolves when the UI
/// answers its corresponding question. Ordering is load-bearing — the
/// receiver at position `i` belongs to the question at position `i`, so
/// the sequential await lines answers up with the input order.
struct Parsed {
    /// The batched request to send through the question channel.
    ///
    /// One [`QuestionRequest`] carrying every parsed question, each with
    /// its own response sender already embedded; sending moves the whole
    /// batch to the UI in the order the model wrote it.
    request: QuestionRequest,

    /// One receiver per question, in input order.
    ///
    /// Awaited sequentially after the send; position `i` resolves when the
    /// UI answers question `i`, which is what keeps multi-question output
    /// ordered regardless of the order the UI answers in.
    responses: Vec<oneshot::Receiver<QuestionResponse>>,
}

/// Parse the `questions` array, minting one response channel per question.
///
/// Structural faults — a missing `questions`/`question`/`options` entry, or
/// an option without a `label` — are [`ToolError::InvalidInput`]. An empty
/// `questions` array or a question with an empty `options` array is a soft
/// rejection instead: the model can fix the call and retry, so it gets an
/// `is_error` result rather than a failed one.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the input's shape is wrong.
fn parse_questions(input: &Value) -> Result<ParseOutcome, ToolError> {
    let Some(questions_value) = input.get("questions").and_then(Value::as_array) else {
        return Err(ToolError::InvalidInput(
            "Missing 'questions' array".to_string(),
        ));
    };
    if questions_value.is_empty() {
        return Ok(ParseOutcome::Soft(ToolOutput::error_text(
            "At least one question is required",
        )));
    }

    let mut questions = Vec::with_capacity(questions_value.len());
    let mut responses = Vec::with_capacity(questions_value.len());
    for question_value in questions_value {
        let question_str = question_value
            .get("question")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("Missing 'question' string".to_string()))?;
        let header = question_value
            .get("header")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(options_value) = question_value.get("options").and_then(Value::as_array) else {
            return Err(ToolError::InvalidInput(
                "Missing 'options' array".to_string(),
            ));
        };
        if options_value.is_empty() {
            return Ok(ParseOutcome::Soft(ToolOutput::error_text(format!(
                "Question '{question_str}' must have at least one option"
            ))));
        }

        let mut options = Vec::with_capacity(options_value.len());
        for option_value in options_value {
            let label = option_value
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("Option missing 'label'".to_string()))?;
            let description = option_value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            options.push(QuestionOption {
                label: label.to_string(),
                description,
            });
        }

        let (response_tx, response_rx) = oneshot::channel();
        questions.push(Question {
            question: question_str.to_string(),
            header,
            options,
            multi_select: read_multi_select(question_value),
            response_tx,
        });
        responses.push(response_rx);
    }
    Ok(ParseOutcome::Parsed(Parsed {
        request: QuestionRequest { questions },
        responses,
    }))
}

/// Read a question's multi-select flag, tolerating casing drift.
///
/// The schema documents `multi_select`; models raised on camelCase schemas
/// sometimes emit `multiSelect`. Both spellings are accepted, and an absent
/// or non-boolean value means single-select.
fn read_multi_select(question_value: &Value) -> bool {
    let flag = question_value
        .get("multi_select")
        .or_else(|| question_value.get("multiSelect"));
    flag.and_then(Value::as_bool).unwrap_or(false)
}

/// Format one answered question for the model.
///
/// The question text is echoed alongside its answers so a multi-question
/// call's output stays self-correlating.
fn format_response(resp: &QuestionResponse) -> String {
    format!("{} → {}", resp.question, resp.answers.join(", "))
}
