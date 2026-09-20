//! Dispatch-side capture of full tool calls.
//!
//! The lifecycle events the display observes deliberately carry no
//! output text — a hash stands in for it — so the conversation's
//! expandable tool blocks need a second source: this middleware,
//! registered on the dispatch pipeline, records each call's full
//! input before it runs and its full (already redacted) output
//! after, keyed by the model-issued call id. Nothing here renders;
//! the shared map it writes is drained by the observer when a call
//! graduates.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use loopctl::message::{ToolContent, ToolContentPart};
use loopctl::middleware::{ToolDispatchContext, ToolMiddleware, ToolPipeline};
use loopctl::tool::ToolDispatchResult;

/// The shared capture store: call id to captured call.
///
/// One allocation with two holders — the capture middleware writes
/// into it from the dispatch pipeline while the observer side
/// drains entries as their calls graduate, so cloning the `Arc` is
/// the whole wiring. The mutex is never held across an await; a
/// poisoned lock is recovered by whichever side touches it next,
/// the display-side policy.
pub type ToolCaptureSink = Arc<Mutex<HashMap<String, ToolCapture>>>;

/// How many characters of a captured payload — output or input —
/// are retained for display.
///
/// Both halves can be arbitrarily large — a single read can return
/// hundreds of kilobytes of output, a single write a whole file of
/// input — while the expansion only needs to be readable, not
/// archival. The retained slice keeps the head and the tail; a
/// marker names what was cut.
const OUTPUT_RETAIN_CHARS: usize = 256 * 1024;

/// How many captures accumulate before the store drops.
///
/// Entries retire as their calls graduate; this bounds the ones
/// that never do (failed or cancelled calls) without ordering
/// machinery — the same policy the observer's summary stash uses.
const CAPTURE_CAP: usize = 256;

/// One tool call's full data, as captured at dispatch.
///
/// The input is recorded before the call runs so an expansion
/// opened while it runs shows the command immediately; the output
/// arrives when the dispatch completes.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCapture {
    /// The call's input, pretty-printed JSON, within the retention
    /// cap.
    ///
    /// One field per line, as the model supplied it; a call whose
    /// input exceeds the cap keeps its head and tail with a marker
    /// naming the cut, the same discipline the output follows.
    pub input_json: String,

    /// The call's output, as text, within the retention cap.
    ///
    /// What the dispatch pipeline returned — already redacted when
    /// redaction is on, since the redacting layer sits inside this
    /// one. Empty while the call runs.
    pub output: String,

    /// Whether the dispatch reported an error.
    ///
    /// Carried for parity with the lifecycle events, which is where
    /// the completed line's styling normally comes from; a reader
    /// holding only the capture can still tell a failed call's
    /// output from a successful one's.
    pub is_error: bool,

    /// Whether the call has completed.
    ///
    /// False between the pre-dispatch record and the result: an
    /// empty output alone cannot distinguish a still-running call
    /// from a quiet finished one, so the flag is what an expansion
    /// opened mid-run reads to say the output is pending rather
    /// than absent.
    pub done: bool,
}

/// Lock a mutex, recovering from poisoning.
///
/// A poisoned lock means some other thread panicked mid-mutation;
/// the capture must never take the dispatch down with it, so the
/// guard is recovered rather than the poison propagated — the same
/// policy the observer applies.
fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Retain a head and a tail of `text`, marking any cut.
///
/// Text within the cap passes through untouched; longer text keeps
/// its first and last halves with a marker naming the omitted
/// middle, so both what a call started with and what it ended on
/// stay readable.
#[must_use]
pub fn cap_output(text: &str) -> String {
    let total = text.chars().count();
    if total <= OUTPUT_RETAIN_CHARS {
        return text.to_string();
    }
    let half = OUTPUT_RETAIN_CHARS / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(total.saturating_sub(half)).collect();
    let omitted = total.saturating_sub(half.saturating_mul(2));
    format!("{head}\n… [{omitted} characters omitted] …\n{tail}")
}

