//! Detect-on-write conflict checks for the file-writing tools.
//!
//! Each writing tool re-verifies that its target has not changed on disk
//! since the tool captured its baseline, immediately before the write. This
//! narrows the read→write race window from the whole call to the gap between
//! the check and the write; that residual gap is deliberate (see
//! [`check_content_unchanged`]). Two baselines exist because the tools hold
//! different data: `Edit` and `MultiEdit` hold the bytes they read, so they
//! compare content exactly; `Write` never reads first, so it compares the
//! `mtime` that Read recorded for the path.

use std::path::Path;
use std::time::SystemTime;

use loopctl::tool::ToolError;

/// The file changed on disk between baseline capture and the write.
///
/// Recoverable: the model re-reads the file and re-issues the write against
/// the current content. Mirrors the existing soft-error shapes (the Edit
/// tool's not-found and ambiguous-match outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conflict {
    /// Bytes on disk differ from the baseline bytes.
    ///
    /// Used by `Edit` and `MultiEdit`, which hold the content they read and
    /// can therefore compare exactly.
    Content,

    /// The `mtime` on disk differs from the baseline `mtime`.
    ///
    /// Used by `Write`, whose baseline is the `mtime` Read recorded for the
    /// path.
    Mtime,
}

impl Conflict {
    /// Format this reason as the soft-error message returned to the loop.
    ///
    /// `path` names the target in the message. The text states what happened
    /// and directs the model to the recovery path (re-read, then re-issue).
    pub(crate) fn message(self, path: &Path) -> String {
        let guidance = "Re-read the file with Read, then re-issue the write against the \
             current content.";
        match self {
            Conflict::Content => format!(
                "{} changed on disk since it was read; not writing to avoid \
                 clobbering the newer content.\n\n{guidance}",
                path.display()
            ),
            Conflict::Mtime => format!(
                "{} changed on disk since it was last read (its modification \
                 time moved); not writing to avoid clobbering the newer \
                 content.\n\n{guidance}",
                path.display()
            ),
        }
    }
}

/// Why a conflict check did not approve the write.
///
/// The two outcomes demand different handling: `Changed` is recoverable and
/// surfaces to the model as a soft error, while `Fault` propagates as a hard
/// [`ToolError`].
#[derive(Debug)]
pub(crate) enum CheckFailure {
    /// The target differs from the baseline.
    ///
    /// A recoverable conflict: the caller refuses the write and surfaces
    /// [`Conflict::message`] as a soft error.
    Changed(Conflict),

    /// A genuine I/O fault while re-reading or statting the target.
    ///
    /// A missing file is a [`Conflict`], not a fault.
    Fault(ToolError),
}

