//! Filesystem helpers shared by the write/edit tools.

use std::io::Write;
use std::path::Path;

use loopctl::tool::ToolError;

use crate::util::ResolvePolicy;

/// Write `content` to `target` atomically.
///
/// Writes to a temp file in the target's directory, then renames it into
/// place. Preserves the existing file's permissions when overwriting. The
/// temp file is co-located with the target so the rename is a single
/// filesystem operation with no torn-write window. Under
/// [`ResolvePolicy::Contained`] the opened temp handle is verified to have
/// resolved inside `workspace`, and the renamed result is re-checked the
/// same way (closing the check-to-use race a component swap would open);
/// Unrestricted dispatch skips both checks.
///
/// Under [`ResolvePolicy::Contained`], a `target` that is itself a symbolic
/// link is rejected: the rename would replace the link with a regular file,
/// silently severing it. Unrestricted dispatch writes onto the path as
/// given. Callers that resolve links themselves should pass the real path.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `target` is a symbolic link.
/// Returns [`ToolError::Execution`] on any failure creating, writing, or
/// persisting the temp file.
pub(crate) fn atomic_write(
    target: &Path,
    content: &str,
    workspace: &Path,
    policy: ResolvePolicy,
) -> Result<(), ToolError> {
    if policy == ResolvePolicy::Contained
        && std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(ToolError::InvalidInput(format!(
            "Refusing to write: {} is a symbolic link. Resolve it and pass the real path.",
            target.display()
        )));
    }

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| ToolError::Execution(format!("Failed to create temp file: {e}")))?;
    if policy == ResolvePolicy::Contained {
        crate::util::verify_handle_inside(tmp.as_file(), workspace)?;
    }
    if let Ok(meta) = std::fs::metadata(target) {
        let perms = meta.permissions();
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| ToolError::Execution(format!("Failed to set permissions: {e}")))?;
    }

    tmp.write_all(content.as_bytes())
        .map_err(|e| ToolError::Execution(format!("Failed to write temp file: {e}")))?;
    tmp.flush()
        .map_err(|e| ToolError::Execution(format!("Failed to flush temp file: {e}")))?;
    tmp.persist(target)
        .map_err(|e| ToolError::Execution(format!("Failed to persist file: {e}")))?;
    if policy == ResolvePolicy::Contained {
        ensure_renamed_inside(target, workspace)?;
    }

    Ok(())
}

/// Post-rename containment check: the renamed file must live in `workspace`.
///
/// `rename` re-resolves its destination, so a component swapped between the
/// handle verification and the rename can land the file outside the
/// workspace. When that is detected the escaped entry is removed and the
/// run fails. The removal never re-resolves a swap-prone path chain: a
/// substituted link entry is unlinked as given (unlink does not traverse a
/// final-entry symlink, so its referent — a path this code did not create
/// — is left untouched), and a regular-file escape is removed relative to
/// its real parent directory opened with `O_DIRECTORY | O_NOFOLLOW`, so a
/// swapped directory link cannot stand in for the parent on Linux.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the renamed file resolves outside
/// `workspace` (removing the entry first), or when containment cannot be
/// confirmed.
fn ensure_renamed_inside(target: &Path, workspace: &Path) -> Result<(), ToolError> {
    let actual = std::fs::canonicalize(target)
        .map_err(|e| ToolError::Execution(format!("cannot verify path containment: {e}")))?;
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|e| ToolError::Execution(format!("cannot verify path containment: {e}")))?;
    if actual.starts_with(&workspace) {
        return Ok(());
    }
    if std::fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink()) {
        drop(std::fs::remove_file(target));
    } else {
        remove_escaped_file(&actual, target);
    }
    Err(ToolError::Execution(format!(
        "Path escaped the working directory: {}",
        actual.display()
    )))
}

