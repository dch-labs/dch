//! The asking path composed: the asking tool, its channel, and the
//! bridge a host puts in front of it.
//!
//! The bridge's own tests pin its stream in isolation; these drive the
//! seams a session actually wires — the asking tool dispatching through a
//! runner context whose slot holds a bridged channel — and pin the whole
//! session-level claim: a prompt its host retires reads as a soft
//! dismissal to the model, and the questions behind it and every later
//! request still reach the display.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing
)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use dch_loop::PromptedQuestion;
use dch_loop::spawn_question_bridge;
use dch_tools::AskTool;
use dch_tools::RunnerContext;
use dch_tools::runner_ctx;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;

/// Wire one session's asking path.
///
/// Returns the dispatch context a tool sees and the prompt stream the host
/// reads: the asking channel goes into a runner context's slot, the bridge
/// takes over that channel's receiver, and the context is composed the way
/// the runner's injector composes it — a clone of the shared context beside
/// the live interactivity flag.
fn asking_path() -> (ToolContext, mpsc::UnboundedReceiver<PromptedQuestion>) {
    let (question_tx, question_rx) = mpsc::unbounded_channel();
    let runner = RunnerContext::new(PathBuf::from("/tmp")).with_question_tx(question_tx);
    let mut context = ToolContext::default();
    context.set_extension(runner);
    context.is_non_interactive = false;
    (context, spawn_question_bridge(question_rx))
}

/// A one-question ask input, as the model would send it.
fn one_question(question: &str) -> Value {
    json!({
        "questions": [
            { "question": question, "options": [ { "label": "yes" }, { "label": "no" } ] }
        ]
    })
}

/// Ask `question` through `context`, returning the pending call unresolved.
fn ask<'a>(
    context: &'a ToolContext,
    question: &str,
) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
    AskTool.call(one_question(question), context)
}

/// The next prompt the host is shown, failing the test if none arrives.
async fn recv_prompt(prompts: &mut mpsc::UnboundedReceiver<PromptedQuestion>) -> PromptedQuestion {
    tokio::time::timeout(std::time::Duration::from_secs(5), prompts.recv())
        .await
        .expect("the prompt reaches the host")
        .expect("the asking path stays open")
}

/// Answer a prompt with one chosen label.
fn answer(prompt: PromptedQuestion, label: &str) {
    prompt
        .reply
        .send(vec![label.to_string()])
        .expect("the asking tool awaits the reply");
}

#[tokio::test]
async fn an_ask_through_the_installed_channel_reaches_the_host_and_answers() {
    let (context, mut prompts) = asking_path();
    let input = json!({
        "questions": [
            {
                "question": "Proceed?",
                "header": "Ship",
                "options": [ { "label": "yes", "description": "now" }, { "label": "no" } ]
            }
        ]
    });

    // The ask and the host's answer have to be in flight together: the
    // tool sends its request when it is first polled, so the prompt can
    // only arrive while the call is being awaited.
    let (result, ()) = tokio::join!(AskTool.call(input, &context), async {
        let prompt = recv_prompt(&mut prompts).await;
        assert_eq!(prompt.question, "Proceed?");
        assert_eq!(prompt.header.as_deref(), Some("Ship"));
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].label, "yes");
        assert_eq!(prompt.options[0].description.as_deref(), Some("now"));
        assert!(!prompt.multi_select);
        answer(prompt, "yes");
    });

    let output = result.expect("the ask resolves");
    assert!(!output.is_error, "an answered ask succeeds");
    let text = output.text_content();
    assert!(
        text.contains("Proceed?") && text.contains("yes"),
        "the model sees its question and the answer: {text}"
    );
}

/// One walked queue, two questions: the batch has to arrive together
/// for the display to walk it at all, and the user is free to answer
/// the questions in the order they like — the second one first here.
#[tokio::test]
async fn an_ask_with_two_questions_reaches_the_host_as_one_queue() {
    let (context, mut prompts) = asking_path();
    let input = json!({
        "questions": [
            { "question": "first?", "options": [ { "label": "yes" }, { "label": "no" } ] },
            { "question": "second?", "options": [ { "label": "later" } ] }
        ]
    });

    let (result, ()) = tokio::join!(AskTool.call(input, &context), async {
        let first = recv_prompt(&mut prompts).await;
        let second = recv_prompt(&mut prompts).await;
        assert_eq!(first.question, "first?");
        assert_eq!(second.question, "second?");
        answer(second, "later");
        answer(first, "no");
    });

    let output = result.expect("the ask resolves");
    assert!(!output.is_error, "an answered ask succeeds");
    let text = output.text_content();
    assert!(
        text.contains("first? → no") && text.contains("second? → later"),
        "each question reads back with its own answer: {text}"
    );
    let first_at = text.find("first?").unwrap_or(usize::MAX);
    let second_at = text.find("second?").unwrap_or(usize::MAX);
    assert!(
        first_at < second_at,
        "the answers read back in the order the model asked: {text}"
    );
}

