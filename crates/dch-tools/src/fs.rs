//! Filesystem helpers shared by the write/edit tools.

use std::io::Write;
use std::path::Path;

use loopctl::tool::ToolError;

use crate::conflict::TargetIdentity;
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
/// When `expected` carries the [`TargetIdentity`] a conflict check captured,
/// the path's entry is compared against it immediately before the rename: a
/// target swapped (or removed) between the byte comparison and this point
/// aborts the write instead of silently replacing a file that was never
/// compared. `None` skips the re-check — new-file writes and platforms
/// without a stable identity.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `target` is a symbolic link.
/// Returns [`ToolError::Execution`] when the checked identity no longer
/// matches the path's entry, and on any failure creating, writing, or
/// persisting the temp file.
pub(crate) fn atomic_write(
    target: &Path,
    content: &str,
    workspace: &Path,
    policy: ResolvePolicy,
    expected: Option<&TargetIdentity>,
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
    if let Some(identity) = expected {
        let current = match std::fs::metadata(target) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(swap_abort(target));
            }
            Err(err) => {
                return Err(ToolError::Execution(format!(
                    "cannot re-check the write target {}: {err}",
                    target.display()
                )));
            }
        };
        if !identity.matches(&current) {
            return Err(swap_abort(target));
        }
    }
    tmp.persist(target)
        .map_err(|e| ToolError::Execution(format!("Failed to persist file: {e}")))?;
    if policy == ResolvePolicy::Contained {
        ensure_renamed_inside(target, workspace)?;
    }

    Ok(())
}

/// The abort for a target that changed between the conflict check and the
/// rename.
///
/// The swap is a fault rather than a plain content conflict, so the error
/// stays on the hard [`ToolError`] channel the write's plumbing already
/// carries. The message still points at the soft path's recovery: the
/// model re-reads the file and re-issues the write against the current
/// content.
fn swap_abort(target: &Path) -> ToolError {
    ToolError::Execution(format!(
        "{} changed while the write was being prepared; re-read it and retry \
         the write.",
        target.display()
    ))
}

/// Create every missing directory of `parent` without following symlinks.
///
/// The contained counterpart of `create_dir_all`: the walk descends from
/// `workspace` one component at a time, opening each with `O_NOFOLLOW`, so
/// a component swapped for a symbolic link after validation can never be
/// traversed — directories are only ever created through a descriptor
/// chain that stayed on the named, link-free path. A symbolic link found
/// anywhere in the parent chain is refused (matching the batch tools'
/// stricter pre-write posture): resolve it and pass the real path. This is
/// deliberately stricter than reads, which may traverse in-workspace
/// links — only creation, which leaves new filesystem entries behind, is
/// no-follow.
///
/// Directories created before a failure are removed again, empty-directory
/// removal only, so a concurrently filled directory is left standing
/// rather than force-deleted. The workspace root itself is opened
/// following links: it is the operator-supplied anchor, while the descent
/// — where a concurrent swap would land — is strictly no-follow.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when `parent` is not inside
/// `workspace`, when the workspace root cannot be opened, when a parent
/// component is a symbolic link or not a directory, or when a directory
/// cannot be created. Non-Linux platforms fail closed without creating
/// anything, consistent with the platform's other contained checks.
#[cfg(target_os = "linux")]
pub(crate) fn create_contained_dirs(parent: &Path, workspace: &Path) -> Result<(), ToolError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let relative = parent.strip_prefix(workspace).map_err(|_| {
        ToolError::Execution(format!(
            "cannot create directories safely: {} is not inside {}",
            parent.display(),
            workspace.display()
        ))
    })?;

    let mut opened: Vec<i32> = Vec::new();
    let mut created: Vec<(i32, CString)> = Vec::new();
    let failure = (|| {
        let root = open_dir_fd(workspace, libc::O_RDONLY | libc::O_DIRECTORY)?;
        opened.push(root);
        let mut current = root;
        for component in relative.components() {
            let name = CString::new(component.as_os_str().as_bytes())
                .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;
            match openat_dir(current, &name) {
                Ok(fd) => {
                    opened.push(fd);
                    current = fd;
                }
                Err(error) => {
                    if is_symlink_entry(current, &name) {
                        return Err(ToolError::Execution(format!(
                            "Refusing to create directories: {} is a \
                             symbolic link. Resolve it and pass the real \
                             path.",
                            component.as_os_str().to_string_lossy()
                        )));
                    }
                    if error.kind() != std::io::ErrorKind::NotFound {
                        return Err(ToolError::Execution(format!(
                            "cannot descend into {}: {error}",
                            component.as_os_str().to_string_lossy()
                        )));
                    }
                    // SAFETY: `current` is a valid open directory descriptor
                    // and `name` outlives the call; the mode is subject to
                    // the umask like any mkdir(2).
                    if unsafe { libc::mkdirat(current, name.as_ptr(), 0o777) } != 0 {
                        return Err(ToolError::Execution(format!(
                            "cannot create directory {}: {}",
                            component.as_os_str().to_string_lossy(),
                            std::io::Error::last_os_error()
                        )));
                    }
                    let fd = openat_dir(current, &name).map_err(|error| {
                        ToolError::Execution(format!(
                            "cannot open the created directory {}: {error}",
                            component.as_os_str().to_string_lossy()
                        ))
                    })?;
                    opened.push(fd);
                    // SAFETY: duplicating the valid parent descriptor for the
                    // rollback table; the duplicate is closed exactly once
                    // after the walk.
                    let parent = unsafe { libc::dup(current) };
                    if parent < 0 {
                        return Err(ToolError::Execution(format!(
                            "cannot track the created directory {}: {}",
                            component.as_os_str().to_string_lossy(),
                            std::io::Error::last_os_error()
                        )));
                    }
                    created.push((parent, name));
                    current = fd;
                }
            }
        }
        Ok(())
    })();

    for fd in opened {
        // SAFETY: each descriptor was pushed exactly once by the walk.
        let _ = unsafe { libc::close(fd) };
    }
    if failure.is_err() {
        for (dir_fd, name) in created.into_iter().rev() {
            // Best-effort rollback: empty-directory removal only, so a
            // directory that gained content concurrently is left standing.
            // SAFETY: `dir_fd` is a valid duplicate and `name` outlives the
            // call.
            let _ = unsafe { libc::unlinkat(dir_fd, name.as_ptr(), libc::AT_REMOVEDIR) };
            // SAFETY: each duplicate is closed exactly once.
            let _ = unsafe { libc::close(dir_fd) };
        }
    }
    failure
}

