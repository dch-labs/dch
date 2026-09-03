//! Filesystem helpers shared by the write/edit tools.

use std::io::Write;
use std::path::Path;

use loopctl::tool::ToolError;

use crate::conflict::TargetIdentity;
use crate::util::ResolvePolicy;

/// Write `content` to `target` atomically.
///
/// The temp file is co-located with the target and renamed into place, so
/// the switch is a single filesystem operation with no torn-write window.
/// The two policies differ in how much of the filesystem is trusted:
///
/// Under [`ResolvePolicy::Contained`] the write is *pinned*: the parent
/// chain is walked from the workspace anchor one component at a time with
/// `O_NOFOLLOW` — creating missing directories through that same link-free
/// chain — the temp file is created inside the pinned directory with
/// `openat`, and the persist is a `renameat` within that one descriptor.
/// Nothing after validation re-resolves a path component, so a concurrent
/// swap cannot relocate the write: placement outside the workspace is
/// impossible by construction. A symbolic link anywhere in the parent
/// chain, or as the final entry, is refused — resolve it and pass the real
/// path. Non-Linux platforms fail closed.
///
/// Under [`ResolvePolicy::Unrestricted`] the write is the plain path-based
/// counterpart: temp file in the target's directory, permissions preserved
/// from the existing entry, rename onto the path as given.
///
/// When `expected` carries the [`TargetIdentity`] a conflict check
/// captured, the target's entry is re-checked against it immediately
/// before the rename — by `fstatat` on the pinned descriptor under
/// Contained, by a path stat under Unrestricted — and a swapped or removed
/// target aborts the write instead of silently replacing a file that was
/// never compared. `None` skips the re-check: new-file writes and
/// platforms without a stable identity. The residual either way is the
/// universal rename-semantics window: an entry swapped in between that
/// stat and the rename (two adjacent syscalls) is replaced by the rename.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when a contained target or parent
/// component is a symbolic link. Returns [`ToolError::Execution`] when the
/// checked identity no longer matches the target's entry, when the walk
/// cannot reach or create the parent directory, and on any failure
/// creating, writing, or persisting the temp file.
pub(crate) fn atomic_write(
    target: &Path,
    content: &str,
    workspace: &Path,
    policy: ResolvePolicy,
    expected: Option<&TargetIdentity>,
) -> Result<(), ToolError> {
    match policy {
        ResolvePolicy::Unrestricted => path_write(target, content, expected),
        ResolvePolicy::Contained => {
            #[cfg(target_os = "linux")]
            {
                pinned_write(target, content, workspace, expected)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (target, content, workspace, expected);
                Err(ToolError::Execution(
                    "path containment cannot be verified on this platform".to_string(),
                ))
            }
        }
    }
}

/// The path-based write behind the Unrestricted policy.
///
/// Unrestricted is the documented no-probing mode: the temp file is created
/// in the target's directory by path, permissions are copied from the
/// existing entry by path, and the rename lands on the path as given. The
/// only check beyond I/O is the identity gate, which stats the path
/// immediately before the rename when a conflict check armed it.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the checked identity no longer
/// matches the path's entry, and on any failure creating, writing, or
/// persisting the temp file.
fn path_write(
    target: &Path,
    content: &str,
    expected: Option<&TargetIdentity>,
) -> Result<(), ToolError> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| ToolError::Execution(format!("Failed to create temp file: {e}")))?;
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

/// Unique-name counter for contained temp files, alongside the creating
/// process's id in the name.
#[cfg(target_os = "linux")]
static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A directory reached through the no-follow contained walk.
///
/// Owns every descriptor the walk opened — the workspace anchor and one per
/// descended component — and closes them all exactly once, on every path,
/// through [`Drop`]. The walk's rollback bookkeeping lives here too:
/// [`created`](Self::created) records the directories the walk itself made
/// as the index of their parent's descriptor in
/// [`walked`](Self::walked) plus the entry name, so a failed walk can
/// unlink them through still-open descriptors before anything is closed.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PinnedDir {
    /// The final walked directory — the parent the write targets.
    dir_fd: i32,

    /// Every descriptor the walk opened, workspace anchor first, `dir_fd`
    /// last.
    walked: Vec<i32>,

    /// Directories the walk created: the index into
    /// [`walked`](Self::walked) of each parent descriptor (pushed before
    /// descent, so always valid while the walk owns it) plus the entry
    /// name. Recorded immediately after `mkdirat` succeeds, before
    /// anything else can fail, so a created directory is never untracked.
    created: Vec<(usize, std::ffi::CString)>,
}

