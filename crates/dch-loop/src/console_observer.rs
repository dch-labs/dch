//! Headless console rendering of agent-run events.
//!
//! [`ConsoleObserver`] turns a running agent loop into terminal output for
//! `dch --headless`: the model's text streams to stdout, and everything else
//! — tool calls, turn markers, token usage, compaction and model-switch
//! notes, errors — goes to stderr, keeping stdout a clean transcript of what
//! the model said. Output volume is governed by
//! `Verbosity` (from `dch_config`): Quiet is model text only, Normal
//! adds concise tool-call lines, Verbose adds durations, token counts, and
//! lifecycle notes.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use dch_config::Verbosity;
use loopctl::observer::{
    CompactedContext, FallbackContext, LoopObserver, ModelSwitchedContext, ResponseContext,
    RunEndContext, RunStartContext, StreamFailureContext, TextDeltaContext, ToolPostContext,
    ToolPreContext, TurnEndContext, TurnStartContext,
};

/// ANSI reset sequence, appended after every colored span.
///
/// Every `paint` span carries its own reset, so color never leaks across
/// writes when a stream splits mid-line.
const RESET: &str = "\x1b[0m";

/// ANSI dim intensity, used for turn separators.
///
/// Applied to the per-turn separator line in verbose output.
const DIM: &str = "2";

/// ANSI cyan, used for streamed assistant text.
///
/// Applied to every streamed delta and to whole responses on the
/// no-streamer path.
const CYAN: &str = "36";

/// ANSI yellow, used for the tool-start line.
///
/// Applied when a tool is about to run.
const YELLOW: &str = "33";

/// ANSI green, used for successful tool results.
///
/// Applied when a tool reports success.
const GREEN: &str = "32";

/// ANSI red, used for failed tool results.
///
/// Applied when a tool reports an error.
const RED: &str = "31";

/// Render agent-run events for a terminal.
///
/// The model's text streams to stdout, and everything else — tool calls,
/// turn markers, token usage, compaction and model-switch notes, errors —
/// goes to stderr, keeping stdout a clean transcript of what the model said.
/// Output volume is governed by [`Verbosity`]: Quiet emits model text only,
/// Normal adds concise tool-call lines, Verbose adds durations, token
/// counts, and lifecycle notes.
///
/// The `Mutex`-wrapped sinks keep the observer `Send + Sync` (the observer
/// trait requires both) and serialize writes so concurrent events never
/// interleave mid-line. Locks are held only for the duration of a write.
///
/// Two contract points for hosts: the observer is designed for **one run
/// per process** — its counters reset at run start, and overlapping runs
/// sharing one observer would interleave their output. And model-derived
/// text (streamed deltas, responses) is written **verbatim**: stdout is
/// the capture payload, so embedded terminal control sequences are not
/// sanitized, and consumers that render the stream in a terminal should
/// treat it as untrusted text.
pub struct ConsoleObserver {
    /// Output detail level, from configuration.
    ///
    /// Gates progress chrome and detail chrome independently of the model's
    /// text, which is emitted at every level.
    verbosity: Verbosity,

    /// Whether ANSI color escapes may be emitted.
    ///
    /// Computed once by the caller (TTY check plus the `NO_COLOR`
    /// convention); the observer never re-probes the environment.
    use_color: bool,

    /// Stdout sink: the model's text.
    ///
    /// Streamed deltas and whole-turn responses both land here, serialized
    /// by the sink's lock.
    out: Mutex<Box<dyn Write + Send>>,

    /// Stderr sink: progress, usage, and errors.
    ///
    /// Tool lifecycle, turn separators, lifecycle notes, and failures land
    /// here, keeping stdout a clean capture of the model's text.
    err: Mutex<Box<dyn Write + Send>>,

    /// Whether the current turn has streamed at least one text delta.
    ///
    /// Set by [`on_text_delta`], reset at each turn start: `on_response`
    /// uses it to decide between closing the streamed line and printing
    /// the whole response as the sole delivery.
    saw_text_delta: AtomicBool,

