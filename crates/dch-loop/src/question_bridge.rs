//! Bridge from the asking tool's requests to a host's prompt stream.
//!
//! The asking tool's channel carries its own batched request type; a
//! display host wants prompts in display terms, one question each.
//! This module converts between the two: each batched request splits
//! into one prompt per question, delivered together in the batch's
//! order for the host to queue, and each answer returns over the
//! asking tool's per-question response channel.

use std::sync::Arc;

use dch_tools::QuestionRequest;
use dch_tools::RunnerContext;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;

/// One selectable row of a [`PromptedQuestion`].
///
/// The label is both the display text and the value that flows back
/// when chosen; the optional description explains the trade-off
/// beneath it.
#[derive(Debug, Clone)]
pub struct PromptOption {
    /// The answer text, shown and echoed back when chosen.
    ///
    /// The value the asking tool matches against; short enough to sit
    /// on one row.
    pub label: String,

    /// Optional longer explanation of this answer.
    ///
    /// States the consequence of picking this row so the user can
    /// decide without extra context; `None` hides the description.
    pub description: Option<String>,
}

/// One question handed to a display host for the user to answer.
///
/// The host renders the question with its options and sends the
/// chosen labels on [`reply`](Self::reply) exactly once — an empty
/// vec for a dismissal. Sending consumes the one-shot, which is the
/// prompt's single-answer contract; dropping it unanswered closes the
/// channel and reads as a dismissal on the asking side.
pub struct PromptedQuestion {
    /// The full question text, shown verbatim.
    ///
    /// What the model asked, complete and specific.
    pub question: String,

    /// Optional short label shown above the question.
    ///
    /// At most ~12 characters by the asking tool's schema contract;
    /// `None` hides the chip.
    pub header: Option<String>,

    /// The selectable answers, in display order.
    ///
    /// One row each; the order the user saw is the order the chosen
    /// labels flow back in.
    pub options: Vec<PromptOption>,

    /// Whether several options may be chosen.
    ///
    /// `false` confirms exactly one answer; `true` confirms the
    /// toggled set.
    pub multi_select: bool,

    /// Where the answer goes.
    ///
    /// The bridge awaits this one-shot and forwards the labels to the
    /// asking tool as its response. Should the asking tool stop waiting
    /// first, the bridge drops its half — a closed reply on this sender
    /// tells the host the question has been withdrawn, and a prompt whose
    /// answer no one will read should not stay on the screen.
    pub reply: oneshot::Sender<Vec<String>>,
}

/// Convert the asking tool's request stream into a host prompt stream.
///
/// Spawns the conversion task and returns the receiver a display host
/// watches: each batched request becomes one [`PromptedQuestion`] per
/// question, and the whole batch is delivered at once in the batch's
/// order. A host that shows one question at a time keeps the rest of
/// the batch in its own queue, so it can walk the batch and answer its
/// questions in any order — waiting here for each answer before
/// handing over the next prompt would leave such a queue holding only
/// the question already on screen, and its walk, its per-question
/// state, and its count of what still waits would all be dead weight.
/// Answers are forwarded in the batch's own order, which is the order
/// the asking tool reads them in. Each answer reaches the asking tool
/// as its response, an empty set included (the dismissal contract). A
/// prompt whose reply sender is dropped unanswered — a host that
/// retired the prompt out from under the task — is dismissed on that
/// same contract, and the stream carries on with the questions behind
/// it and with every later request. The withdrawal runs the other way
/// too: when the asking side stops waiting for a question — the batch
/// dismissed before the wait reached it — the bridge stops waiting with
/// it and drops that prompt's reply sender, which is how a host holding
/// the prompt on screen learns it is no longer an ask. The task ends only when the asking
/// side's channel closes or the host's receiver drops. A host receiver
/// that vanishes mid-batch closes every still-parked answer channel
/// unanswered, which the asking side reads as that same dismissal.
#[must_use]
pub fn spawn_question_bridge(
    mut requests: UnboundedReceiver<QuestionRequest>,
) -> UnboundedReceiver<PromptedQuestion> {
    let (prompts_tx, prompts_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let mut awaiting = Vec::with_capacity(request.questions.len());
            for question in request.questions {
                let (reply_tx, reply_rx) = oneshot::channel();
                let delivered = prompts_tx.send(PromptedQuestion {
                    question: question.question.clone(),
                    header: question.header.clone(),
                    options: question
                        .options
                        .iter()
                        .map(|option| PromptOption {
                            label: option.label.clone(),
                            description: option.description.clone(),
                        })
                        .collect(),
                    multi_select: question.multi_select,
                    reply: reply_tx,
                });
                if delivered.is_err() {
                    return;
                }
                awaiting.push((question.question, question.response_tx, reply_rx));
            }
            for (question_text, mut response_tx, mut reply_rx) in awaiting {
                let labels = tokio::select! {
                    biased;
                    labels = &mut reply_rx => labels.unwrap_or_default(),
                    () = response_tx.closed() => {
                        drop(reply_rx);
                        continue;
                    }
                };
                let response = dch_tools::QuestionResponse {
                    question: question_text,
                    answers: labels,
                };
                if response_tx.send(response).is_err() {
                    tracing::trace!("question response channel already closed");
                }
            }
        }
    });
    prompts_rx
}

