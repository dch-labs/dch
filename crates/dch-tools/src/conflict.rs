//! Detect-on-write conflict checks for the file-writing tools.
//!
//! Each writing tool re-verifies that its target's bytes have not changed
//! since the tool captured its baseline, immediately before the write. This
//! narrows the read→write race window from the whole call to the gap between
//! the check and the write; that residual gap is deliberate (both checks
//! document it). The tools hold baselines in two shapes, and each has a
//! matching check: `Edit` and `MultiEdit` hold the bytes they read and
//! compare them exactly; `Write` holds the content hash Read recorded for
//! the path and compares hashes.

use std::path::Path;

use loopctl::tool::ToolError;

/// Why a conflict check did not approve the write.
///
/// The two outcomes demand different handling: `Changed` is recoverable and
/// surfaces to the model as a soft error, while `Fault` propagates as a hard
/// [`ToolError`].
#[derive(Debug)]
pub(crate) enum CheckFailure {
    /// The target's bytes differ from the baseline.
    ///
    /// A recoverable conflict: the caller refuses the write and surfaces
    /// [`changed_message`] as a soft error.
    Changed,

    /// A genuine I/O fault while re-reading or statting the target.
    ///
    /// A missing file is a [`CheckFailure::Changed`], not a fault.
    Fault(ToolError),
}

/// Format the soft-error message for a refused write of `path`.
///
/// The text states what happened and directs the model to the recovery path
/// (re-read, then re-issue).
pub(crate) fn changed_message(path: &Path) -> String {
    format!(
        "{path} changed on disk since it was read; not writing to avoid \
         clobbering the newer content.\n\nRe-read the file with Read, then \
         re-issue the write against the current content.",
        path = path.display()
    )
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
    check_bytes_unchanged(baseline.as_bytes(), path).await
}

/// Re-read `path` and compare its content hash against `baseline_hash`.
///
/// Used by Write, whose baseline is the content hash Read recorded for the
/// path — Write holds no prior bytes to compare. Hash comparison inherits the
/// exactness of the underlying byte comparison and has no timestamp
/// blind spot: any content change is caught regardless of filesystem
/// timestamp granularity. Missing-at-reread is a conflict.
///
/// # Errors
///
/// Returns `Err(CheckFailure::Fault)` only on a genuine I/O fault that is not
/// "file missing".
///
/// # Race window
///
/// As with [`check_content_unchanged`], the check narrows the race window to
/// the gap between this compare and the caller's write; it does not eliminate
/// it.
pub(crate) async fn check_content_hash_unchanged(
    baseline_hash: u64,
    path: &Path,
) -> Result<(), CheckFailure> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            if crate::state::content_hash(&bytes) == baseline_hash {
                Ok(())
            } else {
                Err(CheckFailure::Changed)
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(CheckFailure::Changed),
        Err(err) => Err(CheckFailure::Fault(ToolError::Execution(format!(
            "conflict check for {}: {err}",
            path.display()
        )))),
    }
}

/// Shared body of the two checks: compare candidate bytes against a baseline.
///
/// # Errors
///
/// Returns `Err(CheckFailure::Fault)` only on a genuine I/O fault that is not
/// "file missing".
async fn check_bytes_unchanged(baseline: &[u8], path: &Path) -> Result<(), CheckFailure> {
    match tokio::fs::read(path).await {
        Ok(current) if current == baseline => Ok(()),
        Ok(_) => Err(CheckFailure::Changed),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(CheckFailure::Changed),
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
            Err(CheckFailure::Changed)
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
            Err(CheckFailure::Changed)
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
                Err(CheckFailure::Changed)
            ),
            "a binary swap is a content change, not an io fault"
        );
    }

    #[tokio::test]
    async fn hash_check_passes_when_the_file_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        let baseline = crate::state::content_hash(b"A");
        assert!(check_content_hash_unchanged(baseline, &path).await.is_ok());
    }

    #[tokio::test]
    async fn hash_check_catches_a_same_mtime_content_change() {
        // The regression that motivated the hash check: an external writer
        // swaps the bytes but pins the mtime back to the baseline's value.
        // The mtime method allowed this; the content hash does not.
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        let baseline_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let baseline_hash = crate::state::content_hash(b"A");
        std::fs::write(&path, "EXTERNAL").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(baseline_mtime).unwrap();

        assert!(matches!(
            check_content_hash_unchanged(baseline_hash, &path).await,
            Err(CheckFailure::Changed)
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "EXTERNAL",
            "the check must never write"
        );
    }

    #[tokio::test]
    async fn hash_check_treats_a_deleted_file_as_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = temp_file(tmp.path(), "a.txt", "A");
        let baseline = crate::state::content_hash(b"A");
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            check_content_hash_unchanged(baseline, &path).await,
            Err(CheckFailure::Changed)
        ));
    }

    #[test]
    fn changed_message_names_the_file_and_the_recovery_path() {
        let message = changed_message(Path::new("src/a.rs"));
        assert!(message.contains("src/a.rs"), "{message}");
        assert!(message.contains("changed"), "{message}");
        assert!(message.contains("Read"), "{message}");
    }
}