    /// Tool calls completed this run.
    ///
    /// Counted per finished tool and reported in the verbose run summary.
    tool_calls: AtomicUsize,

    /// Input tokens accumulated across completed turns.
    input_tokens: AtomicU64,

    /// Output tokens accumulated across completed turns.
    output_tokens: AtomicU64,
}

impl ConsoleObserver {
    /// Build an observer writing to the process's real stdout and stderr.
    ///
    /// `use_color` should be the AND of a TTY check on stderr and an
    /// unset-or-empty `NO_COLOR` environment variable — headless output is
    /// frequently piped, and ANSI escapes in a captured log are noise. See
    /// `ConsoleObserver::detect_color`.
    #[must_use]
    pub fn new(verbosity: Verbosity, use_color: bool) -> Self {
        Self::with_sinks(
            verbosity,
            use_color,
            Box::new(std::io::stdout()),
            Box::new(std::io::stderr()),
        )
    }

    /// Build an observer writing to explicit sinks.
    ///
    /// Production callers pass the real stdout/stderr; tests pass buffers so
    /// output can be asserted without touching the process's streams.
    #[must_use]
    pub fn with_sinks(
        verbosity: Verbosity,
        use_color: bool,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            verbosity,
            use_color,
            out: Mutex::new(out),
            err: Mutex::new(err),
            saw_text_delta: AtomicBool::new(false),
            tool_calls: AtomicUsize::new(0),
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
        }
    }

    /// Whether ANSI color should be emitted for this terminal.
    ///
    /// True when stderr is a terminal and `NO_COLOR` is unset or empty — the
    /// de-facto standard for disabling color.
    #[must_use]
    pub fn detect_color() -> bool {
        let no_color = std::env::var("NO_COLOR").is_ok_and(|value| !value.is_empty());
        !no_color && std::io::stderr().is_terminal()
    }

    /// Wrap `text` in the ANSI span `code` when color is on.
    ///
    /// Every span carries its own reset, so color never leaks across
    /// writes; with color off, `text` is returned unchanged.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.use_color {
            format!("\x1b[{code}m{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    /// Write one line to stdout, newline included.
    ///
    /// I/O faults are ignored: stdout is line-buffered, so the newline
    /// flushes the line anyway, and a broken pipe (a consumer that closed
    /// early) must never take the run down.
    fn say(&self, msg: &str) {
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(writeln!(out, "{msg}"));
    }

    /// Write a partial fragment to stdout with no newline — the streaming
    /// path.
    ///
    /// Flushes immediately after writing: stdout is line-buffered, so an
    /// unflushed fragment stays invisible until a later line closes it.
    /// I/O faults are ignored, as in [`Self::say`] — a broken pipe must
    /// never take the run down.
    fn say_raw(&self, fragment: &str) {
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(write!(out, "{fragment}"));
        out.flush().ok();
    }

    /// Write one line to stderr, newline included.
    ///
    /// Carries all progress chrome, token usage, and failure notices. I/O
    /// faults are ignored as in [`Self::say`] — diagnostics must never
    /// take the run down.
    fn note(&self, msg: &str) {
        let mut err = self
            .err
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(writeln!(err, "{msg}"));
    }

    /// Whether progress chrome (tool lines, separators) is visible.
    ///
    /// Visible at Normal and Verbose; hidden at Quiet, where stdout
    /// carries the model's text and nothing else.
    fn chrome_visible(&self) -> bool {
        !matches!(self.verbosity, Verbosity::Quiet)
    }

    /// Whether detail chrome (durations, token counts) is visible.
    ///
    /// True only at Verbose. Everything gated on this is supplementary
    /// accounting, never primary output.
    fn detail_visible(&self) -> bool {
        matches!(self.verbosity, Verbosity::Verbose)
    }
}

