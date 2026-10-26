//! The seam between the interface and a run that is already in flight.
//!
//! Every other path from the composer to the model is a *turn*: the app
//! hands text to [`set_submit_tx`](crate::app::TuiApp::set_submit_tx) and a
//! mode driver opens a run with it. A run already open cannot be entered that
//! way — the task driving it holds the loop — so guidance goes through a
//! slot the loop drains itself at the next turn boundary.

/// A steering slot the interface can offer text to.
///
/// Implementations are shared (`Arc<dyn Guidance>`) because the handle
/// outlives no single borrow of anything: the app holds one clone, the
/// loop holds the state behind it.
pub trait Guidance: Send + Sync {
    /// Offer `text` to the run in flight, as guidance rather than a turn.
    ///
    /// `true` when the run took it, and will hand it to the model at its
    /// next turn boundary. `false` when nothing is running — or when the
    /// run ended in the instant before the offer landed — and the text is
    /// still the caller's to deliver another way. The interface relies on
    /// that atomicity: a refused offer falls back to an ordinary submit,
    /// and the words cannot be both queued and delivered.
    fn steer(&self, text: String) -> bool;

    /// Take everything no run delivered, oldest first.
    ///
    /// Guidance a run accepted but never reached a boundary for, plus any
    /// the run before it left behind — advice about a turn that no longer
    /// stands, which no later run should silently deliver. The interface
    /// gives it back to the user rather than discarding it, so this is
    /// called only while nothing is in flight: called during a run it would
    /// relieve that run of guidance it had not yet read.
    fn take_returned(&self) -> Vec<String>;
}
