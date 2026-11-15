//! The permission prompt bridge between the gate and the TUI.
//!
//! The runner's permission gate resolves its `Ask` verdicts through the
//! typed ask resolver; this type is what that resolver hands the UI —
//! one request per pending ask, answered once on the reply channel. The
//! gate races the run's cancel signal against the reply, so a request
//! the user never answers cannot outlive a cancelled run.

use loopctl::middleware::PermissionResolution;

/// One permission ask awaiting the user's answer.
///
/// Created by the typed resolver the TUI installs on the runner,
/// delivered over an unbounded channel, and answered exactly once by
/// sending a [`PermissionResolution`] on [`reply`](Self::reply) —
/// `AllowOnce` lets the call run, the denials refuse it, and
/// `DenyWithFeedback` refuses it with the user's own words as the
/// reason the model reads. Dropping the request unanswered reads as a
/// denial on the resolver side, and a cancelled run denies its pending
/// ask without waiting for the reply at all.
#[derive(Debug)]
pub struct PermissionRequest {
    /// The registered name of the tool asking, as the gate classified
    /// it.
    ///
    /// The gate's prompt already names the tool, so the overlay
    /// renders the prompt; this field carries the bare name for hosts
    /// that want it without parsing the prompt apart.
    pub tool_name: String,

    /// The gate's question, naming the tool, its category, and the
    /// active mode.
    ///
    /// Composed by the gate (not the UI) so the wording the model's
    /// dispatch produced and the wording the user sees cannot drift.
    /// Pure display — what the ask carries is [`input`](Self::input).
    pub prompt: String,

    /// The raw JSON input of the call, verbatim from the dispatch.
    ///
    /// What the user approves, they approve from this: the sheet
    /// renders the command from it ([`command_detail`](Self::command_detail)),
    /// never from a display string, so what is shown is exactly what
    /// will run.
    pub input: serde_json::Value,

    /// What an "allow always" answer on this ask would remember.
    ///
    /// Derived from [`input`](Self::input) by the resolver — typed
    /// rules, one arity-trimmed verb prefix per clean shell segment
    /// plus the exact text of each complex one, the write scopes, or
    /// the tool itself — so the panel records exactly what the approval
    /// covers without parsing the prompt. Empty when the ask carries
    /// nothing a rule can name.
    pub always: Vec<crate::queue::PermissionRule>,

    /// What a "deny always" answer on this ask would record — the
    /// narrow derivation: readers mint nothing (a reader is never what
    /// was refused), complex non-reader segments their exact text,
    /// clean non-reader segments the verb prefix; write scopes and
    /// whole-tool asks the same rules the allow side mints. Composed
    /// by the resolver beside [`always`](Self::always) from the same
    /// raw input, and the memory skips same-shape rules the allow list
    /// already carries at record time — a blessing is sticky, the
    /// refusal cannot retract it by association.
    pub never: Vec<crate::queue::PermissionRule>,

    /// Where the answer goes.
    ///
    /// The resolver's future awaits this channel; sending consumes
    /// it, which is the request's single-answer contract. A sender
    /// dropped unanswered closes the channel and the resolver reads
    /// a denial.
    pub reply: tokio::sync::oneshot::Sender<PermissionResolution>,
}

impl PermissionRequest {
    /// The command the ask would run, read from the raw call input.
    ///
    /// The Bash ask's whole command — never truncated, a second line
    /// rendered as a `⏎` mark — exactly the bytes the model sent, not
    /// a rendering of them. `None` for every other tool: their prompts
    /// name the tool, and nothing in their inputs renders as one
    /// honest command line.
    #[must_use]
    pub fn command_detail(&self) -> Option<String> {
        if self.tool_name != "Bash" {
            return None;
        }
        self.input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|command| command.replace('\n', " ⏎ "))
    }
}