impl LoopObserver for ConsoleObserver {
    fn name(&self) -> &'static str {
        "console"
    }

    fn on_run_start(&self, _ctx: &RunStartContext) {
        self.saw_text_delta.store(false, Ordering::Relaxed);
        self.tool_calls.store(0, Ordering::Relaxed);
        self.input_tokens.store(0, Ordering::Relaxed);
        self.output_tokens.store(0, Ordering::Relaxed);
    }

    fn on_turn_start(&self, _ctx: &TurnStartContext) {
        self.saw_text_delta.store(false, Ordering::Relaxed);
    }

    /// Stream the delta to stdout in the assistant accent.
    ///
    /// Marks the turn as streamed, so the closing `on_response` does not
    /// duplicate the text.
    fn on_text_delta(&self, ctx: &TextDeltaContext) {
        self.stream_delta(&ctx.delta);
    }

    /// Deliver the turn's assembled response to stdout.
    ///
    /// After streamed deltas this only closes the line the deltas opened —
    /// reprinting the text would duplicate it. When nothing streamed, the
    /// whole response arrives here as the sole copy. At Verbose, a
    /// token-usage line accompanies it on stderr.
    fn on_response(&self, ctx: &ResponseContext) {
        let usage = ctx
            .usage
            .as_ref()
            .map(|usage| (usage.input_tokens, usage.output_tokens));
        self.finish_response(&ctx.text, usage);
    }

    /// Announce a tool about to run (chrome; hidden at Quiet).
    ///
    /// The turn marker is included only when detail chrome is visible.
    fn on_tool_pre(&self, ctx: &ToolPreContext) {
        self.tool_started(&ctx.tool, ctx.turn);
    }

    /// Report a finished tool with a ✓/✗ marker on stderr.
    ///
    /// Verbose adds the tool's duration in milliseconds. Every completion
    /// counts toward the end-of-run summary's tool-call total.
    fn on_tool_post(&self, ctx: &ToolPostContext) {
        self.tool_finished(&ctx.tool, ctx.is_error, ctx.duration);
    }

    /// Close a turn with its closing chrome, or its error.
    ///
    /// Successful turns get a token-total separator at Verbose and a plain
    /// blank line at Normal; failures print the error verbatim instead, at
    /// every verbosity.
    fn on_turn_end(&self, ctx: &TurnEndContext) {
        self.turn_finished(
            ctx.turn,
            ctx.success,
            ctx.error.as_deref(),
            ctx.duration_ms,
            ctx.input_tokens,
            ctx.output_tokens,
        );
    }

    /// Report a stream failure to stderr.
    ///
    /// Printed at every verbosity, Quiet included: a stream failure is the
    /// agent stopping, not progress noise, and suppressing it would leave a
    /// silent run.
    fn on_stream_failure(&self, ctx: &StreamFailureContext) {
        self.stream_failed(&ctx.model, &ctx.error.to_string());
    }

    /// Note a compaction pass with its token reduction.
    ///
    /// Verbose only: the before/after totals explain why earlier context
    /// stopped being visible to the model.
    fn on_compaction(&self, ctx: &CompactedContext) {
        self.history_compacted(ctx.tokens_before, ctx.tokens_after);
    }

    /// Note a model fallback with both endpoints.
    ///
    /// Verbose only. The primary model failed over to its fallback; the
    /// message names the model that was left and the one that took over.
    fn on_fallback(&self, ctx: &FallbackContext) {
        self.model_changed("fallback", &ctx.from, &ctx.to);
    }

    /// Note a hot-switched model with both endpoints.
    ///
    /// Verbose only, covering mid-run switches that are not failure
    /// fallbacks.
    fn on_model_switched(&self, ctx: &ModelSwitchedContext) {
        self.model_changed("switched", &ctx.from, &ctx.to);
    }

    /// Print the end-of-run summary to stderr.
    ///
    /// Verbose only: outcome, wall-clock duration, turn count, tool-call
    /// count, and the token totals accumulated across turns. The totals are
    /// the ones [`on_run_start`](Self::on_run_start) reset, so they cover
    /// exactly this run.
    fn on_run_end(&self, ctx: &RunEndContext) {
        self.run_finished(
            ctx.success,
            ctx.error.as_deref(),
            ctx.total_turns,
            ctx.duration_ms,
        );
    }
}