/// Remove a detected regular-file escape through its resolved parent.
///
/// Called when the post-rename check has established that `actual` — the
/// canonical form of `target` — lies outside the workspace and the final
/// entry is a regular file, so the removal can bind to the escape's real
/// parent. The canonical path's components are real directories at
/// resolution time: opening the parent pins the directory the escape
/// landed in, and only the entry's plain basename is unlinked through
/// that descriptor, never a re-resolved chain.
///
/// Best-effort by contract: the result is discarded and the run fails
/// with the containment error regardless, so a removal failure leaves a
/// leftover file behind, never a wrongly removed one.
#[cfg(target_os = "linux")]
fn remove_escaped_file(actual: &Path, target: &Path) {
    match (actual.parent(), actual.file_name()) {
        (Some(parent), Some(name)) => drop(remove_bounded(parent, name)),
        _ => drop(std::fs::remove_file(target)),
    }
}

/// Remove a detected escape where the descriptor API is unavailable.
///
/// Mirrors the Linux counterpart's contract on platforms without
/// `O_DIRECTORY` and `O_NOFOLLOW`: the substituted-link case never
/// reaches here, and unlinking the as-given path does not traverse a
/// final-entry symlink, so a substituted referent is not deleted.
/// Best-effort by contract — the result is discarded and the run fails
/// with the containment error regardless.
#[cfg(not(target_os = "linux"))]
fn remove_escaped_file(_actual: &Path, target: &Path) {
    drop(std::fs::remove_file(target));
}