/// Flatten a dispatch result's content to displayable text.
///
/// Multipart content joins its text parts with newlines; images
/// stand in as a named placeholder, so an expansion never silently
/// drops content it cannot show.
fn content_text(content: &ToolContent) -> String {
    match content {
        ToolContent::Text(text) => text.clone(),
        ToolContent::Multipart(parts) => parts
            .iter()
            .map(|part| match part {
                ToolContentPart::Text { text } => text.as_str(),
                ToolContentPart::Image { .. } => "[image]",
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Strip what a terminal would re-interpret, keeping the text.
///
/// Tool output can carry ANSI escape sequences and stray control
/// characters — colors and cursor moves from a child program, bytes
/// read out of a binary file. Rendered as cell symbols they reach
/// the terminal verbatim and re-execute mid-frame, corrupting the
/// layout in ways the app's frame diff can no longer match, so
/// stale characters survive every later repaint. The sanitizer
/// drops escape sequences whole, keeps line breaks, expands tabs,
/// and removes the remaining control characters.
fn sanitize_for_display(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(&ch) = chars.get(index) {
        index = index.saturating_add(1);
        match ch {
            '\x1b' => match chars.get(index) {
                Some('[') => {
                    index = index.saturating_add(1);
                    while let Some(&c) = chars.get(index) {
                        index = index.saturating_add(1);
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    index = index.saturating_add(1);
                    while let Some(&c) = chars.get(index) {
                        index = index.saturating_add(1);
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.get(index) == Some(&'\\') {
                            index = index.saturating_add(1);
                            break;
                        }
                    }
                }
                Some('P' | 'X' | '^' | '_') => {
                    index = index.saturating_add(1);
                    while let Some(&c) = chars.get(index) {
                        index = index.saturating_add(1);
                        if c == '\x1b' && chars.get(index) == Some(&'\\') {
                            index = index.saturating_add(1);
                            break;
                        }
                    }
                }
                // Any other escape: a run of intermediate bytes
                // (0x20–0x2f — charset designations, their
                // introducers) then one final byte.
                Some(_) => {
                    while chars
                        .get(index)
                        .is_some_and(|&c| ('\x20'..='\x2f').contains(&c))
                    {
                        index = index.saturating_add(1);
                    }
                    index = index.saturating_add(1);
                }
                None => {}
            },
            '\r' => {
                if chars.get(index) == Some(&'\n') {
                    index = index.saturating_add(1);
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            _ if ch < ' ' || ch == '\x7f' || ('\u{80}'..='\u{9f}').contains(&ch) => {}
            _ => out.push(ch),
        }
    }
    out
}

/// A dispatch middleware recording every tool call in full.
///
/// Wraps the pipeline from outside: the input is recorded before
/// the call runs and the result's output after it completes, both
/// keyed by the call id the engine reports. The captured output is
/// what the inner layers already redacted, so the display never
/// sees a secret the model was spared.
pub struct CapturingMiddleware {
    /// The store this middleware writes into.
    ///
    /// The same allocation the observer side drains from.
    sink: ToolCaptureSink,
}

impl CapturingMiddleware {
    /// Create a middleware writing into `sink`.
    ///
    /// The sink is the shared state's capture store, cloned by
    /// `Arc` so the middleware and the observer side share one
    /// allocation. One middleware instance serves the whole
    /// pipeline; every dispatched call passes through it.
    #[must_use]
    pub fn new(sink: ToolCaptureSink) -> Self {
        Self { sink }
    }

    /// Insert a capture, dropping the store wholesale at capacity.
    ///
    /// Bounded the way the observer's summary stash is bounded:
    /// entries retire on graduation, and the ones that never do are
    /// cleared together once they pile past the cap.
    fn record(&self, capture: ToolCapture, call_id: &str) {
        let mut sink = recover(&self.sink);
        if sink.len() >= CAPTURE_CAP {
            sink.clear();
        }
        sink.insert(call_id.to_string(), capture);
    }
}

impl ToolMiddleware for CapturingMiddleware {
    fn name(&self) -> &'static str {
        "tool-capture"
    }

    /// Record the call in two phases and never alter it.
    ///
    /// Before the tool runs, the call's pretty-printed input lands
    /// in the store with the output empty and the completion flag
    /// clear — the record an expansion opened mid-run reads. After
    /// the inner pipeline resolves, the same key is overwritten with
    /// the sanitized, capped output and the dispatch's own error
    /// flag. Both records key on the engine-reported call id, so a
    /// retried call simply replaces its own earlier capture, and the
    /// result passes through to the engine exactly as the inner
    /// layers produced it.
    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ToolDispatchContext,
        next: &'a ToolPipeline,
    ) -> Pin<Box<dyn Future<Output = ToolDispatchResult> + Send + 'a>> {
        Box::pin(async move {
            let call_id = ctx.call_id.clone();
            let input_json = cap_output(
                &serde_json::to_string_pretty(&ctx.input).unwrap_or_else(|_| ctx.input.to_string()),
            );
            self.record(
                ToolCapture {
                    input_json,
                    output: String::new(),
                    is_error: false,
                    done: false,
                },
                &call_id,
            );
            let result = next.dispatch(ctx).await;
            self.record(
                ToolCapture {
                    input_json: cap_output(
                        &serde_json::to_string_pretty(&ctx.input)
                            .unwrap_or_else(|_| ctx.input.to_string()),
                    ),
                    output: cap_output(&sanitize_for_display(&content_text(&result.output))),
                    is_error: result.is_error,
                    done: true,
                },
                &call_id,
            );
            result
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

    #[test]
    fn short_output_passes_the_cap_untouched() {
        assert_eq!(cap_output("hello"), "hello");
    }

    #[test]
    fn colored_output_strips_to_plain_text() {
        let raw = "\x1b[32mok\x1b[0m: \x1b[1;4m4 passed\x1b[24;22m";
        assert_eq!(sanitize_for_display(raw), "ok: 4 passed");
    }

    #[test]
    fn charset_sequences_drop_whole() {
        let raw = "a\x1b(Bb\x1b(0c\x1b($1d";
        assert_eq!(
            sanitize_for_display(raw),
            "abcd",
            "intermediate runs and their final byte drop, surrounding text survives"
        );
    }

    #[test]
    fn cursor_and_title_sequences_drop_whole() {
        let raw = "\x1b[2J\x1b[1;1Hline\x1b]0;title\x07after\x1bP+q\x1b\\end";
        assert_eq!(sanitize_for_display(raw), "lineafterend");
    }

    #[test]
    fn carriage_returns_and_tabs_read_as_structure() {
        assert_eq!(
            sanitize_for_display("a\r\nb"),
            "a\nb",
            "CRLF keeps one break"
        );
        assert_eq!(sanitize_for_display("a\rb"), "a\nb", "a lone CR is a break");
        assert_eq!(sanitize_for_display("a\tb"), "a    b", "a tab expands");
    }

    #[test]
    fn stray_control_characters_drop() {
        assert_eq!(sanitize_for_display("a\u{0}\u{7}b\u{7f}c\u{85}d"), "abcd");
        assert_eq!(
            sanitize_for_display("a\x1bb"),
            "a",
            "a bare ESC consumes one char, as terminals do"
        );
    }

    struct DyeTool {
        /// The store the middleware under test writes into.
        sink: ToolCaptureSink,
        /// What the tool itself observed mid-call: the pre-dispatch
        /// record, if any. Shared by `Arc` so the test reads it after
        /// the registry takes the tool instance.
        seen_mid_call: Arc<Mutex<Option<ToolCapture>>>,
    }

    impl loopctl::tool::Tool for DyeTool {
        fn name(&self) -> &'static str {
            "Dye"
        }
        fn description(&self) -> &'static str {
            "Emits colored text"
        }
        fn schema(&self) -> loopctl::tool::ToolSchema {
            loopctl::tool::ToolSchema {
                tool: "Dye".into(),
                description: "Emits colored text".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &loopctl::tool::ToolContext,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<loopctl::tool::ToolOutput, loopctl::tool::ToolError>>
                    + Send
                    + '_,
            >,
        > {
            let mid_call = self.sink.lock().expect("sink").get("call-x").cloned();
            *self.seen_mid_call.lock().expect("observer") = mid_call;
            Box::pin(async {
                Ok(loopctl::tool::ToolOutput::text(
                    "\x1b[31mred\x1b[0m text\ttail",
                ))
            })
        }
    }

    #[tokio::test]
    async fn the_capture_stores_what_the_redacting_layer_already_scrubbed() {
        // The capture wraps the pipeline from outside the redacting
        // layer, so the store holds redacted output: the display
        // never sees a secret the model was spared.
        struct LeakyTool;
        impl loopctl::tool::Tool for LeakyTool {
            fn name(&self) -> &'static str {
                "Leak"
            }
            fn description(&self) -> &'static str {
                "Emits a bearer header"
            }
            fn schema(&self) -> loopctl::tool::ToolSchema {
                loopctl::tool::ToolSchema {
                    tool: "Leak".into(),
                    description: "Emits a bearer header".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            fn call(
                &self,
                _input: serde_json::Value,
                _ctx: &loopctl::tool::ToolContext,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<loopctl::tool::ToolOutput, loopctl::tool::ToolError>>
                        + Send
                        + '_,
                >,
            > {
                Box::pin(async {
                    Ok(loopctl::tool::ToolOutput::text(
                        "Authorization: Bearer sk-super-secret-value",
                    ))
                })
            }
        }
        let sink: ToolCaptureSink = Arc::new(Mutex::new(HashMap::new()));
        let mut registry = loopctl::tool::ToolRegistry::new();
        registry.register(LeakyTool);
        let pipeline = ToolPipeline::builder()
            .with_middleware(CapturingMiddleware::new(Arc::clone(&sink)))
            .with_middleware(loopctl::middleware::RedactingMiddleware::new(
                loopctl::middleware::SecretPatternSet::default_common(),
            ))
            .with_core(Arc::new(registry))
            .build()
            .expect("the pipeline builds");
        let ctx = ToolDispatchContext {
            tool_name: "Leak".to_string(),
            input: serde_json::json!({}),
            call_id: "call-leak".to_string(),
            turn_number: 1,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::allow(),
            tool_context: loopctl::tool::ToolContext::default(),
        };
        let result = pipeline.invoke(ctx).await;
        assert!(!result.is_error);
        let capture = sink
            .lock()
            .expect("sink")
            .get("call-leak")
            .cloned()
            .expect("the call was captured");
        assert!(
            !capture.output.contains("sk-super-secret-value"),
            "the secret never reaches the store: {}",
            capture.output
        );
        assert!(
            capture.output.contains("[REDACTED:"),
            "the redacted placeholder is what the expansion will show: {}",
            capture.output
        );
    }

    #[tokio::test]
    async fn the_middleware_records_the_input_first_and_the_sanitized_output_last() {
        let sink: ToolCaptureSink = Arc::new(Mutex::new(HashMap::new()));
        let seen_mid_call: Arc<Mutex<Option<ToolCapture>>> = Arc::new(Mutex::new(None));
        let mut registry = loopctl::tool::ToolRegistry::new();
        registry.register(DyeTool {
            sink: Arc::clone(&sink),
            seen_mid_call: Arc::clone(&seen_mid_call),
        });
        let pipeline = ToolPipeline::builder()
            .with_middleware(CapturingMiddleware::new(Arc::clone(&sink)))
            .with_core(Arc::new(registry))
            .build()
            .expect("the pipeline builds");
        let ctx = ToolDispatchContext {
            tool_name: "Dye".to_string(),
            input: serde_json::json!({"shade": "red"}),
            call_id: "call-x".to_string(),
            turn_number: 1,
            cancel: Arc::new(loopctl::cancel::CancelSignal::new()),
            permission: loopctl::tool::PermissionCheck::allow(),
            tool_context: loopctl::tool::ToolContext::default(),
        };
        let result = pipeline.invoke(ctx).await;
        assert!(!result.is_error, "the call itself succeeded");

        let mid_call = seen_mid_call
            .lock()
            .expect("observer")
            .clone()
            .expect("the input was recorded before the tool ran");
        assert!(!mid_call.done, "the mid-call record is the running one");
        assert!(mid_call.input_json.contains("\"shade\": \"red\""));
        assert!(mid_call.output.is_empty());

        let capture = sink
            .lock()
            .expect("sink")
            .get("call-x")
            .cloned()
            .expect("the completed record replaced it");
        assert!(capture.done);
        assert_eq!(
            capture.output, "red text    tail",
            "escape sequences strip and tabs expand before retention"
        );
    }

    #[test]
    fn the_cap_boundary_is_exact() {
        let exact: String = "x".repeat(OUTPUT_RETAIN_CHARS);
        assert_eq!(cap_output(&exact).chars().count(), OUTPUT_RETAIN_CHARS);
        let over: String = "x".repeat(OUTPUT_RETAIN_CHARS + 1);
        assert!(
            cap_output(&over).contains("characters omitted"),
            "one character past the cap takes the cut"
        );
    }

    #[test]
    fn long_output_keeps_its_head_and_tail_with_a_marker() {
        let text: String = (0..(OUTPUT_RETAIN_CHARS + 100))
            .map(|i| char::from(b'a' + u8::try_from(i % 26).expect("a small remainder")))
            .collect();
        let capped = cap_output(&text);
        assert!(
            capped.contains("characters omitted] …"),
            "the cut is named: {}…",
            capped.chars().take(120).collect::<String>()
        );
        assert!(capped.starts_with(&text[..200]), "the head is retained");
        assert!(
            capped.ends_with(&text[text.len() - 200..]),
            "the tail is retained"
        );
    }
}