impl ConsoleObserver {
    /// Stream one text delta to stdout in the assistant accent.
    ///
    /// Marks the turn as streamed so the closing response does not
    /// duplicate the text.
    fn stream_delta(&self, delta: &str) {
        self.saw_text_delta.store(true, Ordering::Relaxed);
        let fragment = self.paint(CYAN, delta);
        self.say_raw(&fragment);
    }

    /// Deliver the turn's assembled response to stdout.
    ///
    /// The closing half of the streaming contract: after deltas this emits
    /// only the terminating newline; without deltas it paints and writes
    /// the whole text as the sole delivery. The usage pair, when present,
    /// becomes a token-usage line on stderr at Verbose.
    fn finish_response(&self, text: &str, usage: Option<(u32, u32)>) {
        if self.saw_text_delta.load(Ordering::Relaxed) {
            self.say("");
        } else {
            self.say(&self.paint(CYAN, text));
        }
        if self.detail_visible()
            && let Some((input, output)) = usage
        {
            self.note(&format!("{input}+{output} tok"));
        }
    }

    /// Announce a tool about to run (chrome; hidden at Quiet).
    ///
    /// The turn marker is included only when detail chrome is visible.
    fn tool_started(&self, tool: &str, turn: usize) {
        if !self.chrome_visible() {
            return;
        }
        let line = if self.detail_visible() {
            format!("▸ {tool} (turn {turn})")
        } else {
            format!("▸ {tool}")
        };
        self.note(&self.paint(YELLOW, &line));
    }

    /// Announce a tool's completion on stderr.
    ///
    /// The line is a ✓/✗ marker with the tool's name, plus its duration in
    /// milliseconds at Verbose. Hidden at Quiet, but still counted toward
    /// the end-of-run summary.
    fn tool_finished(&self, tool: &str, is_error: bool, duration: std::time::Duration) {
        if !self.chrome_visible() {
            return;
        }
        self.tool_calls.fetch_add(1, Ordering::Relaxed);
        let marker = if is_error {
            self.paint(RED, "✗")
        } else {
            self.paint(GREEN, "✓")
        };
        let line = if self.detail_visible() {
            format!("{marker} {tool} ({}ms)", duration.as_millis())
        } else {
            format!("{marker} {tool}")
        };
        self.note(&line);
    }

