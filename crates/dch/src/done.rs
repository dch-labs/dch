//! Done-file writing for headless runs.
//!
//! A done-file is a JSON status marker written when a headless run reaches
//! any terminal outcome — success, failure, or cancellation. CI orchestrators
//! poll for its existence instead of parsing stdout. Write failures are
//! non-fatal: the run's exit code is the source of truth, not the marker.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Completion status written to the `--done-file` path on exit.
///
/// The marker a polling orchestrator reads instead of parsing
/// stdout: one JSON object naming the outcome, the run's totals, and
/// the session the transcript saved under. Built by the three
/// outcome builders and stamped with the session id at the write
/// call.
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
    /// `Some` when the engine produced run totals — including a run
    /// that ended without a final answer (`failure_with_counts`); `None`
    /// when the run failed with an error, since the engine's error
    /// values carry no partial-run totals.
    pub turns: Option<usize>,

    /// Tool calls made during the run.
    ///
    /// `None` under the same conditions as [`turns`](Self::turns).
    pub tools_used: Option<usize>,

    /// The session the run's transcript saves under, once resolved.
    ///
    /// `Some` on every marker written after the run's session id is
    /// known — the resolved-outcome write and the force-cancel hook's
    /// — so a consumer reading the marker can correlate it with the
    /// sessions directory and resume by id without guessing which
    /// save the run produced. `None` on markers written before a
    /// session existed (bootstrap, startup-cancel, and construction
    /// failures) and on markers from older versions, which predate
    /// the field.
    #[serde(default)]
    pub session_id: Option<Uuid>,
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
            session_id: None,
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
            session_id: None,
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
            session_id: None,
        }
    }

    /// Stamp the session the run is bound to onto the status.
    ///
    /// The marker writers know where the run's session id lives —
    /// resolved after construction, or absent on the paths that end
    /// before one exists — so every builder leaves the field `None`
    /// and each writer stamps what it holds. Returns the status for
    /// use at the write call.
    #[must_use]
    pub fn with_session_id(mut self, session_id: Option<Uuid>) -> Self {
        self.session_id = session_id;
        self
    }
}

/// Serialize `status` as pretty JSON to `path` with a trailing newline.
///
/// Uses temp-then-rename so a reader never sees a half-written JSON file,
/// and applies `inherited_mode` to the temporary file before the rename
/// when given — the mode a run captured before clearing the stale marker,
/// so the replacement never exists at the platform default and a
/// restrictive marker stays restrictive without a post-write window.
/// Without an inherited mode, an existing marker's own permissions are
/// carried over instead (a rename swaps the file, not its metadata, so
/// the mode would otherwise reset to the platform default — silently
/// widening a restrictive marker); a first-time marker with neither
/// available keeps the platform default.
///
/// # Errors
///
/// Returns the underlying I/O or serialization error when the file cannot
/// be written. Callers should log and continue — the done-file is a status
/// marker, not the source of truth. A failed write leaves no marker behind
/// and removes the temporary file.
pub fn write_done_file(
    path: &Path,
    status: &DoneStatus,
    inherited_mode: Option<&std::fs::Permissions>,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(status)?;
    let tmp = unique_sibling(path);
    let result = write_marker(&tmp, path, &json, inherited_mode);
    if result.is_err() {
        drop(std::fs::remove_file(&tmp));
    }
    result
}

/// Overwrite the marker in place, without the temp-file rename.
///
/// The fallback for a path whose directory cannot host the rename —
/// typically right after a failed stale-marker removal: an in-place write
/// needs write permission on the file alone, and at minimum invalidates
/// the previous run's outcome instead of leaving it authoritative while
/// the process exits in error.
///
/// # Errors
///
/// Returns the underlying I/O or serialization error; unlike
/// [`write_done_file`] a concurrent reader may observe a partial line.
pub fn overwrite_done_file(
    path: &Path,
    status: &DoneStatus,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(status)?;
    std::fs::write(path, format!("{json}\n"))?;
    Ok(())
}