/// A host's handle to one runner's asking path.
///
/// The handle exists so a host can keep the asking tool reachable for a
/// session's whole life without owning the runner: it shares the runner's
/// channel slot, and each [`install`](Self::install) puts a fresh asking
/// channel and its bridge in place, returning the prompt stream to show.
/// Reinstalling matters because a bridge ends when the channel it listens
/// on is replaced or dropped from under it — a session whose only bridge
/// died would otherwise answer every later question with a closed-channel
/// failure while still advertising the tool as available.
#[derive(Clone)]
pub struct QuestionPath {
    /// The runner context whose channel slot each generation installs into.
    ///
    /// Shared with the runner, so an installation here is what every tool
    /// dispatch reads as the asking channel, and interactivity flips to
    /// true again on the next call.
    context: Arc<RunnerContext>,
}

impl QuestionPath {
    /// The handle on `context`'s asking-channel slot.
    ///
    /// Crate-internal by design: a host reaches a path through
    /// [`Runner::question_path`](crate::Runner::question_path), which shares
    /// the runner's own context so that whatever this handle installs is what
    /// every tool dispatch reads.
    #[must_use]
    pub(crate) fn new(context: Arc<RunnerContext>) -> Self {
        Self { context }
    }

    /// Install a fresh asking channel with its bridge, returning that
    /// generation's prompt stream.
    ///
    /// The channel replaces whatever the slot held, and dropping the
    /// previous one closes the channel its bridge was parked on — so this
    /// is how a host retires a generation deliberately, not only how it
    /// starts one. The returned receiver yields prompts until that bridge
    /// ends: its receiver dropping, or the channel this call installed
    /// being replaced or dropped.
    #[must_use]
    pub fn install(&self) -> UnboundedReceiver<PromptedQuestion> {
        let (question_tx, question_rx) = tokio::sync::mpsc::unbounded_channel();
        self.context.install_question_tx(question_tx);
        spawn_question_bridge(question_rx)
    }