    /// Close a turn with its closing chrome, or its error.
    ///
    /// Also accumulates a successful turn's token totals into the run
    /// counters, so the end-of-run summary reflects every turn. Successful
    /// turns render a token-total separator at Verbose and a blank line at
    /// Normal; failures print the error verbatim at every verbosity.
    fn turn_finished(
        &self,
        turn: usize,
        success: bool,
        error: Option<&str>,
        duration_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        if !success {
            if let Some(error) = error {
                self.note(error);
            }
            return;
        }

        self.input_tokens.fetch_add(input_tokens, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(output_tokens, Ordering::Relaxed);

        if self.detail_visible() {
            let separator = self.paint(
                DIM,
                &format!("──── turn {turn}: {input_tokens}+{output_tokens} tok, {duration_ms}ms"),
            );
            self.note(&separator);
        } else if self.chrome_visible() {
            self.note("");
        }
    }

    /// Render a stream failure to stderr, naming the model.
    ///
    /// Printed at every verbosity, Quiet included — a stream failure is the
    /// agent stopping, not progress noise, and suppressing it would leave
    /// the user staring at a silent run.
    fn stream_failed(&self, model: &str, error: &str) {
        self.note(&format!("{model}: {error}"));
    }

    /// Note a compaction pass with its token reduction.
    ///
    /// Verbose only: the before/after totals explain why earlier context
    /// stopped being visible to the model.
    fn history_compacted(&self, tokens_before: u64, tokens_after: u64) {
        if self.detail_visible() {
            self.note(&format!(
                "history compacted: {tokens_before} → {tokens_after} tok"
            ));
        }
    }

    /// Note a model change with both endpoints.
    ///
    /// Shared by fallback and hot-switch reporting; the caller supplies the
    /// verb that distinguishes them. Verbose only.
    fn model_changed(&self, kind: &str, from: &str, to: &str) {
        if self.detail_visible() {
            self.note(&format!("{kind}: {from} → {to}"));
        }
    }

    /// Print the end-of-run summary to stderr.
    ///
    /// Verbose only: outcome, wall-clock duration, turn count, tool-call
    /// count, and the token totals accumulated across turns. The totals are
    /// the ones [`on_run_start`](Self::on_run_start) reset, so they cover
    /// exactly this run.
    fn run_finished(
        &self,
        success: bool,
        error: Option<&str>,
        total_turns: usize,
        duration_ms: u64,
    ) {
        if !self.detail_visible() {
            return;
        }
        let tool_calls = self.tool_calls.load(Ordering::Relaxed);
        let input = self.input_tokens.load(Ordering::Relaxed);
        let output = self.output_tokens.load(Ordering::Relaxed);
        let outcome = if success {
            "finished".to_string()
        } else {
            format!("failed: {}", error.unwrap_or("unknown error"))
        };
        self.note(&format!(
            "run {outcome} in {duration_ms}ms: {total_turns} turns, \
             {tool_calls} tool calls, {input}+{output} tok"
        ));
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]
mod tests {
    use super::*;
    use dch_config::Verbosity;
    use std::sync::Arc;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// An observer writing into shared in-memory sinks, plus readable
    /// handles to what stdout and stderr collected.
    struct Harness {
        observer: ConsoleObserver,
        out: Arc<Mutex<Vec<u8>>>,
        err: Arc<Mutex<Vec<u8>>>,
    }

    /// A `Write` sink backed by a shared buffer, so a test can read what
    /// the observer wrote through its own `Mutex<Box<dyn Write + Send>>`.
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Harness {
        fn build(verbosity: Verbosity, use_color: bool) -> Self {
            let out = Arc::new(Mutex::new(Vec::new()));
            let err = Arc::new(Mutex::new(Vec::new()));
            let observer = ConsoleObserver::with_sinks(
                verbosity,
                use_color,
                Box::new(SharedSink(Arc::clone(&out))),
                Box::new(SharedSink(Arc::clone(&err))),
            );
            Self { observer, out, err }
        }

        fn stdout(&self) -> String {
            String::from_utf8(self.out.lock().unwrap().clone()).unwrap()
        }

        fn stderr(&self) -> String {
            String::from_utf8(self.err.lock().unwrap().clone()).unwrap()
        }

        fn delta(&self, text: &str) {
            self.observer.stream_delta(text);
        }

        fn tool_started(&self, tool: &str) {
            self.observer.tool_started(tool, 0);
        }

        fn tool_finished(&self, tool: &str, is_error: bool) {
            self.observer
                .tool_finished(tool, is_error, std::time::Duration::from_millis(42));
        }

        fn turn_ended(&self, turn: usize, success: bool) {
            self.observer.turn_finished(
                turn,
                success,
                (!success).then(|| "boom".to_string()).as_deref(),
                250,
                100,
                20,
            );
        }

        fn response(&self, text: &str) {
            self.observer.finish_response(text, Some((100, 20)));
        }
    }

    #[test]
    fn quiet_suppresses_tool_call_chrome() {
        let h = Harness::build(Verbosity::Quiet, false);
        h.tool_started("Bash");
        h.tool_finished("Bash", false);
        h.turn_ended(0, true);
        assert_eq!(h.stderr(), "", "quiet: no chrome on stderr");
        assert_eq!(h.stdout(), "", "quiet: no chrome on stdout");
    }

    #[test]
    fn normal_prints_tool_lines_to_stderr_only() {
        let h = Harness::build(Verbosity::Normal, false);
        h.tool_started("Bash");
        h.tool_finished("Bash", false);
        assert!(h.stderr().contains("▸ Bash"), "{}", h.stderr());
        assert!(h.stderr().contains("✓ Bash"), "{}", h.stderr());
        assert!(h.stdout().is_empty(), "chrome must not pollute stdout");
    }

    #[test]
    fn verbose_adds_turn_markers_durations_and_token_totals() {
        let h = Harness::build(Verbosity::Verbose, false);
        h.tool_started("Bash");
        h.tool_finished("Bash", false);
        h.turn_ended(0, true);
        assert!(h.stderr().contains("(turn 0)"), "{}", h.stderr());
        assert!(h.stderr().contains("42ms"), "{}", h.stderr());
        assert!(h.stderr().contains("100+20 tok"), "{}", h.stderr());
    }

    #[test]
    fn on_response_prints_whole_text_when_nothing_streamed() {
        let h = Harness::build(Verbosity::Quiet, false);
        h.response("hello");
        assert_eq!(h.stdout(), "hello\n");
    }

    #[test]
    fn text_deltas_stream_verbatim_to_stdout() {
        let h = Harness::build(Verbosity::Quiet, false);
        h.delta("foo");
        h.delta("bar");
        assert_eq!(h.stdout(), "foobar", "streaming is verbatim, no newlines");
    }

    #[test]
    fn on_response_after_deltas_only_closes_the_line() {
        let h = Harness::build(Verbosity::Normal, false);
        h.delta("streamed");
        h.response("streamed");
        // The deltas already delivered the text; the response only closes
        // the line — printing it whole again would duplicate the answer.
        assert_eq!(h.stdout(), "streamed\n");
    }

    #[test]
    fn stream_failure_prints_even_at_quiet() {
        let h = Harness::build(Verbosity::Quiet, false);
        h.observer.stream_failed("test-model", "stream died");
        assert!(
            h.stderr().contains("stream died"),
            "failures are the agent stopping, not progress noise"
        );
    }

    #[test]
    fn turn_end_error_prints_at_all_verbosities() {
        for verbosity in [Verbosity::Quiet, Verbosity::Normal, Verbosity::Verbose] {
            let h = Harness::build(verbosity, false);
            h.turn_ended(0, false);
            assert!(
                h.stderr().contains("boom"),
                "{verbosity:?}: the error must surface: {:?}",
                h.stderr()
            );
        }
    }

    #[test]
    fn color_off_emits_no_escape_codes() {
        let h = Harness::build(Verbosity::Verbose, false);
        h.delta("text");
        h.tool_started("Bash");
        h.tool_finished("Bash", false);
        assert!(
            !h.stdout().contains('\x1b') && !h.stderr().contains('\x1b'),
            "color off: no ANSI escapes anywhere"
        );
    }

    #[test]
    fn color_on_wraps_streamed_text_in_cyan() {
        let h = Harness::build(Verbosity::Quiet, true);
        h.delta("hi");
        assert!(
            h.stdout().contains("\x1b[36m"),
            "assistant text carries the cyan accent: {:?}",
            h.stdout()
        );
        assert!(h.stdout().contains("\x1b[0m"), "every span is reset");
    }

    #[test]
    fn tool_finished_line_is_marker_tool_and_duration_only() {
        // The rendered line is marker + tool + duration — structurally
        // unable to carry the context's result_hash (loop-internal state).
        let h = Harness::build(Verbosity::Verbose, false);
        h.tool_finished("Bash", false);
        assert_eq!(h.stderr(), "✓ Bash (42ms)\n");
    }

    #[test]
    fn tool_finished_failure_line_uses_the_failure_marker() {
        let h = Harness::build(Verbosity::Normal, false);
        h.tool_finished("Bash", true);
        assert_eq!(h.stderr(), "✗ Bash\n");
    }

    #[test]
    fn verbose_lifecycle_notes_print_at_detail_level() {
        let h = Harness::build(Verbosity::Verbose, false);
        h.observer.history_compacted(12_000, 4_000);
        h.observer
            .model_changed("switched", "big-model", "small-model");
        h.observer
            .model_changed("fallback", "big-model", "fallback-model");
        let stderr = h.stderr();
        assert!(stderr.contains("compacted: 12000 → 4000 tok"), "{stderr}");
        assert!(
            stderr.contains("switched: big-model → small-model"),
            "{stderr}"
        );
        assert!(
            stderr.contains("fallback: big-model → fallback-model"),
            "{stderr}"
        );
    }

    #[test]
    fn quiet_lifecycle_notes_stay_silent() {
        let h = Harness::build(Verbosity::Quiet, false);
        h.observer.history_compacted(12_000, 4_000);
        h.observer
            .model_changed("switched", "big-model", "small-model");
        h.observer
            .model_changed("fallback", "big-model", "fallback-model");
        assert!(h.stderr().is_empty());
    }

    #[test]
    fn run_end_verbose_prints_the_summary_and_run_start_resets_it() {
        let h = Harness::build(Verbosity::Verbose, false);
        h.tool_finished("Bash", false);
        h.observer.on_run_start(&RunStartContext {
            session_id: Uuid::new_v4(),
        });
        h.observer.on_run_end(&RunEndContext {
            success: true,
            error: None,
            total_turns: 3,
            duration_ms: 1500,
        });
        let stderr = h.stderr();
        assert!(stderr.contains("finished in 1500ms"), "{stderr}");
        assert!(stderr.contains("3 turns"), "{stderr}");
        // on_run_start reset the counters, so nothing from the pre-reset
        // tool call leaks into the summary.
        assert!(stderr.contains("0 tool calls"), "{stderr}");
    }

    #[test]
    fn run_end_summary_is_verbose_only() {
        // The end-of-run summary is detail chrome: Quiet and Normal runs get
        // the model's text and per-event errors, but no summary line.
        for verbosity in [Verbosity::Quiet, Verbosity::Normal] {
            let h = Harness::build(verbosity, false);
            h.observer.on_run_end(&RunEndContext {
                success: true,
                error: None,
                total_turns: 2,
                duration_ms: 300,
            });
            assert!(
                h.stderr().is_empty(),
                "{verbosity:?}: the run summary is detail chrome: {:?}",
                h.stderr()
            );
        }
    }

    #[test]
    fn streamed_fragments_flush_stdout_immediately() {
        // Stdout is line-buffered, so a streamed fragment without a newline
        // would stay invisible until a later line closed it unless say_raw
        // flushes after every write.
        struct FlushSpy {
            flushes: Arc<AtomicUsize>,
        }
        impl Write for FlushSpy {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        let flushes = Arc::new(AtomicUsize::new(0));
        let observer = ConsoleObserver::with_sinks(
            Verbosity::Quiet,
            false,
            Box::new(FlushSpy {
                flushes: Arc::clone(&flushes),
            }),
            Box::new(SharedSink(Arc::new(Mutex::new(Vec::new())))),
        );
        observer.stream_delta("hello");
        assert!(
            flushes.load(Ordering::Relaxed) >= 1,
            "a streamed fragment must flush stdout immediately"
        );
    }

    #[test]
    fn each_turn_resets_the_delta_flag_so_unstreamed_responses_still_print() {
        let h = Harness::build(Verbosity::Quiet, false);

        // Turn 1 streams deltas; the response closes the streamed line.
        h.delta("streamed");
        h.response("streamed");
        assert_eq!(h.stdout(), "streamed\n");

        // Turn 2 is delivered without deltas (the engine's non-streaming
        // last-chance path): the per-turn flag reset must let it print.
        h.observer.on_turn_start(&TurnStartContext {
            turn: 1,
            query: "next prompt".to_string(),
        });
        h.response("the answer");
        assert_eq!(
            h.stdout(),
            "streamed\nthe answer\n",
            "a non-streamed turn's answer must reach stdout"
        );
    }

    #[test]
    fn observer_is_send_sync_and_named_console() {
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let h = Harness::build(Verbosity::Normal, false);
        assert_send_sync(&h.observer);
        assert_eq!(LoopObserver::name(&h.observer), "console");
    }
}
