//! Small helpers shared across tools.

use std::path::Path;

/// Recognized image extensions and their MIME types.
const IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// MIME type for an image extension, if recognized.
///
/// # Examples
///
/// ```
/// use dch_tools::util::mime_type_from_extension;
/// assert_eq!(mime_type_from_extension("png"), Some("image/png"));
/// assert_eq!(mime_type_from_extension("JPG"), Some("image/jpeg"));
/// assert_eq!(mime_type_from_extension("txt"), None);
/// ```
#[must_use]
pub fn mime_type_from_extension(ext: &str) -> Option<&'static str> {
    let ext_lower = ext.to_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext_lower)
        .map(|(_, mime)| *mime)
}

/// MIME type for a file path, based on its extension.
#[must_use]
pub fn mime_type_from_path(path: &Path) -> Option<&'static str> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(mime_type_from_extension)
}

/// Whether a path has a recognized image extension.
///
/// # Examples
///
/// ```
/// use dch_tools::util::is_image_file;
/// assert!(is_image_file("screenshot.png"));
/// assert!(is_image_file("photo.JPG"));
/// assert!(!is_image_file("document.txt"));
/// ```
#[must_use]
pub fn is_image_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| mime_type_from_extension(ext).is_some())
}

/// Whether a string looks like an HTTP(S) URL.
#[must_use]
pub fn is_url(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

/// Resolve a possibly-relative `file_path` against `cwd`.
///
/// Absolute paths are used as-is; relative paths are joined to `cwd`. This
/// is the shared path-resolution primitive used by every file-touching tool
/// (`Read`, `Write`, `Edit`, `MultiEdit`, `FileViewer`) so they can't drift apart.
#[must_use]
pub fn resolve_path(file_path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let path = std::path::Path::new(file_path);
    if path.is_relative() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
mod tests {
    use super::*;

    #[test]
    fn mime_type_from_extension_recognized() {
        assert_eq!(mime_type_from_extension("png"), Some("image/png"));
        assert_eq!(mime_type_from_extension("jpg"), Some("image/jpeg"));
        assert_eq!(mime_type_from_extension("jpeg"), Some("image/jpeg"));
        assert_eq!(mime_type_from_extension("webp"), Some("image/webp"));
        assert_eq!(mime_type_from_extension("gif"), Some("image/gif"));
    }

    #[test]
    fn mime_type_from_extension_case_insensitive() {
        assert_eq!(mime_type_from_extension("PNG"), Some("image/png"));
        assert_eq!(mime_type_from_extension("Jpg"), Some("image/jpeg"));
    }

    #[test]
    fn mime_type_from_extension_unrecognized() {
        assert_eq!(mime_type_from_extension("txt"), None);
        assert_eq!(mime_type_from_extension("rs"), None);
        assert_eq!(mime_type_from_extension(""), None);
    }

    #[test]
    fn mime_type_from_path_recognized() {
        assert_eq!(
            mime_type_from_path(std::path::Path::new("photo.png")),
            Some("image/png")
        );
        assert_eq!(
            mime_type_from_path(std::path::Path::new("/abs/path/to/img.JPEG")),
            Some("image/jpeg")
        );
    }

    #[test]
    fn mime_type_from_path_unrecognized() {
        assert_eq!(mime_type_from_path(std::path::Path::new("readme.md")), None);
        assert_eq!(mime_type_from_path(std::path::Path::new("noext")), None);
    }

    #[test]
    fn is_image_file_true_for_images() {
        assert!(is_image_file("screenshot.png"));
        assert!(is_image_file("photo.JPG"));
        assert!(is_image_file("/path/to/img.webp"));
        assert!(is_image_file("anim.gif"));
    }

    #[test]
    fn is_image_file_false_for_non_images() {
        assert!(!is_image_file("document.txt"));
        assert!(!is_image_file("archive.zip"));
        assert!(!is_image_file("noext"));
    }

    #[test]
    fn is_url_detects_http_and_https() {
        assert!(is_url("http://example.com"));
        assert!(is_url("https://example.com/page"));
    }

    #[test]
    fn is_url_rejects_non_urls() {
        assert!(!is_url("file:///tmp/x"));
        assert!(!is_url("src/main.rs"));
        assert!(!is_url("ftp://example.com"));
        assert!(!is_url(""));
    }
}