/// Re-read `path` and compare its bytes against `baseline`.
///
/// Returns `Ok(())` when the bytes match exactly. A differing file, or a file
/// that no longer exists (a deletion between read and re-read is "the file
/// changed"), is `Err(CheckFailure::Changed)`; the caller refuses the write
/// and surfaces a soft error.
///
/// # Errors
///
/// Returns `Err(CheckFailure::Fault)` only on a genuine I/O fault that is not
/// "file missing" (e.g. a permission problem). Byte comparison is deliberate:
/// an external writer replacing the text file with non-UTF-8 content is a
/// content change, not a fault.
///
/// # Race window
///
/// This check narrows the read→write race window from "the whole tool call"
/// to "the gap between this compare and the caller's write" — it does not
/// eliminate the race. Closing it entirely would require OS-level file
/// locking, which this helper deliberately does not do.
pub(crate) async fn check_content_unchanged(
    baseline: &str,
    path: &Path,
) -> Result<(), CheckFailure> {
    match tokio::fs::read(path).await {
        Ok(current) if current == baseline.as_bytes() => Ok(()),
        Ok(_) => Err(CheckFailure::Changed(Conflict::Content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(CheckFailure::Changed(Conflict::Content))
        }
        Err(err) => Err(CheckFailure::Fault(ToolError::Execution(format!(
            "conflict check for {}: {err}",
            path.display()
        )))),
    }
}

/// `stat` `path` and compare its `mtime` against `baseline_mtime`.
///
/// Used by Write, whose baseline is the extrinsic `mtime` recorded at Read
/// time — Write holds no prior bytes to compare. Same-mtime edits are
/// possible on filesystems with coarse timestamps; that miss rate is the
/// accepted cost of not holding full content per read. Missing-at-reread is
/// a [`Conflict::Mtime`].
///
/// # Errors
///
/// Returns `Err(CheckFailure::Fault)` on a genuine I/O fault other than
/// "file missing".
pub(crate) async fn check_mtime_unchanged(
    baseline_mtime: SystemTime,
    path: &Path,
) -> Result<(), CheckFailure> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => match metadata.modified() {
            Ok(mtime) if mtime == baseline_mtime => Ok(()),
            Ok(_) => Err(CheckFailure::Changed(Conflict::Mtime)),
            Err(err) => Err(CheckFailure::Fault(ToolError::Execution(format!(
                "conflict check for {}: {err}",
                path.display()
            )))),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(CheckFailure::Changed(Conflict::Mtime))
        }
        Err(err) => Err(CheckFailure::Fault(ToolError::Execution(format!(
            "conflict check for {}: {err}",
            path.display()
        )))),
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

    fn temp_file(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    #[tokio::test]
    async fn content_check_passes_when_the_file_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        assert!(check_content_unchanged("A", &path).await.is_ok());
    }

    #[tokio::test]
    async fn content_check_fails_when_an_external_writer_changed_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        std::fs::write(&path, "EXTERNAL").unwrap();
        assert!(matches!(
            check_content_unchanged("A", &path).await,
            Err(CheckFailure::Changed(Conflict::Content))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "EXTERNAL",
            "the check must never write"
        );
    }

    #[tokio::test]
    async fn content_check_treats_a_deleted_file_as_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            check_content_unchanged("A", &path).await,
            Err(CheckFailure::Changed(Conflict::Content))
        ));
    }

    #[tokio::test]
    async fn content_check_maps_a_genuine_io_fault_to_a_fault() {
        let tmp = tempfile::tempdir().unwrap();
        // Reading a directory is an I/O fault, not a "changed file".
        assert!(matches!(
            check_content_unchanged("A", tmp.path()).await,
            Err(CheckFailure::Fault(ToolError::Execution(_)))
        ));
    }

    #[tokio::test]
    async fn content_check_detects_non_utf8_external_content_as_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.bin", "text");
        std::fs::write(&path, [0xFF, 0xFE, 0x00]).unwrap();
        assert!(
            matches!(
                check_content_unchanged("text", &path).await,
                Err(CheckFailure::Changed(Conflict::Content))
            ),
            "a binary swap is a content change, not an io fault"
        );
    }

    #[tokio::test]
    async fn mtime_check_passes_when_the_file_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        let baseline = crate::state::current_mtime(&path).await.unwrap();
        assert!(check_mtime_unchanged(baseline, &path).await.is_ok());
    }

    #[tokio::test]
    async fn mtime_check_fails_after_an_external_write_that_bumps_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        let baseline = crate::state::current_mtime(&path).await.unwrap();
        std::fs::write(&path, "EXTERNAL").unwrap();
        let bumped = baseline + std::time::Duration::from_secs(30);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(bumped).unwrap();
        assert!(matches!(
            check_mtime_unchanged(baseline, &path).await,
            Err(CheckFailure::Changed(Conflict::Mtime))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "EXTERNAL",
            "the check must never write"
        );
    }

    #[tokio::test]
    async fn mtime_check_treats_a_deleted_file_as_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        let baseline = crate::state::current_mtime(&path).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            check_mtime_unchanged(baseline, &path).await,
            Err(CheckFailure::Changed(Conflict::Mtime))
        ));
    }

    #[test]
    fn messages_name_the_file_and_the_recovery_path() {
        for reason in [Conflict::Content, Conflict::Mtime] {
            let message = reason.message(Path::new("src/a.rs"));
            assert!(message.contains("src/a.rs"), "{message}");
            assert!(message.contains("changed"), "{message}");
            assert!(message.contains("Read"), "{message}");
        }
    }
}