    /// Keep prompts flowing into `sink` for as long as its receiver lives.
    ///
    /// Installs the first generation before returning, so a host that starts
    /// its first run the moment this call ends already finds a live asking
    /// channel in the slot. The spawned task then forwards each prompt
    /// through `convert` and installs the next generation whenever the
    /// current stream closes — the reinstall is what keeps a display from
    /// ever sitting in a session that has quietly become unable to ask. The
    /// task ends when `sink`'s receiver is dropped, since from there is no
    /// display left to prompt.
    pub fn supervise<T>(
        &self,
        sink: tokio::sync::mpsc::UnboundedSender<T>,
        convert: impl Fn(PromptedQuestion) -> T + Send + 'static,
    ) -> tokio::task::JoinHandle<()>
    where
        T: Send + 'static,
    {
        let mut prompted = self.install();
        let path = self.clone();
        tokio::spawn(async move {
            loop {
                while let Some(next) = prompted.recv().await {
                    if sink.send(convert(next)).is_err() {
                        return;
                    }
                }
                if sink.is_closed() {
                    return;
                }
                prompted = path.install();
            }
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
mod tests {
    use super::*;
    use dch_tools::Question;
    use dch_tools::QuestionOption;

    /// One single-select question wired for an answer round trip.
    ///
    /// Returns the asking-side sender (kept alive by the caller), the
    /// receiver the bridge consumes, and the response channel the
    /// answer must arrive on — the full path a batch of one question
    /// travels.
    fn one_question() -> (
        tokio::sync::mpsc::UnboundedSender<QuestionRequest>,
        tokio::sync::mpsc::UnboundedReceiver<QuestionRequest>,
        tokio::sync::oneshot::Receiver<dch_tools::QuestionResponse>,
    ) {
        let (response_tx, response_rx) = oneshot::channel();
        let question = Question {
            question: "Deploy now?".to_string(),
            header: Some("Release".to_string()),
            options: vec![
                QuestionOption {
                    label: "yes".to_string(),
                    description: Some("ships immediately".to_string()),
                },
                QuestionOption {
                    label: "no".to_string(),
                    description: None,
                },
            ],
            multi_select: false,
            response_tx,
        };
        let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
        requests_tx
            .send(QuestionRequest {
                questions: vec![question],
            })
            .expect("the bridge task holds the receiver");
        (requests_tx, requests_rx, response_rx)
    }

    /// A batch of `count` questions, already handed to the bridge.
    ///
    /// Returns the asking-side sender (kept alive by the caller), the
    /// receiver the bridge consumes, and the response channels in the
    /// batch's own order — the full path a multi-question request
    /// travels.
    fn batch(
        count: usize,
    ) -> (
        tokio::sync::mpsc::UnboundedSender<QuestionRequest>,
        tokio::sync::mpsc::UnboundedReceiver<QuestionRequest>,
        std::collections::VecDeque<tokio::sync::oneshot::Receiver<dch_tools::QuestionResponse>>,
    ) {
        let (responses_tx, responses_rx): (Vec<_>, Vec<_>) =
            std::iter::repeat_with(oneshot::channel).take(count).unzip();
        let questions = responses_tx
            .into_iter()
            .enumerate()
            .map(|(index, response_tx)| Question {
                question: format!("question {index}?"),
                header: None,
                options: vec![QuestionOption {
                    label: "ok".to_string(),
                    description: None,
                }],
                multi_select: false,
                response_tx,
            })
            .collect();
        let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
        requests_tx
            .send(QuestionRequest { questions })
            .expect("the bridge task holds the receiver");
        (requests_tx, requests_rx, responses_rx.into())
    }

    /// One question for `requests_tx`, ready to send as its own request.
    ///
    /// Returns the request's single question beside the channel its
    /// answer must come back on.
    fn one_more(question: &str) -> (Question, oneshot::Receiver<dch_tools::QuestionResponse>) {
        let (response_tx, response_rx) = oneshot::channel();
        (
            Question {
                question: question.to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "ok".to_string(),
                    description: None,
                }],
                multi_select: false,
                response_tx,
            },
            response_rx,
        )
    }

    /// One prompt from `prompts`, failing the test if none arrives.
    async fn recv_prompt(prompts: &mut UnboundedReceiver<PromptedQuestion>) -> PromptedQuestion {
        tokio::time::timeout(std::time::Duration::from_secs(5), prompts.recv())
            .await
            .expect("the prompt arrives")
            .expect("the bridge task stays alive")
    }

    /// The answer forwarded on `response`, failing the test if none comes.
    async fn recv_answer(
        response: oneshot::Receiver<dch_tools::QuestionResponse>,
    ) -> dch_tools::QuestionResponse {
        tokio::time::timeout(std::time::Duration::from_secs(5), response)
            .await
            .expect("the answer forwards")
            .expect("the asking tool still waits")
    }

    #[tokio::test]
    async fn an_answer_round_trips_into_the_asking_tools_response() {
        let (requests_tx, requests_rx, response_rx) = one_question();
        let mut prompts = spawn_question_bridge(requests_rx);
        let _kept_alive = requests_tx;
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), prompts.recv())
            .await
            .expect("the prompt arrives")
            .expect("the bridge task stays alive");
        assert_eq!(prompt.question, "Deploy now?");
        assert_eq!(prompt.header.as_deref(), Some("Release"));
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(
            prompt
                .options
                .first()
                .and_then(|option| option.description.as_deref()),
            Some("ships immediately")
        );
        assert!(!prompt.multi_select);
        prompt
            .reply
            .send(vec!["yes".to_string()])
            .expect("the bridge awaits the reply");
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), response_rx)
            .await
            .expect("the answer forwards")
            .expect("the asking tool still waits");
        assert_eq!(response.question, "Deploy now?");
        assert_eq!(response.answers, vec!["yes".to_string()]);
    }

    #[tokio::test]
    async fn a_batch_delivers_every_question_before_any_is_answered() {
        // The whole batch reaches the host before any of it is answered.
        //
        // The display queues what it is handed and lets the user walk it,
        // so learning that a batch has more in it cannot depend on
        // answering the question already on screen.
        let (requests_tx, requests_rx, mut responses) = batch(2);
        let _kept_alive = requests_tx;
        let mut prompts = spawn_question_bridge(requests_rx);

        let first = recv_prompt(&mut prompts).await;
        assert_eq!(first.question, "question 0?");
        let second = recv_prompt(&mut prompts).await;
        assert_eq!(second.question, "question 1?");

        // Answered back to front: each question carries its own answer
        // channel, so the host's order is its own business, and the
        // forwarding follows the batch instead of the answering.
        second
            .reply
            .send(vec!["later".to_string()])
            .expect("the bridge awaits the reply");
        first
            .reply
            .send(vec!["ok".to_string()])
            .expect("the bridge awaits the reply");

        let answered = recv_answer(
            responses
                .pop_front()
                .expect("the first question's response channel"),
        )
        .await;
        assert_eq!(answered.question, "question 0?");
        assert_eq!(answered.answers, vec!["ok".to_string()]);
        let answered = recv_answer(
            responses
                .pop_front()
                .expect("the second question's response channel"),
        )
        .await;
        assert_eq!(answered.question, "question 1?");
        assert_eq!(answered.answers, vec!["later".to_string()]);
    }

    #[tokio::test]
    async fn a_dropped_reply_dismisses_its_question_and_delivers_the_next_one() {
        let (requests_tx, requests_rx, mut responses) = batch(2);
        let _kept_alive = requests_tx;
        let mut prompts = spawn_question_bridge(requests_rx);

        let retired = recv_prompt(&mut prompts).await;
        drop(retired.reply);

        let dismissed = recv_answer(
            responses
                .pop_front()
                .expect("the first question's response channel"),
        )
        .await;
        assert_eq!(dismissed.question, "question 0?");
        assert!(
            dismissed.answers.is_empty(),
            "a dropped reply answers the question as the dismissal contract: {dismissed:?}"
        );

        let second = recv_prompt(&mut prompts).await;
        assert_eq!(second.question, "question 1?");
        second
            .reply
            .send(vec!["ok".to_string()])
            .expect("the bridge awaits the reply");
        let answered = recv_answer(
            responses
                .pop_front()
                .expect("the second question's response channel"),
        )
        .await;
        assert_eq!(answered.answers, vec!["ok".to_string()]);
    }

    #[tokio::test]
    async fn a_dropped_reply_leaves_later_requests_reaching_the_host() {
        let (requests_tx, requests_rx, mut responses) = batch(1);
        let mut prompts = spawn_question_bridge(requests_rx);

        let retired = recv_prompt(&mut prompts).await;
        drop(retired.reply);
        let dismissed = recv_answer(
            responses
                .pop_front()
                .expect("the first question's response channel"),
        )
        .await;
        assert!(
            dismissed.answers.is_empty(),
            "the retired prompt reads as a dismissal: {dismissed:?}"
        );

        let (question, answer) = one_more("Still asking?");
        requests_tx
            .send(QuestionRequest {
                questions: vec![question],
            })
            .expect("the bridge still listens to the asking side");
        let later = recv_prompt(&mut prompts).await;
        assert_eq!(later.question, "Still asking?");
        later
            .reply
            .send(vec!["ok".to_string()])
            .expect("the bridge awaits the reply");
        assert_eq!(recv_answer(answer).await.answers, vec!["ok".to_string()]);
    }

    #[tokio::test]
    async fn a_dropped_prompt_stream_dismisses_the_unanswered_asks_and_ends_the_generation() {
        let (requests_tx, requests_rx, mut responses) = batch(2);
        drop(spawn_question_bridge(requests_rx));

        for _ in 0..2 {
            let response = responses.pop_front().expect("the batch's response channel");
            // The asking tool reads a closed response channel as its
            // dismissal result, so the dropped stream dismisses the ask
            // instead of stranding it.
            let dismissed = tokio::time::timeout(std::time::Duration::from_secs(5), response)
                .await
                .expect("the bridge drops the unanswered channel instead of parking");
            assert!(dismissed.is_err(), "no answer is sent: {dismissed:?}");
        }

        for _ in 0..500 {
            if requests_tx.is_closed() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the generation ends once its prompt stream is gone");
    }

    /// Ask `question` through whatever channel the slot holds right now.
    ///
    /// Returns the channel its answer comes back on, or `None` when the
    /// slot holds no channel whose reader is still alive.
    fn ask_through_the_slot(
        context: &RunnerContext,
        question: &str,
    ) -> Option<oneshot::Receiver<dch_tools::QuestionResponse>> {
        let slot = context
            .question_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sender = slot.clone()?;
        let (response_tx, response_rx) = oneshot::channel();
        sender
            .send(QuestionRequest {
                questions: vec![Question {
                    question: question.to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "ok".to_string(),
                        description: None,
                    }],
                    multi_select: false,
                    response_tx,
                }],
            })
            .ok()?;
        Some(response_rx)
    }

    /// Ask again until some generation's channel accepts the request.
    ///
    /// A reinstall lands on the supervisor's schedule, not the test's, so
    /// a case that retired a generation waits for the next one to reach
    /// the slot instead of assuming it is already there.
    async fn ask_once_a_generation_is_live(
        context: &RunnerContext,
        question: &str,
    ) -> oneshot::Receiver<dch_tools::QuestionResponse> {
        for _ in 0..200 {
            if let Some(answer) = ask_through_the_slot(context, question) {
                return answer;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("a supervised path installs a live asking channel");
    }

    #[tokio::test]
    async fn a_supervised_path_reinstalls_the_asking_channel_after_its_generation_ends() {
        let context = Arc::new(RunnerContext::new(std::path::PathBuf::from(".")));
        let path = QuestionPath::new(Arc::clone(&context));
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel();
        let _supervisor = path.supervise(sink_tx, |prompt| prompt);

        let first_answer = ask_through_the_slot(&context, "First?")
            .expect("the call that started supervision installed a live channel");
        let first = recv_prompt(&mut sink_rx).await;
        assert_eq!(first.question, "First?");

        let (retired_tx, retired_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(retired_rx);
        context.install_question_tx(retired_tx);
        drop(first);
        let dismissed = recv_answer(first_answer).await;
        assert!(
            dismissed.answers.is_empty(),
            "the retired generation dismisses the question it was parked on: {dismissed:?}"
        );

        let second_answer = ask_once_a_generation_is_live(&context, "Still asking?").await;
        let second = recv_prompt(&mut sink_rx).await;
        assert_eq!(second.question, "Still asking?");
        second
            .reply
            .send(vec!["ok".to_string()])
            .expect("the next generation awaits the reply");
        assert_eq!(
            recv_answer(second_answer).await.answers,
            vec!["ok".to_string()]
        );
    }
}
