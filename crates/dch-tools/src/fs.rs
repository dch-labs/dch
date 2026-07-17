//! Filesystem helpers shared by the write/edit tools.

use std::io::Write;
use std::path::Path;

use loopctl::tool::ToolError;

/// Write `content` to `target` atomically.
///
/// Writes to a temp file in the target's directory, then renames it into
/// place. Preserves the existing file's permissions when overwriting. The
/// temp file is co-located with the target so the rename is a single
/// filesystem operation with no torn-write window.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] on any failure creating, writing, or
/// persisting the temp file.
pub(crate) fn atomic_write(target: &Path, content: &str) -> Result<(), ToolError> {
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
    tmp.persist(target)
        .map_err(|e| ToolError::Execution(format!("Failed to persist file: {e}")))?;

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

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("out.txt");
        std::fs::write(&target, "old\n").unwrap();
        atomic_write(&target, "new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[test]
    fn atomic_write_no_temp_left() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("clean.rs");
        atomic_write(&target, "fn main() {}\n").unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["clean.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("script.sh");
        std::fs::write(&target, "#!/bin/bash\necho old\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write(&target, "#!/bin/bash\necho new\n").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "permissions should be preserved as 0o755, got 0o{:o}",
            mode & 0o777
        );
    }
}