/// Non-Linux fallback: fail closed without creating anything.
///
/// The bounded walk needs descriptor-relative directory operations this
/// platform build does not implement; contained writes already fail closed
/// at their handle verification here, so refusing before any directory is
/// created only moves the failure earlier — and leaves no entries behind.
///
/// # Errors
///
/// Always returns [`ToolError::Execution`].
#[cfg(not(target_os = "linux"))]
pub(crate) fn create_contained_dirs(_parent: &Path, _workspace: &Path) -> Result<(), ToolError> {
    Err(ToolError::Execution(
        "directories cannot be created under containment on this platform".to_string(),
    ))
}

/// Open `path` as a directory descriptor with `flags`.
///
/// The workspace anchor is opened through this helper so every descriptor
/// the walk holds carries close-on-exec. The `flags` argument is the
/// caller's containment statement: the anchor itself is opened following
/// links (it is the operator-supplied root), while the walk's own
/// [`openat_dir`] calls add `O_NOFOLLOW` to their flags.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the path cannot be opened as a
/// directory, carrying the OS error.
#[cfg(target_os = "linux")]
fn open_dir_fd(path: &Path, flags: i32) -> Result<i32, ToolError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;
    // SAFETY: `path` outlives the call.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(ToolError::Execution(format!(
            "cannot open {}: {}",
            path.to_string_lossy(),
            std::io::Error::last_os_error()
        )))
    } else {
        Ok(fd)
    }
}

