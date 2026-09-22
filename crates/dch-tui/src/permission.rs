//! The permission prompt bridge between the gate and the TUI.
//!
//! The runner's permission gate resolves its `Ask` verdicts through an
//! async resolver; this type is what that resolver hands the UI — one
//! request per pending ask, answered once on the reply channel. The
//! gate races the run's cancel signal against the reply, so a request
//! the user never answers cannot outlive a cancelled run.

/// One permission ask awaiting the user's answer.
///
/// Created by the resolver the TUI installs on the runner, delivered
/// over an unbounded channel, and answered exactly once by sending on
/// [`reply`](Self::reply) — `true` allows the tool call, `false`
/// denies it. Dropping the request unanswered reads as a denial on
/// the resolver side, and a cancelled run denies its pending ask
/// without waiting for the reply at all.
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
    pub prompt: String,

    /// Where the answer goes.
    ///
    /// The resolver's future awaits this channel; sending consumes
    /// it, which is the request's single-answer contract. A sender
    /// dropped unanswered closes the channel and the resolver reads
    /// a denial.
    pub reply: tokio::sync::oneshot::Sender<bool>,
}