#[tokio::test]
async fn a_dismissed_question_leaves_the_rest_being_asked() {
    // A dismissal is one question's answer, not the batch's. The ask used
    // to end on the first empty one, which cost the whole exchange to a
    // stray keypress and told the model nothing about what had been
    // answered; the questions behind a dismissal are therefore still
    // asked, still live on the host's screen, and still answerable.
    let (context, mut prompts) = asking_path();
    let input = json!({
        "questions": [
            { "question": "first?", "options": [ { "label": "yes" } ] },
            { "question": "second?", "options": [ { "label": "later" } ] }
        ]
    });

    let (result, ()) = tokio::join!(AskTool.call(input, &context), async {
        let first = recv_prompt(&mut prompts).await;
        let second = recv_prompt(&mut prompts).await;
        first
            .reply
            .send(Vec::new())
            .expect("the bridge awaits the reply");
        // Give the bridge its beat, then look: the dismissal is the first
        // question's whole answer, so the second must still be waiting.
        for _ in 0..10 {
            assert!(
                !second.reply.is_closed(),
                "a dismissal elsewhere withdrew a question still being asked"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        answer(second, "later");
    });

    let out = result.expect("the ask resolves");
    assert!(out.is_error, "an incomplete ask is still a soft result");
    let text = out.text_content();
    assert!(
        text.contains("second? \u{2192} later"),
        "the question that was answered still counts: {text}"
    );
    assert!(
        text.contains("1 of 2"),
        "and the one that was dismissed is counted, not hidden: {text}"
    );
}

#[tokio::test]
async fn a_cancelled_ask_withdraws_its_prompt() {
    // What the withdrawal is actually for: the asking side is gone
    // mid-wait, so the prompt on the host's screen can never be read by
    // anyone again. A cancelled dispatch is that case — the tool's future
    // drops, its reply receiver with it, and the bridge stops waiting
    // rather than holding a dead question open. The closed reply channel
    // is how the host learns.
    let (context, mut prompts) = asking_path();
    let mut tool = ask(&context, "Cancel me?");
    let prompt = tokio::select! {
        biased;
        _ = tool.as_mut() => panic!("the ask answered itself before its prompt was shown"),
        p = recv_prompt(&mut prompts) => p,
    };

    drop(tool); // the dispatch is cancelled while the question is on screen

    for _ in 0..100 {
        if prompt.reply.is_closed() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the prompt stayed live after the ask asking it was cancelled");
}

#[tokio::test]
async fn a_retired_prompt_soft_dismisses_and_a_later_ask_still_reaches_the_host() {
    let (context, mut prompts) = asking_path();

    let (result, ()) = tokio::join!(ask(&context, "Proceed?"), async {
        let prompt = recv_prompt(&mut prompts).await;
        drop(prompt);
    });
    let retired = result.expect("the ask resolves");
    assert!(
        retired.is_error,
        "a prompt retired by its host is a soft refusal, not a fault"
    );
    let text = retired.text_content();
    assert!(
        text.contains("dismissed"),
        "the model is told it was dismissed: {text}"
    );

    let (result, ()) = tokio::join!(ask(&context, "Still worth asking?"), async {
        let prompt = recv_prompt(&mut prompts).await;
        assert_eq!(
            prompt.question, "Still worth asking?",
            "the stream survived the retirement"
        );
        answer(prompt, "yes");
    });
    let answered = result.expect("the later ask resolves");
    assert!(!answered.is_error, "{}", answered.text_content());
    assert!(
        answered.text_content().contains("yes"),
        "a later answer still reaches the model"
    );
}

#[tokio::test]
async fn an_ask_after_its_generation_retires_soft_dismisses() {
    let (question_tx, question_rx) = mpsc::unbounded_channel();
    let runner = RunnerContext::new(PathBuf::from("/tmp")).with_question_tx(question_tx);
    let mut context = ToolContext::default();
    context.set_extension(runner);
    context.is_non_interactive = false;
    let prompts = spawn_question_bridge(question_rx);

    // The host's prompt stream goes away, so the bridge retires its
    // generation and takes the reader with it — while the slot still holds
    // that generation's sender, which is what every later dispatch clones.
    drop(prompts);
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ask(&context, "Farewell?"),
    )
    .await
    .expect("the retirement settles the first ask")
    .expect("a retiring generation dismisses, it does not fault");
    assert!(first.is_error, "the first ask never reached a host");

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ask(&context, "Still there?"),
    )
    .await
    .expect("a send into a retired generation settles")
    .expect("the closed channel answers with the soft dismissal, never a tool error");
    assert!(second.is_error, "no user was ever shown the question");
    assert!(
        second.text_content().contains("dismissed"),
        "the model reads the dismissal: {}",
        second.text_content()
    );
}

#[tokio::test]
async fn a_dispatch_in_flight_across_the_generation_swap_soft_dismisses() {
    let (question_tx, question_rx) = mpsc::unbounded_channel();
    let runner = RunnerContext::new(PathBuf::from("/tmp")).with_question_tx(question_tx);
    let mut context = ToolContext::default();
    context.set_extension(runner);
    context.is_non_interactive = false;
    let prompts = spawn_question_bridge(question_rx);

    // The dispatch's sender clone is taken now, while its generation is
    // live; retirement and the slot's swap land underneath it before the
    // call is ever polled — the crossing a reinstall drives a dispatch
    // through.
    let pending = ask(&context, "Mid-flight?");

    drop(prompts);
    let (fresh_tx, fresh_rx) = mpsc::unbounded_channel();
    runner_ctx(&context)
        .expect("the session carries its runner context")
        .install_question_tx(fresh_tx);
    let mut fresh_prompts = spawn_question_bridge(fresh_rx);

    let dismissed = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .expect("the crossing dispatch settles; it does not ride the swap into a hang")
        .expect("a retired generation answers with the soft dismissal, never a fault");
    assert!(
        dismissed.is_error,
        "the question never reached a host across the swap"
    );
    assert!(
        dismissed.text_content().contains("dismissed"),
        "the model reads the dismissal through the swap: {}",
        dismissed.text_content()
    );

    let (result, ()) = tokio::join!(ask(&context, "Still asking?"), async {
        let prompt = recv_prompt(&mut fresh_prompts).await;
        assert_eq!(
            prompt.question, "Still asking?",
            "the new generation serves the questions behind the crossing"
        );
        answer(prompt, "yes");
    });
    let answered = result.expect("the later ask resolves");
    assert!(!answered.is_error, "{}", answered.text_content());
}
