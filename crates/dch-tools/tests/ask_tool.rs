//! Integration tests for the `AskUserQuestion` tool.
//!
//! Every case drives the public surface only — `Tool::call` against a
//! hand-built `ToolContext` whose runner context carries a real question
//! channel — with the test holding the receiver the UI would own and
//! resolving each response channel by hand. No loop, no TUI, no terminal.

use dch_tools::AskTool;
use dch_tools::context::RunnerContext;
use dch_tools::question::QuestionRequest;
use dch_tools::question::QuestionResponse;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use serde_json::Value;
use serde_json::json;

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::indexing_slicing,
    clippy::redundant_closure_for_method_calls
)]
mod cases {
    use super::*;

    /// Build a context with a live question channel; return the context and
    /// the receiver the UI would hold.
    fn ctx_with_channel(
        is_non_interactive: bool,
    ) -> (ToolContext, std::sync::mpsc::Receiver<QuestionRequest>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let runner = RunnerContext::new(std::path::PathBuf::from("/tmp")).with_question_tx(tx);
        let mut ctx = ToolContext::default();
        ctx.set_extension(runner);
        ctx.is_non_interactive = is_non_interactive;
        (ctx, rx)
    }

    /// A minimal single-question input.
    fn input(question: &str, labels: &[&str]) -> Value {
        let options: Vec<_> = labels
            .iter()
            .map(|label| json!({ "label": label }))
            .collect();
        json!({ "questions": [ { "question": question, "options": options } ] })
    }

    /// Drive the tool and return its result.
    async fn ask(input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        AskTool.call(input, ctx).await
    }

    /// Answer the request's single question with `answers`.
    fn answer_all(request: QuestionRequest, answers: &[&str]) {
        for question in request.questions {
            let answers: Vec<String> = answers.iter().map(|a| (*a).to_string()).collect();
            question
                .response_tx
                .send(QuestionResponse {
                    question: question.question,
                    answers,
                })
                .expect("receiver alive");
        }
    }

