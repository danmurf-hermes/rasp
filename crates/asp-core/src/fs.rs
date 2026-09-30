//! Filesystem helpers for loading pages safely from the application root.

use crate::error::{AspError, AspResult};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Split a page-relative or app-root-relative raw path into components,
/// rejecting anything that could leave the application root: absolute
/// paths, `..` components, and backslash separators.
pub fn confined_components(raw: &str) -> AspResult<Vec<OsString>> {
    if raw.contains('\\') || raw.contains('\0') {
        return Err(AspError::PathEscape(raw.to_string()));
    }
    let mut out = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => out.push(part.to_os_string()),
            Component::CurDir => {}
            _ => return Err(AspError::PathEscape(raw.to_string())),
        }
    }
    Ok(out)
}

/// Join `raw` onto `base` while keeping the result inside `root`.
///
/// `base` is the starting directory (application root or the directory of
/// the file containing the reference) and must itself be inside `root`.
pub fn confined_join(base: &Path, root: &Path, raw: &str) -> AspResult<PathBuf> {
    let parts = confined_components(raw)?;
    if parts.is_empty() {
        return Err(AspError::PathEscape(raw.to_string()));
    }
    let mut joined = base.to_path_buf();
    for part in parts {
        joined.push(part);
    }
    let canonical = joined.canonicalize().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => AspError::PageNotFound(raw.to_string()),
        _ => AspError::Io(format!("cannot resolve {}: {e}", joined.display())),
    })?;
    let root_canonical = root
        .canonicalize()
        .map_err(|e| AspError::Io(format!("cannot resolve app root: {e}")))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(AspError::PathEscape(raw.to_string()));
    }
    Ok(canonical)
}

/// Read a file that must exist inside `root`.
pub fn confined_read(root: &Path, raw: &str) -> AspResult<String> {
    let path = confined_join(root, root, raw)?;
    std::fs::read_to_string(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => AspError::PageNotFound(raw.to_string()),
        _ => AspError::Io(format!("cannot read {}: {e}", path.display())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rasp-fs-tests")
            .join(format!("{tag}-{}", std::process::id()));
        fs::create_dir_all(dir.join("inner")).unwrap();
        fs::write(dir.join("page.asp"), "<%= 1 %>").unwrap();
        dir
    }

    #[test]
    fn confined_join_resolves_inside_root() {
        let root = temp_root("join");
        let path = confined_join(&root, &root, "page.asp").unwrap();
        assert!(path.ends_with("page.asp"));
    }

    #[test]
    fn confined_join_rejects_traversal() {
        let root = temp_root("traverse");
        let err = confined_join(&root, &root, "../outside.asp").unwrap_err();
        assert!(matches!(err, AspError::PathEscape(_)));
        let err = confined_join(&root, &root, "../../etc/passwd").unwrap_err();
        assert!(matches!(err, AspError::PathEscape(_)));
    }

    #[test]
    fn confined_join_rejects_absolute_paths() {
        let root = temp_root("absolute");
        let err = confined_join(&root, &root, "/etc/passwd").unwrap_err();
        assert!(matches!(err, AspError::PathEscape(_)));
    }

    #[test]
    fn confined_read_reports_missing_files() {
        let root = temp_root("missing");
        let err = confined_read(&root, "nope.asp").unwrap_err();
        assert!(matches!(err, AspError::PageNotFound(_)));
    }
}
