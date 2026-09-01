//! Done-file writing for headless runs.
//!
//! A done-file is a JSON status marker written when a headless run reaches
//! any terminal outcome — success, failure, or cancellation. CI orchestrators
//! poll for its existence instead of parsing stdout. Write failures are
//! non-fatal: the run's exit code is the source of truth, not the marker.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Completion status written to the `--done-file` path on exit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoneStatus {
    /// Whether the run completed successfully.
    ///
    /// `false` covers both a failed run and a run that ended without a
    /// final answer — a CI consumer treats either as "not done".
    pub success: bool,

    /// Human-readable outcome summary or error description.
    ///
    /// The final answer on success, the error text otherwise; every
    /// builder renders it, so a consumer can display it unconditionally.
    pub message: Option<String>,

    /// Turns completed during the run.
    ///
    /// `None` when the run failed before producing a result — there was
    /// nothing to count.
    pub turns: Option<usize>,

    /// Tool calls made during the run.
    ///
    /// `None` under the same conditions as [`turns`](Self::turns).
    pub tools_used: Option<usize>,
}

impl DoneStatus {
    /// Build a success status with the given message and counts.
    ///
    /// Called after `runner.run()` returns `Ok` with output — the counts
    /// come from the `Run` accessors, not from observer state.
    #[must_use]
    pub fn success(message: impl Into<String>, turns: usize, tools_used: usize) -> Self {
        Self {
            success: true,
            message: Some(message.into()),
            turns: Some(turns),
            tools_used: Some(tools_used),
        }
    }

    /// Build a failure status with a message and no counts.
    ///
    /// Used when the run fails before producing a result (config error,
    /// construction error, empty prompt).
    #[must_use]
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            turns: None,
            tools_used: None,
        }
    }

    /// Build a failure status that still carries the run's final counts.
    ///
    /// Used when the loop ended without producing a final answer: the run
    /// failed, but the turn and tool-call totals it accumulated are real.
    #[must_use]
    pub fn failure_with_counts(
        message: impl Into<String>,
        turns: usize,
        tools_used: usize,
    ) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            turns: Some(turns),
            tools_used: Some(tools_used),
        }
    }
}

/// Serialize `status` as pretty JSON to `path` with a trailing newline.
///
/// Uses temp-then-rename so a reader never sees a half-written JSON file.
///
/// # Errors
///
/// Returns the underlying I/O or serialization error when the file cannot
/// be written. Callers should log and continue — the done-file is a status
/// marker, not the source of truth.
pub fn write_done_file(path: &Path, status: &DoneStatus) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(status)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{json}\n"))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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

    #[test]
    fn done_status_round_trips_through_json() {
        let status = DoneStatus::success("ok", 3, 5);
        let json = serde_json::to_string_pretty(&status).unwrap();
        let restored: DoneStatus = serde_json::from_str(&json).unwrap();
        assert!(restored.success);
        assert_eq!(restored.turns, Some(3));
        assert_eq!(restored.tools_used, Some(5));
    }

    #[test]
    fn failure_status_has_no_counts() {
        let status = DoneStatus::failure("config not found");
        assert!(!status.success);
        assert_eq!(status.turns, None);
        assert_eq!(status.tools_used, None);
    }

    #[test]
    fn failure_with_counts_keeps_the_run_totals() {
        let status = DoneStatus::failure_with_counts("no final answer", 4, 9);
        let json = serde_json::to_string_pretty(&status).unwrap();
        let restored: DoneStatus = serde_json::from_str(&json).unwrap();
        assert!(!restored.success);
        assert_eq!(restored.turns, Some(4));
        assert_eq!(restored.tools_used, Some(9));
    }

    #[test]
    fn write_done_file_creates_the_file_and_cleans_up_tmp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        let status = DoneStatus::success("ok", 2, 4);
        write_done_file(&path, &status).unwrap();
        assert!(path.exists());
        assert!(!tmp.path().join("done.tmp").exists(), "no temp residue");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"success\": true"));
    }

    #[test]
    fn write_done_file_overwrites_an_existing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        std::fs::write(&path, "old").unwrap();
        let status = DoneStatus::success("new", 1, 1);
        write_done_file(&path, &status).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("new"));
    }
}