#[cfg(target_os = "linux")]
impl PinnedDir {
    /// Remove the directories this walk created, deepest first.
    ///
    /// Best-effort and empty-directory-only: an entry that gained content
    /// concurrently is left standing rather than force-deleted. Called on
    /// the walk's failure paths, while every descriptor is still open.
    fn remove_created(&mut self) {
        for (parent, name) in self.created.iter().rev() {
            let Some(dir_fd) = self.walked.get(*parent) else {
                continue;
            };
            // SAFETY: `dir_fd` is one of the still-open walk descriptors
            // and `name` outlives the call; `AT_REMOVEDIR` removes only an
            // empty directory.
            let _ = unsafe { libc::unlinkat(*dir_fd, name.as_ptr(), libc::AT_REMOVEDIR) };
        }
        self.created.clear();
    }
}

#[cfg(target_os = "linux")]
impl Drop for PinnedDir {
    fn drop(&mut self) {
        for fd in self.walked.drain(..) {
            // SAFETY: each descriptor was pushed exactly once and closed
            // only here, on every path.
            let _ = unsafe { libc::close(fd) };
        }
    }
}

/// Pin `parent` for a contained write: walk to it without following
/// symlinks, creating missing directories when `create_missing` is set.
///
/// The walk descends from the `workspace` anchor one component at a time,
/// opening each with `O_NOFOLLOW`, so a component swapped for a symbolic
/// link after validation can never be traversed — descent happens only
/// through a descriptor chain that stayed on the named, link-free path. A
/// symbolic link found anywhere in the chain is refused (matching the
/// batch tools' stricter pre-write posture): resolve it and pass the real
/// path. This is deliberately stricter than reads, which may traverse
/// in-workspace links — only operations that leave new filesystem entries
/// behind are no-follow. The workspace anchor itself is opened following
/// links: it is the operator-supplied root, while the descent — where a
/// concurrent swap would land — is strictly no-follow. `parent` may be
/// spelled through the anchor or through the workspace's resolved form
/// (both are accepted for containment); the walk anchors at whichever
/// spelling matched — physically the same directory.
///
/// With `create_missing`, directories that do not exist are created
/// through the pinned chain (`mkdirat` on the current descriptor) and
/// recorded so a later failure in the same walk removes them again —
/// empty-directory removal only, so a concurrently filled directory is
/// left standing rather than force-deleted.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when `parent` is not inside
/// `workspace`, when the anchor cannot be opened, when a component is a
/// symbolic link or cannot be opened or created, and — without
/// `create_missing` — when a component does not exist. On any error the
/// walk's own creations are rolled back and every descriptor is closed
/// before the error is returned.
#[cfg(target_os = "linux")]
fn open_contained_dir(
    parent: &Path,
    workspace: &Path,
    create_missing: bool,
) -> Result<PinnedDir, ToolError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // The parent may be spelled through the operator's anchor or through
    // the workspace's resolved form — `resolve_path` accepts both. Match
    // whichever spelling the parent uses and anchor the walk there; the
    // no-follow descent below the anchor is identical either way.
    let canonical_workspace =
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let (anchor, relative) = match parent.strip_prefix(workspace) {
        Ok(relative) => (workspace, relative),
        Err(_) => match parent.strip_prefix(&canonical_workspace) {
            Ok(relative) => (canonical_workspace.as_path(), relative),
            Err(_) => {
                return Err(ToolError::Execution(format!(
                    "cannot create directories safely: {} is not inside {}",
                    parent.display(),
                    workspace.display()
                )));
            }
        },
    };

    let mut pinned = PinnedDir {
        dir_fd: -1,
        walked: Vec::new(),
        created: Vec::new(),
    };
    let failure = (|| {
        let root = open_dir_fd(anchor, libc::O_RDONLY | libc::O_DIRECTORY)?;
        pinned.walked.push(root);
        let mut current = root;
        for component in relative.components() {
            let name = CString::new(component.as_os_str().as_bytes())
                .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;
            match openat_dir(current, &name) {
                Ok(fd) => {
                    pinned.walked.push(fd);
                    current = fd;
                }
                Err(error) => {
                    if is_symlink_entry(current, &name) {
                        return Err(ToolError::Execution(format!(
                            "Refusing to write: {} is a symbolic link. \
                             Resolve it and pass the real path.",
                            component.as_os_str().to_string_lossy()
                        )));
                    }
                    if error.kind() != std::io::ErrorKind::NotFound || !create_missing {
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
                    // Tracked before anything else can fail, so the
                    // rollback sees every directory the walk created.
                    pinned
                        .created
                        .push((pinned.walked.len().saturating_sub(1), name.clone()));
                    let fd = openat_dir(current, &name).map_err(|error| {
                        ToolError::Execution(format!(
                            "cannot open the created directory {}: {error}",
                            component.as_os_str().to_string_lossy()
                        ))
                    })?;
                    pinned.walked.push(fd);
                    current = fd;
                }
            }
        }
        pinned.dir_fd = current;
        Ok(())
    })();

    if failure.is_err() {
        // Rollback runs while every descriptor is still open; `Drop` then
        // closes them all exactly once.
        pinned.remove_created();
    }
    failure.map(|()| pinned)
}

