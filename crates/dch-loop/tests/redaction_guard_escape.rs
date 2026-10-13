//! The overwrite guard's unresolvable-target refusals, through real
//! pipeline dispatches.
//!
//! A redirect target the shell expands — `~`, `$HOME/…`, `${VAR}…`, a
//! computed `$(…)` word, or a backticked substitution — names a file
//! the lexical scan cannot reach, so the guard refuses the command
//! instead of guessing. These pins dispatch a Bash-named probe through
//! the redaction pair and assert the refusal fires before any tool
//! runs, alongside the plain-path controls the earlier escape round
//! verified against the shell itself. A heredoc with a computed
//! delimiter belongs to the same class: the scan finds no terminator
//! it can match, so nothing past the body can be checked and the
//! refusal fires before the tool runs.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use dch_loop::redaction_guard::with_redaction;
use loopctl::cancel::CancelSignal;
use loopctl::middleware::ToolDispatchContext;
use loopctl::middleware::ToolPipeline;
use loopctl::tool::PermissionCheck;
use loopctl::tool::Tool;
use loopctl::tool::ToolContext;
use loopctl::tool::ToolError;
use loopctl::tool::ToolOutput;
use loopctl::tool::ToolRegistry;
use loopctl::tool::ToolSchema;
use serde_json::Value;
use serde_json::json;

/// A tool that records that it ran.
///
/// The guard matches the dispatch name, so one probe shape wearing
/// the shell's name routes through the redirect scan; the flag proves
/// whether the pipeline ever reached the tool.
struct BashProbe {
    /// Set when the probe's call runs.
    ran: Arc<std::sync::Mutex<bool>>,
}

impl Tool for BashProbe {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "shell probe"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new("Bash", "shell probe", json!({"type": "object"}))
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        _ctx: &ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>> {
        *self.ran.lock().expect("the probe flag") = true;
        Box::pin(async move { Ok(ToolOutput::text("probe ran")) })
    }
}

/// One dispatch context carrying `command` as the Bash input.
fn bash_dispatch(command: &str) -> ToolDispatchContext {
    ToolDispatchContext {
        tool_name: "Bash".to_string(),
        input: json!({"command": command}),
        call_id: "call_probe".to_string(),
        turn_number: 0,
        cancel: Arc::new(CancelSignal::new()),
        permission: PermissionCheck::Allow,
        tool_context: ToolContext::default(),
    }
}

/// A redacting pipeline over the recording probe.
fn pipeline(ran: Arc<std::sync::Mutex<bool>>) -> ToolPipeline {
    let mut registry = ToolRegistry::new();
    registry.register(BashProbe { ran });
    with_redaction(ToolPipeline::builder())
        .with_core(Arc::new(registry))
        .build()
        .expect("static composition builds")
}

#[tokio::test]
async fn shell_expanded_redirect_spellings_are_refused_before_the_tool_runs() {
    for command in [
        "printf x > $HOME/env.conf",
        "printf x > \"$HOME/env.conf\"",
        "printf x > ~/env.conf",
        "printf x > ${TARGET}/env.conf",
        "printf x > $TARGET",
        "cat a > $(echo out)",
    ] {
        let ran = Arc::new(std::sync::Mutex::new(false));
        let pipeline = pipeline(Arc::clone(&ran));
        let result = pipeline.dispatch(&mut bash_dispatch(command)).await;
        assert!(
            result.is_error,
            "the expanded spelling in `{command}` is refused, not dispatched"
        );
        let text = match result.output {
            loopctl::message::ToolContent::Text(text) => text,
            other @ loopctl::message::ToolContent::Multipart(_) => {
                panic!("the refusal is text: {other:?}")
            }
        };
        assert!(
            text.contains("shell-expanded spelling"),
            "the refusal names the unresolvable target: {text}"
        );
        assert!(
            !*ran.lock().expect("the probe flag"),
            "the tool never runs behind the refusal: `{command}`"
        );
    }
}

#[tokio::test]
async fn plain_relative_redirects_still_reach_the_tool() {
    let ran = Arc::new(std::sync::Mutex::new(false));
    let pipeline = pipeline(Arc::clone(&ran));
    let result = pipeline
        .dispatch(&mut bash_dispatch("printf x > env.conf"))
        .await;
    assert!(
        !result.is_error,
        "a plain path the guard can resolve is the tool's own dispatch"
    );
    assert!(
        *ran.lock().expect("the probe flag"),
        "the probe ran behind the clean redirect"
    );
}

#[tokio::test]
async fn a_backticked_redirect_target_refuses_before_the_tool_runs() {
    for command in [
        "printf x > `echo ./env.conf`",
        "printf x > `echo env.conf`",
        "printf x > out`id`.conf",
    ] {
        let ran = Arc::new(std::sync::Mutex::new(false));
        let pipeline = pipeline(Arc::clone(&ran));
        let result = pipeline.dispatch(&mut bash_dispatch(command)).await;
        assert!(
            result.is_error,
            "the backticked spelling in `{command}` is refused, not dispatched"
        );
        let text = match result.output {
            loopctl::message::ToolContent::Text(text) => text,
            other @ loopctl::message::ToolContent::Multipart(_) => {
                panic!("the refusal is text: {other:?}")
            }
        };
        assert!(
            text.contains("shell-expanded spelling"),
            "the refusal names the unresolvable target: {text}"
        );
        assert!(
            !*ran.lock().expect("the probe flag"),
            "the tool never runs behind the refusal: `{command}`"
        );
    }
}

#[tokio::test]
async fn a_computed_heredoc_delimiter_refuses_before_the_tool_runs() {
    let ran = Arc::new(std::sync::Mutex::new(false));
    let pipeline = pipeline(Arc::clone(&ran));
    let command = "cat <<$(echo EOF)\nbody\nEOF\necho x >> ./credentials";
    let result = pipeline.dispatch(&mut bash_dispatch(command)).await;
    assert!(
        result.is_error,
        "a delimiter the shell computes leaves no body boundary to check past, \
         so the command is refused, not dispatched"
    );
    let text = match result.output {
        loopctl::message::ToolContent::Text(text) => text,
        other @ loopctl::message::ToolContent::Multipart(_) => {
            panic!("the refusal is text: {other:?}")
        }
    };
    assert!(
        text.contains("heredoc") && text.contains("computed delimiter"),
        "the refusal names the heredoc it could not resolve: {text}"
    );
    assert!(
        !*ran.lock().expect("the probe flag"),
        "the tool never runs behind the refusal"
    );
}

#[tokio::test]
async fn a_plain_delimiter_heredoc_with_a_trailing_redirect_reaches_the_tool() {
    let ran = Arc::new(std::sync::Mutex::new(false));
    let pipeline = pipeline(Arc::clone(&ran));
    let result = pipeline
        .dispatch(&mut bash_dispatch(
            "cat <<EOF > out\nbody\nEOF\necho x >> later.txt",
        ))
        .await;
    let shown = result.output.to_string();
    assert!(
        !result.is_error,
        "a plain delimiter is a terminator the scan can match, so the command \
         keeps its route: {shown}"
    );
    assert!(
        *ran.lock().expect("the probe flag"),
        "the probe ran behind the ordinary heredoc"
    );
}