/// Unlink `name` relative to `parent`, refusing to traverse any symlink.
///
/// The parent is opened with `O_DIRECTORY` and `O_NOFOLLOW` — a substituted
/// link can never stand in for the directory — and the entry is removed
/// relative to that descriptor, which never follows a final-entry link.
/// The residual window is a component of the parent's (already canonical,
/// outside-the-workspace) path being swapped between resolution and the
/// open. Best-effort by contract: callers discard the result and fail the
/// run regardless, so failure leaves a leftover file, never a wrongly
/// removed one.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the directory cannot be opened or
/// the entry cannot be removed.
#[cfg(target_os = "linux")]
fn remove_bounded(parent: &Path, name: &std::ffi::OsStr) -> Result<(), ToolError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let dir = CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;
    let entry = CString::new(name.as_bytes())
        .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;
    // SAFETY: both pointers refer to CStrings alive for the call's duration.
    let dir_fd = unsafe {
        libc::open(
            dir.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if dir_fd < 0 {
        return Err(ToolError::Execution(format!(
            "cannot open the escaped file's directory: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `dir_fd` is a valid open descriptor and `entry` outlives the call.
    let removed = unsafe { libc::unlinkat(dir_fd, entry.as_ptr(), 0) };
    let unlink_error = std::io::Error::last_os_error();
    // SAFETY: `dir_fd` was opened above and is closed exactly once.
    let _ = unsafe { libc::close(dir_fd) };
    if removed != 0 {
        return Err(ToolError::Execution(format!(
            "cannot remove the escaped entry: {unlink_error}"
        )));
    }
    Ok(())
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
    use crate::util::ResolvePolicy;

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("out.txt");
        std::fs::write(&target, "old\n").unwrap();
        atomic_write(&target, "new\n", tmp.path(), ResolvePolicy::Unrestricted).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[test]
    fn atomic_write_no_temp_left() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("clean.rs");
        atomic_write(
            &target,
            "fn main() {}\n",
            tmp.path(),
            ResolvePolicy::Unrestricted,
        )
        .unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["clean.rs"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("script.sh");
        std::fs::write(&target, "#!/bin/bash\necho old\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write(
            &target,
            "#!/bin/bash\necho new\n",
            tmp.path(),
            ResolvePolicy::Contained,
        )
        .unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "permissions should be preserved as 0o755, got 0o{:o}",
            mode & 0o777
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_rejects_symlink_target_without_clobbering() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.txt");
        std::fs::write(&real, "original\n").unwrap();
        let link = tmp.path().join("link.txt");
        symlink(&real, &link).unwrap();

        let err =
            atomic_write(&link, "new content\n", tmp.path(), ResolvePolicy::Contained).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref s) if s.contains("symbolic link")),
            "{err:?}"
        );

        // The symlink must remain a symlink (not replaced by a regular file)
        // and the linked-to file must be untouched.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link should still be a symlink"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "original\n");
    }

    #[test]
    fn renamed_inside_the_workspace_passes_the_post_check() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("done.txt");
        std::fs::write(&target, "x").unwrap();
        assert!(ensure_renamed_inside(&target, tmp.path()).is_ok());
    }

    #[test]
    fn renamed_outside_the_workspace_is_detected_and_removed() {
        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let target = outside.path().join("escaped.txt");
        std::fs::write(&target, "x").unwrap();

        let err = ensure_renamed_inside(&target, workspace.path()).unwrap_err();
        assert!(err.to_string().contains("escaped"), "{err}");
        assert!(!target.exists(), "the escaped file must be removed");
    }

    #[cfg(unix)]
    #[test]
    fn escape_through_a_symlinked_directory_is_removed_via_the_real_parent() {
        // The attack shape at rest: the workspace-side directory is a link
        // out, and the escaped file is a regular file behind it. Detection
        // resolves through the link, and the bounded cleanup pins the real
        // parent instead of re-walking the attacker-controlled chain.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let dir = workspace.path().join("dir");
        symlink(outside.path(), &dir).unwrap();
        let escaped = outside.path().join("escaped.txt");
        std::fs::write(&escaped, "x").unwrap();

        let err = ensure_renamed_inside(&dir.join("escaped.txt"), workspace.path()).unwrap_err();
        assert!(err.to_string().contains("escaped"), "{err}");
        assert!(!escaped.exists(), "the escaped entry must be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_of_a_substituted_symlink_unlinks_the_link_not_the_referent() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        let target = workspace.path().join("marker.json");
        symlink(&victim, &target).unwrap();

        let err = ensure_renamed_inside(&target, workspace.path()).unwrap_err();
        assert!(err.to_string().contains("escaped"), "{err}");
        assert!(victim.exists(), "the referent must survive the cleanup");
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "the substituted link entry itself must be removed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remove_bounded_refuses_a_symlinked_parent_and_spares_the_referent() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::TempDir::new().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        let link = outside.path().join("link");
        symlink(outside.path(), &link).unwrap();

        let err = remove_bounded(&link, std::ffi::OsStr::new("victim.txt")).unwrap_err();
        assert!(
            err.to_string().contains("cannot open"),
            "the symlinked parent must be refused: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "keep me",
            "the referent must survive the refused cleanup"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remove_bounded_reports_a_missing_entry() {
        let outside = tempfile::TempDir::new().unwrap();
        let err = remove_bounded(outside.path(), std::ffi::OsStr::new("absent.txt")).unwrap_err();
        assert!(
            err.to_string().contains("cannot remove"),
            "a missing entry is a reported removal failure: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_unrestricted_replaces_the_entry_as_given() {
        // The layered contract: atomic_write itself never resolves links —
        // unrestricted callers do that first. The rename replaces the link
        // entry with a regular file; the referent is untouched.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&real, "original\n").unwrap();
        symlink(&real, &link).unwrap();

        atomic_write(
            &link,
            "replacement\n",
            tmp.path(),
            ResolvePolicy::Unrestricted,
        )
        .unwrap();

        assert!(
            !std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the entry at the given path is replaced by a regular file"
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "original\n",
            "the referent must be untouched"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_write_rejects_a_swapped_parent_directory_when_contained() {
        // The TOCTOU race, deterministically: `resolve_path` validated the
        // target before `dir` was swapped for a symlink out of the
        // workspace. The fd verification must catch what the earlier walk
        // can no longer see.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let dir = workspace.path().join("dir");
        symlink(outside.path(), &dir).unwrap();

        let target = dir.join("file.txt");
        let err =
            atomic_write(&target, "x", workspace.path(), ResolvePolicy::Contained).unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref s) if s.contains("escaped")),
            "{err:?}"
        );
        assert!(
            !outside.path().join("file.txt").exists(),
            "the escape must not have written the file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_allows_a_swapped_parent_directory_when_unrestricted() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let dir = workspace.path().join("dir");
        symlink(outside.path(), &dir).unwrap();

        let target = dir.join("file.txt");
        atomic_write(&target, "x", workspace.path(), ResolvePolicy::Unrestricted).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }
}