/// Write `json` to `tmp`, apply the marker's mode, then rename it into
/// place.
///
/// # Errors
///
/// Returns any error from creating or writing the temporary file, from
/// applying the mode, or from the final rename.
fn write_marker(
    tmp: &Path,
    target: &Path,
    json: &str,
    inherited_mode: Option<&std::fs::Permissions>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut handle = std::fs::File::create(tmp)?;
    handle.write_all(format!("{json}\n").as_bytes())?;
    drop(handle);
    apply_marker_mode(tmp, target, inherited_mode)?;
    std::fs::rename(tmp, target)?;
    Ok(())
}

/// Give `tmp` the marker's permissions before it becomes the marker.
///
/// The inherited mode wins when present; otherwise an existing target's
/// own mode carries over.
///
/// # Errors
///
/// Returns the underlying I/O error when the permissions cannot be read
/// or applied.
fn apply_marker_mode(
    tmp: &Path,
    target: &Path,
    inherited_mode: Option<&std::fs::Permissions>,
) -> std::io::Result<()> {
    match inherited_mode {
        Some(mode) => std::fs::set_permissions(tmp, mode.clone()),
        None => preserve_target_mode(target, tmp),
    }
}

/// Apply `target`'s existing permissions to `tmp` before the rename.
///
/// Without this, the replacement carries the platform default mode and a
/// restrictive marker (for example `0600`) is silently widened. A target
/// that does not exist yet is left at the platform default.
///
/// # Errors
///
/// Returns the underlying I/O error when the target's metadata cannot be
/// read or the temporary file's permissions cannot be set.
#[cfg(unix)]
fn preserve_target_mode(target: &Path, tmp: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(target) {
        let permissions = std::fs::Permissions::from_mode(metadata.permissions().mode());
        std::fs::set_permissions(tmp, permissions)?;
    }
    Ok(())
}

/// Non-Unix fallback: no permission bits to carry over.
///
/// # Errors
///
/// Never fails.
#[cfg(not(unix))]
fn preserve_target_mode(_target: &Path, _tmp: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A uniquely named sibling of `path` in the same directory.
///
/// Staying in the directory keeps the final rename atomic (one filesystem),
/// and the per-call uuid keeps concurrent runs targeting the same marker
/// path from writing into each other's temporary file.
fn unique_sibling(path: &Path) -> PathBuf {
    let file_name = path.file_name().map_or_else(
        || "done".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    dir.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()))
}

#[cfg(test)]
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
        assert_eq!(
            restored.session_id, None,
            "builders leave the field unstamped"
        );
    }

    #[test]
    fn a_marker_from_an_older_version_parses_without_the_session_field() {
        // A pre-field marker carries four keys; adding the key must
        // not break a reader pointed at an old done-file.
        let legacy = "{\n  \"success\": true,\n  \"message\": \"ok\",\n  \"turns\": 2,\n  \"tools_used\": 4\n}";
        let restored: DoneStatus = serde_json::from_str(legacy).unwrap();
        assert!(restored.success);
        assert_eq!(
            restored.session_id, None,
            "a missing key reads as unstamped"
        );
    }

    #[test]
    fn a_stamped_session_id_round_trips() {
        let id = Uuid::new_v4();
        let status = DoneStatus::success("ok", 1, 1).with_session_id(Some(id));
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains(&format!("\"session_id\":\"{id}\"")),
            "the uuid serializes in the --resume spelling: {json}"
        );
        let restored: DoneStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_id, Some(id));
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
        write_done_file(&path, &status, None).unwrap();
        assert!(path.exists());
        let residue: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.to_ascii_lowercase().ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "temp residue: {residue:?}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"success\": true"));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_restricted_marker_preserves_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        std::fs::write(&path, "old").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&path, permissions).unwrap();

        write_done_file(&path, &DoneStatus::success("new", 1, 1), None).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "replacement must keep the marker's mode"
        );
        assert!(std::fs::read_to_string(&path).unwrap().contains("new"));
    }

    #[test]
    fn write_done_file_overwrites_an_existing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("done.json");
        std::fs::write(&path, "old").unwrap();
        let status = DoneStatus::success("new", 1, 1);
        write_done_file(&path, &status, None).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("new"));
    }
}