/// The contained write: everything happens through the pinned directory.
///
/// The parent chain is walked no-follow (creating missing directories),
/// the final entry is refused if it is a symbolic link, permissions are
/// copied from the existing entry by descriptor, the temp file is created
/// in the pinned directory with `openat(O_CREAT | O_EXCL)` under a unique
/// name, and the persist is a `renameat` within that one descriptor. No
/// step after the walk resolves a path component, so the placement the
/// walk proved cannot be changed by a concurrent swap.
///
/// # Errors
///
/// Propagates [`open_contained_dir`]'s errors; returns
/// [`ToolError::InvalidInput`] for a symbolic-link final entry,
/// [`swap_abort`] when an armed identity no longer matches the entry
/// (missing included), and [`ToolError::Execution`] for temp-file,
/// write, or rename failures. A temp file that cannot be written is
/// unlinked before returning.
#[cfg(target_os = "linux")]
fn pinned_write(
    target: &Path,
    content: &str,
    workspace: &Path,
    expected: Option<&TargetIdentity>,
) -> Result<(), ToolError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let parent = target.parent().ok_or_else(|| {
        ToolError::Execution(format!(
            "cannot write to {}: no parent directory",
            target.display()
        ))
    })?;
    let name = target.file_name().ok_or_else(|| {
        ToolError::Execution(format!(
            "cannot write to {}: no file name",
            target.display()
        ))
    })?;
    let name = CString::new(name.as_bytes())
        .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;

    let pinned = open_contained_dir(parent, workspace, true)?;
    // One no-follow stat decides both the link refusal and the permission
    // copy: a separate stat for the mode would open a window where a racer
    // swaps in a symlink and the copy takes the link's 0o777 onto the new
    // file.
    let existing = fstatat_entry(pinned.dir_fd, &name, libc::AT_SYMLINK_NOFOLLOW).ok();
    if existing
        .as_ref()
        .is_some_and(|entry| (entry.st_mode & libc::S_IFMT) == libc::S_IFLNK)
    {
        return Err(ToolError::InvalidInput(format!(
            "Refusing to write: {} is a symbolic link. Resolve it and pass the real path.",
            target.display()
        )));
    }
    let (tmp_name, mut tmp_file) = create_temp_in(pinned.dir_fd)?;
    if let Some(existing) = &existing {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(existing.st_mode & 0o7777);
        tmp_file
            .set_permissions(perms)
            .map_err(|e| ToolError::Execution(format!("Failed to set permissions: {e}")))?;
    }
    if let Err(error) = tmp_file
        .write_all(content.as_bytes())
        .and_then(|()| tmp_file.flush())
    {
        drop(tmp_file);
        // SAFETY: `dir_fd` is the pinned directory and `tmp_name` names the
        // temp file this function created there.
        let _ = unsafe { libc::unlinkat(pinned.dir_fd, tmp_name.as_ptr(), 0) };
        return Err(ToolError::Execution(format!(
            "Failed to write temp file: {error}"
        )));
    }

    // The identity gate sits directly against the rename: an entry swapped
    // in between these two syscalls is the documented residual.
    if let Some(identity) = expected {
        match fstatat_entry(pinned.dir_fd, &name, libc::AT_SYMLINK_NOFOLLOW) {
            Ok(entry) if identity.matches_parts(entry.st_dev, entry.st_ino) => {}
            Ok(_) => {
                pinned_discard(pinned.dir_fd, &tmp_name);
                return Err(swap_abort(target));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                pinned_discard(pinned.dir_fd, &tmp_name);
                return Err(swap_abort(target));
            }
            Err(error) => {
                pinned_discard(pinned.dir_fd, &tmp_name);
                return Err(ToolError::Execution(format!(
                    "cannot re-check the write target {}: {error}",
                    target.display()
                )));
            }
        }
    }

    // SAFETY: all four arguments refer to the pinned directory's descriptor
    // and to names this function created or verified there; the rename is
    // contained within that single directory, so the placement the walk
    // proved cannot be changed by re-resolution.
    if unsafe {
        libc::renameat(
            pinned.dir_fd,
            tmp_name.as_ptr(),
            pinned.dir_fd,
            name.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        pinned_discard(pinned.dir_fd, &tmp_name);
        return Err(ToolError::Execution(format!(
            "Failed to persist file: {error}"
        )));
    }
    Ok(())
}