/// Open `name` under the open directory `dir`, refusing symbolic links.
///
/// One component per call is what makes the bounded walk sound: with
/// `O_NOFOLLOW`, `O_DIRECTORY`, and `O_CLOEXEC` set, a component can only
/// resolve to a real directory descriptor, and a symbolic link fails
/// instead of being traversed. With `O_DIRECTORY` in the mix the kernel
/// reports that failure as a generic not-a-directory rather than a loop
/// error, which is why the caller confirms the link case through
/// [`is_symlink_entry`].
///
/// # Errors
///
/// Returns the OS error when the entry cannot be opened as a directory —
/// including the not-a-directory or loop failure a symbolic-link component
/// produces under `O_NOFOLLOW`.
#[cfg(target_os = "linux")]
fn openat_dir(dir: i32, name: &std::ffi::CString) -> std::io::Result<i32> {
    // SAFETY: `dir` is a valid open descriptor and `name` outlives the call;
    // O_NOFOLLOW makes a symbolic-link component fail instead of traverse.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

/// Whether the entry `name` under the open directory `dir` is a symlink.
///
/// `O_NOFOLLOW` reports a symbolic-link component as a generic
/// not-a-directory or loop error, indistinguishable from a component that
/// is genuinely not a directory. Statting the entry without following —
/// `fstatat` with `AT_SYMLINK_NOFOLLOW`, relative to the same descriptor
/// the failed open used — separates the two, so the walk can refuse links
/// with a precise message. An entry that cannot itself be stated is
/// reported as not-a-link and lands in the caller's generic error path.
#[cfg(target_os = "linux")]
fn is_symlink_entry(dir: i32, name: &std::ffi::CString) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dir` is a valid open descriptor, `name` outlives the call,
    // and the stat buffer is writable for the duration of the call.
    let filled = unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if filled != 0 {
        return false;
    }
    // SAFETY: `fstatat` fully initialized the buffer on success.
    let stat = unsafe { stat.assume_init() };
    (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK
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
    use crate::conflict::check_content_unchanged;
    use crate::util::ResolvePolicy;

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("out.txt");
        std::fs::write(&target, "old\n").unwrap();
        atomic_write(
            &target,
            "new\n",
            tmp.path(),
            ResolvePolicy::Unrestricted,
            None,
        )
        .unwrap();
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
            None,
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
            None,
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

        let err = atomic_write(
            &link,
            "new content\n",
            tmp.path(),
            ResolvePolicy::Contained,
            None,
        )
        .unwrap_err();
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
            None,
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
        let err = atomic_write(
            &target,
            "x",
            workspace.path(),
            ResolvePolicy::Contained,
            None,
        )
        .unwrap_err();
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
        atomic_write(
            &target,
            "x",
            workspace.path(),
            ResolvePolicy::Unrestricted,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }

    #[tokio::test]
    async fn atomic_write_aborts_when_the_target_changed_since_the_check() {
        // The swap window, deterministically: the conflict check compared
        // one file, the path's entry was replaced before the write, and the
        // identity re-check must abort instead of replacing the newcomer.
        // The replacement is created while the checked file still exists
        // and renamed into place — rename never changes the inode, and two
        // simultaneously live files cannot share one, so the swap cannot
        // alias the checked identity the way a remove-then-recreate can
        // when the filesystem reclaims the freed inode.
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("watched.txt");
        std::fs::write(&target, "checked\n").unwrap();
        let identity = check_content_unchanged("checked\n", &target).await.unwrap();
        let newcomer = tmp.path().join("swapped-in.txt");
        std::fs::write(&newcomer, "swapped in\n").unwrap();
        std::fs::rename(&newcomer, &target).unwrap();

        let err = atomic_write(
            &target,
            "ours\n",
            tmp.path(),
            ResolvePolicy::Unrestricted,
            Some(&identity),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("changed while the write was being prepared"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "swapped in\n",
            "the swapped-in file must be untouched"
        );
    }

    #[tokio::test]
    async fn atomic_write_proceeds_when_the_identity_still_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("stable.txt");
        std::fs::write(&target, "old\n").unwrap();
        let identity = check_content_unchanged("old\n", &target).await.unwrap();

        atomic_write(
            &target,
            "new\n",
            tmp.path(),
            ResolvePolicy::Unrestricted,
            Some(&identity),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn atomic_write_aborts_when_the_target_vanished_since_the_check() {
        // Missing-at-rename is a conflict, matching the check's own
        // missing-at-reread doctrine — and nothing may be recreated.
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("gone.txt");
        std::fs::write(&target, "checked\n").unwrap();
        let identity = check_content_unchanged("checked\n", &target).await.unwrap();
        std::fs::remove_file(&target).unwrap();

        let err = atomic_write(
            &target,
            "ours\n",
            tmp.path(),
            ResolvePolicy::Unrestricted,
            Some(&identity),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("changed while the write was being prepared"),
            "{err}"
        );
        assert!(!target.exists(), "the vanished target must stay absent");
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.is_empty(), "no temp file may be left: {entries:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn create_contained_dirs_builds_a_missing_chain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("a").join("b");
        create_contained_dirs(&parent, tmp.path()).unwrap();
        assert!(parent.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn create_contained_dirs_is_a_no_op_for_an_existing_chain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("a");
        std::fs::create_dir(&parent).unwrap();
        create_contained_dirs(&parent, tmp.path()).unwrap();
        assert!(parent.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn create_contained_dirs_refuses_a_swapped_symlink_component_without_creating() {
        // The escape, deterministically: the component was swapped for a
        // link pointing outside after validation. The no-follow walk must
        // refuse and create nothing on the far side.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), workspace.path().join("dir")).unwrap();
        let parent = workspace.path().join("dir").join("nested");

        let err = create_contained_dirs(&parent, workspace.path()).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(
            !outside.path().join("nested").exists(),
            "nothing may be created beyond the link"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn create_contained_dirs_refuses_an_in_workspace_symlink_component() {
        // The stricter creation posture, pinned: even a link resolving
        // inside the workspace is refused during creation — resolve it and
        // pass the real path.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let real = workspace.path().join("real");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, workspace.path().join("link")).unwrap();
        let parent = workspace.path().join("link").join("new");

        let err = create_contained_dirs(&parent, workspace.path()).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(!real.join("new").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn create_contained_dirs_rolls_back_created_dirs_on_a_later_failure() {
        // A failure on a later component must not leave the earlier
        // walk-created chain behind. The trigger is deterministic: a
        // component longer than NAME_MAX fails only after its predecessor
        // was created. (A symlink refusal cannot double as the trigger —
        // a symlink's parent must pre-exist, so nothing can have been
        // created before the walk reaches it.)
        let workspace = tempfile::TempDir::new().unwrap();
        let long = "n".repeat(300);
        let parent = workspace.path().join("a").join(&long);

        let err = create_contained_dirs(&parent, workspace.path()).unwrap_err();
        assert!(err.to_string().contains("cannot"), "{err}");
        assert!(
            !workspace.path().join("a").exists(),
            "the partially created chain must be rolled back"
        );
    }
}