    #[test]
    fn schema_matches_the_spec_shape() {
        let schema = AskTool.schema();
        let input = schema.input_schema;
        assert_eq!(input.get("type").and_then(Value::as_str), Some("object"));
        let required = input.get("required").and_then(Value::as_array).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "questions");
        let item_required = input
            .pointer("/properties/questions/items/required")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(item_required.len(), 2);
        assert!(item_required.contains(&json!("question")));
        assert!(item_required.contains(&json!("options")));
        let label_required = input
            .pointer("/properties/questions/items/properties/options/items/required")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(label_required.len(), 1);
        assert_eq!(label_required[0], "label");
    }

    #[test]
    fn category_flags_are_false_and_the_name_matches() {
        assert_eq!(AskTool.name(), "AskUserQuestion");
        assert!(!AskTool.is_concurrency_safe());
        assert!(!AskTool.is_read_only());
        let reg = dch_tools::builtin_registry();
        assert!(reg.get("AskUserQuestion").is_some(), "registered");
    }

    #[tokio::test]
    async fn single_question_round_trips_the_selected_label() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            let question = request.questions.first().expect("one question");
            assert_eq!(question.question, "Proceed?");
            assert_eq!(question.options.len(), 2);
            assert_eq!(question.options[0].label, "Yes");
            assert_eq!(question.options[1].label, "No");
            assert!(!question.multi_select);
            answer_all(request, &["Yes"]);
        });

        let out = ask(input("Proceed?", &["Yes", "No"]), &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        let text = out.text_content();
        assert!(text.contains("Proceed?"), "question echoed: {text}");
        assert!(text.contains("Yes"), "answer present: {text}");
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_questions_answer_in_input_order() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            assert_eq!(request.questions.len(), 2);
            // Answer the second question first: the tool awaits in input
            // order, so the output must still list Q1 before Q2.
            let mut questions = request.questions.into_iter();
            let first = questions.next().expect("Q1");
            let second = questions.next().expect("Q2");
            second
                .response_tx
                .send(QuestionResponse {
                    question: "Second?".to_string(),
                    answers: vec!["two".to_string()],
                })
                .expect("Q2 receiver alive");
            first
                .response_tx
                .send(QuestionResponse {
                    question: "First?".to_string(),
                    answers: vec!["one".to_string()],
                })
                .expect("Q1 receiver alive");
        });

        let two = json!({
            "questions": [
                { "question": "First?", "options": [ { "label": "one" } ] },
                { "question": "Second?", "options": [ { "label": "two" } ] }
            ]
        });
        let out = ask(two, &ctx).await.unwrap();
        let text = out.text_content();
        let first = text.find("First?").expect("Q1 present");
        let second = text.find("Second?").expect("Q2 present");
        assert!(first < second, "input order preserved: {text}");
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn multi_select_answers_join_with_commas() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            let question = request.questions.first().expect("one question");
            assert!(question.multi_select, "flag round-trips");
            answer_all(request, &["A", "B"]);
        });

        let body = json!({
            "questions": [
                {
                    "question": "Select features",
                    "options": [ { "label": "A" }, { "label": "B" } ],
                    "multi_select": true
                }
            ]
        });
        let out = ask(body, &ctx).await.unwrap();
        assert!(out.text_content().contains("A, B"), "comma-joined");
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn header_is_optional_and_round_trips() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            let question = request.questions.first().expect("one question");
            assert_eq!(question.header.as_deref(), Some("Test"));
            answer_all(request, &["Yes"]);
        });

        let with_header = json!({
            "questions": [
                {
                    "question": "With header?",
                    "header": "Test",
                    "options": [ { "label": "Yes" } ]
                }
            ]
        });
        ask(with_header, &ctx).await.unwrap();
        driver.await.unwrap();

        let (ctx2, rx2) = ctx_with_channel(false);
        let driver2 = tokio::spawn(async move {
            let request = rx2.recv().expect("request arrives");
            let question = request.questions.first().expect("one question");
            assert_eq!(question.header, None);
            answer_all(request, &["Yes"]);
        });
        ask(input("No header?", &["Yes"]), &ctx2).await.unwrap();
        driver2.await.unwrap();
    }

    #[tokio::test]
    async fn non_interactive_mode_returns_a_soft_error_without_sending() {
        let (ctx, rx) = ctx_with_channel(true);
        let out = ask(input("Q?", &["a"]), &ctx).await.unwrap();
        assert!(out.is_error, "soft error");
        assert!(
            out.text_content().contains("non-interactive"),
            "{}",
            out.text_content()
        );
        assert!(rx.try_recv().is_err(), "nothing was sent on the channel");
    }

    #[tokio::test]
    async fn missing_question_channel_returns_a_soft_error() {
        let runner = RunnerContext::new(std::path::PathBuf::from("/tmp"));
        let mut ctx = ToolContext::default();
        ctx.set_extension(runner);
        ctx.is_non_interactive = false;

        let out = ask(input("Q?", &["a"]), &ctx).await.unwrap();
        assert!(out.is_error, "soft error");
        assert!(
            out.text_content().contains("none is available"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn dropped_response_channel_means_dismissed() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            for question in request.questions {
                drop(question.response_tx);
            }
        });

        let out = ask(input("Q?", &["a"]), &ctx).await.unwrap();
        assert!(out.is_error, "soft error");
        assert!(
            out.text_content().contains("dismissed"),
            "{}",
            out.text_content()
        );
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn empty_questions_array_is_a_soft_error() {
        let (ctx, _rx) = ctx_with_channel(false);
        let out = ask(json!({ "questions": [] }), &ctx).await.unwrap();
        assert!(out.is_error, "soft error");
        assert!(
            out.text_content().contains("At least one question"),
            "{}",
            out.text_content()
        );
    }

    #[tokio::test]
    async fn empty_options_is_a_soft_error_naming_the_question() {
        let (ctx, _rx) = ctx_with_channel(false);
        let body = json!({
            "questions": [ { "question": "Lonely?", "options": [] } ]
        });
        let out = ask(body, &ctx).await.unwrap();
        assert!(out.is_error, "soft error");
        let text = out.text_content();
        assert!(text.contains("Lonely?"), "names the question: {text}");
        assert!(text.contains("at least one option"), "{text}");
    }

    #[tokio::test]
    async fn malformed_input_is_a_structural_error() {
        let (ctx, _rx) = ctx_with_channel(false);
        for bad in [
            json!({}),
            json!({ "questions": "not an array" }),
            json!({ "questions": [ { "options": [ { "label": "a" } ] } ] }),
            json!({ "questions": [ { "question": "Q?", "options": "nope" } ] }),
            json!({ "questions": [ { "question": "Q?", "options": [ { "hint": "x" } ] } ] }),
        ] {
            let err = ask(bad, &ctx).await.unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidInput(_)),
                "structural fault: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn camel_case_multi_select_is_accepted() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            let question = request.questions.first().expect("one question");
            assert!(question.multi_select, "camelCase accepted");
            answer_all(request, &["a"]);
        });

        let body = json!({
            "questions": [
                {
                    "question": "Q?",
                    "options": [ { "label": "a" } ],
                    "multiSelect": true
                }
            ]
        });
        let out = ask(body, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.text_content());
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn closed_channel_is_a_structural_execution_error() {
        let (ctx, rx) = ctx_with_channel(false);
        drop(rx);

        let err = ask(input("Q?", &["a"]), &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref s) if s.contains("closed")),
            "structural failure distinct from headless: {err:?}"
        );
    }

    #[test]
    fn the_boxed_future_is_send() {
        fn assert_send<T: Send>(_future: &T) {}

        let (ctx, _rx) = ctx_with_channel(false);
        let future = AskTool.call(input("Q?", &["a"]), &ctx);
        assert_send(&future);
        // Hold `ctx` beyond the future so the borrow the future carries is
        // exercised for its whole lifetime, not dropped early.
        drop(future);
        drop(ctx);
    }

    #[tokio::test]
    async fn empty_answers_mean_dismissed() {
        let (ctx, rx) = ctx_with_channel(false);
        let driver = tokio::spawn(async move {
            let request = rx.recv().expect("request arrives");
            for question in request.questions {
                question
                    .response_tx
                    .send(QuestionResponse {
                        question: question.question,
                        answers: Vec::new(),
                    })
                    .expect("receiver alive");
            }
        });

        let out = ask(input("Q?", &["a"]), &ctx).await.unwrap();
        assert!(out.is_error, "soft error");
        assert!(
            out.text_content().contains("dismissed"),
            "{}",
            out.text_content()
        );
        driver.await.unwrap();
    }
}