/// Unlink a failed write's temp file from the pinned directory.
///
/// Best-effort by contract: the result is discarded and the write's error
/// is returned regardless, so a cleanup failure leaves a temp file behind
/// rather than a wrongly deleted target.
#[cfg(target_os = "linux")]
fn pinned_discard(dir_fd: i32, tmp_name: &std::ffi::CString) {
    // SAFETY: `dir_fd` is the pinned directory and `tmp_name` names the
    // temp file this function created there.
    let _ = unsafe { libc::unlinkat(dir_fd, tmp_name.as_ptr(), 0) };
}

/// Create a uniquely named temp file inside the pinned directory.
///
/// The name is derived from the process id and a process-local counter,
/// created with `O_CREAT | O_EXCL` and mode `0o600` (subject to the
/// umask), with a bounded retry on the vanishingly unlikely collision. The
/// returned [`std::fs::File`] owns the descriptor and closes it on drop.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when a temp file cannot be created for
/// any reason other than a name collision, or when the collision retry
/// budget is exhausted.
#[cfg(target_os = "linux")]
fn create_temp_in(dir_fd: i32) -> Result<(std::ffi::CString, std::fs::File), ToolError> {
    use std::ffi::CString;
    use std::os::unix::io::FromRawFd;
    use std::sync::atomic::Ordering;

    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp_name = CString::new(format!(".tmp-{}-{sequence}", std::process::id()))
            .map_err(|_| ToolError::Execution("path contains a NUL byte".to_string()))?;
        // SAFETY: `dir_fd` is a valid open directory descriptor and
        // `tmp_name` outlives the call; the mode is subject to the umask
        // like any open-with-create.
        let fd = unsafe {
            libc::openat(
                dir_fd,
                tmp_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` is the descriptor `openat` just created for this
            // temp file, not owned anywhere else.
            return Ok((tmp_name, unsafe { std::fs::File::from_raw_fd(fd) }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(ToolError::Execution(format!(
                "Failed to create temp file: {error}"
            )));
        }
    }
    Err(ToolError::Execution(
        "Failed to create temp file: exhausted unique names".to_string(),
    ))
}

/// Stat `name` under the open directory `dir`, by descriptor.
///
/// Never resolves a path: the stat is relative to `dir`, so the answer
/// belongs to the entry in the pinned directory rather than to whatever a
/// re-resolved path would reach. `flags` selects following
/// (`AT_SYMLINK_NOFOLLOW` for the entry itself).
///
/// # Errors
///
/// Returns the OS error when the entry cannot be stated — including
/// `ENOENT` for a missing entry.
#[cfg(target_os = "linux")]
fn fstatat_entry(dir: i32, name: &std::ffi::CString, flags: i32) -> std::io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dir` is a valid open descriptor, `name` outlives the call,
    // and the stat buffer is writable for the duration of the call.
    let filled = unsafe { libc::fstatat(dir, name.as_ptr(), stat.as_mut_ptr(), flags) };
    if filled != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fstatat` fully initialized the buffer on success.
    Ok(unsafe { stat.assume_init() })
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
    fstatat_entry(dir, name, libc::AT_SYMLINK_NOFOLLOW)
        .is_ok_and(|stat| (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK)
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
/// instead of being traversed. The failure kind is not distinguishable by
/// [`std::io::ErrorKind`] alone (a loop error versus not-a-directory),
/// which is why the caller confirms the link case through
/// [`is_symlink_entry`].
///
/// # Errors
///
/// Returns the OS error when the entry cannot be opened as a directory,
/// including the failure a symbolic-link component produces under
/// `O_NOFOLLOW`.
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

    #[cfg(target_os = "linux")]
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
        // The escape, deterministically: the parent component is a link
        // out of the workspace. The no-follow walk refuses at that
        // component, before anything is created or written — the pinned
        // rename never reaches a path that could re-resolve elsewhere.
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
            matches!(err, ToolError::Execution(ref s) if s.contains("symbolic link")),
            "the walk must refuse the swapped component: {err:?}"
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
    fn open_contained_dir_builds_a_missing_chain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("a").join("b");
        open_contained_dir(&parent, tmp.path(), true).unwrap();
        assert!(parent.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_contained_dir_is_a_no_op_for_an_existing_chain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("a");
        std::fs::create_dir(&parent).unwrap();
        open_contained_dir(&parent, tmp.path(), false).unwrap();
        assert!(parent.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_contained_dir_requires_an_existing_parent_when_creation_is_off() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("absent");
        let err = open_contained_dir(&parent, tmp.path(), false).unwrap_err();
        assert!(err.to_string().contains("cannot descend"), "{err}");
        assert!(!parent.exists(), "nothing may be created without the flag");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_contained_dir_leaks_no_descriptors_across_a_successful_chain() {
        // The leak pin: every descriptor the walk opened — anchor and
        // components — must be closed by the time the `PinnedDir` is
        // dropped, on the success path included. Absolute `/proc/self/fd`
        // counts wobble with parallel tests' runtimes starting and
        // stopping, so the pin takes the minimum delta over several
        // windows: a real leak inflates every window (tens of descriptors
        // at ten calls each), while at least one window lands in a quiet
        // stretch and reads near zero.
        let count_open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let tmp = tempfile::TempDir::new().unwrap();
        let calls_per_window = 10;
        let min_delta = (0..5)
            .map(|window| {
                let before = count_open_fds();
                for i in 0..calls_per_window {
                    let parent = tmp.path().join(format!("w{window}a{i}")).join("b");
                    open_contained_dir(&parent, tmp.path(), true).unwrap();
                    assert!(parent.is_dir());
                }
                count_open_fds().saturating_sub(before)
            })
            .min()
            .unwrap();
        assert!(
            min_delta <= 8,
            "each call leaks descriptors: minimum window delta {min_delta} \
             over {calls_per_window} calls"
        );
        for window in 0..5 {
            drop(std::fs::remove_dir_all(
                tmp.path().join(format!("w{window}a0")),
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_contained_dir_refuses_a_swapped_symlink_component_without_creating() {
        // The escape, deterministically: the component was swapped for a
        // link pointing outside after validation. The no-follow walk must
        // refuse and create nothing on the far side.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), workspace.path().join("dir")).unwrap();
        let parent = workspace.path().join("dir").join("nested");

        let err = open_contained_dir(&parent, workspace.path(), true).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(
            !outside.path().join("nested").exists(),
            "nothing may be created beyond the link"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_contained_dir_refuses_an_in_workspace_symlink_component() {
        // The stricter creation posture, pinned: even a link resolving
        // inside the workspace is refused — resolve it and pass the real
        // path.
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let real = workspace.path().join("real");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, workspace.path().join("link")).unwrap();
        let parent = workspace.path().join("link").join("new");

        let err = open_contained_dir(&parent, workspace.path(), true).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(!real.join("new").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_contained_dir_rolls_back_created_dirs_on_a_later_failure() {
        // A failure on a later component must not leave the earlier
        // walk-created chain behind. The trigger is deterministic: a
        // component longer than NAME_MAX fails only after its predecessor
        // was created. (A symlink refusal cannot double as the trigger —
        // a symlink's parent must pre-exist, so nothing can have been
        // created before the walk reaches it.)
        let workspace = tempfile::TempDir::new().unwrap();
        let long = "n".repeat(300);
        let parent = workspace.path().join("a").join(&long);

        let err = open_contained_dir(&parent, workspace.path(), true).unwrap_err();
        assert!(err.to_string().contains("cannot"), "{err}");
        assert!(
            !workspace.path().join("a").exists(),
            "the partially created chain must be rolled back"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_write_creates_parents_and_lands_the_file() {
        // End-to-end contained write of a new file into a missing chain:
        // the walk creates `a/b` inside the pinned parent and the rename
        // lands within it — and no temp file survives. The descriptor
        // accounting uses the same min-delta measurement as the walk pin:
        // the anchor, the walked components, and the temp file must all be
        // closed on every call.
        let count_open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let workspace = tempfile::TempDir::new().unwrap();
        let calls_per_window = 10;
        let min_delta = (0..5)
            .map(|window| {
                let before = count_open_fds();
                for i in 0..calls_per_window {
                    let target = workspace
                        .path()
                        .join(format!("w{window}a{i}"))
                        .join("b")
                        .join("new.rs");
                    atomic_write(
                        &target,
                        "fn main() {}\n",
                        workspace.path(),
                        ResolvePolicy::Contained,
                        None,
                    )
                    .unwrap();
                    assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn main() {}\n");
                }
                count_open_fds().saturating_sub(before)
            })
            .min()
            .unwrap();
        assert!(
            min_delta <= 8,
            "each write leaks descriptors: minimum window delta {min_delta} \
             over {calls_per_window} calls"
        );
        let left: Vec<_> = std::fs::read_dir(workspace.path().join("w0a0").join("b"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["new.rs"], "no temp file may be left");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn atomic_write_contained_aborts_when_the_target_changed_since_the_check() {
        // The fd analogue of the unrestricted swap test: the newcomer is
        // created while the checked file lives and renamed over it, so its
        // identity provably differs, and the pinned write must abort at the
        // pre-rename `fstatat` — leaving the newcomer and no temp file.
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
            ResolvePolicy::Contained,
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
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "no temp file may be left: {entries:?}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn atomic_write_contained_aborts_when_the_target_vanished_since_the_check() {
        // Missing-at-rename is a conflict here too, matching the check's
        // own missing-at-reread doctrine — and nothing may be recreated.
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("gone.txt");
        std::fs::write(&target, "checked\n").unwrap();
        let identity = check_content_unchanged("checked\n", &target).await.unwrap();
        std::fs::remove_file(&target).unwrap();

        let err = atomic_write(
            &target,
            "ours\n",
            tmp.path(),
            ResolvePolicy::Contained,
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
}
