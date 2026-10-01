//! Shared AST, values, errors, page model, and include model for RASP.

pub mod cookie_sign;
pub mod db;
pub mod error;
pub mod fs;
pub mod parser;

pub use db::{AdoValue, ConnectionTarget, DatabaseEngine};
pub use error::{AspError, AspResult, Diagnostic};
pub use parser::Page;

/// The application root every page and include must stay inside.
///
/// Relative page paths (`hello.asp`, `inner/footer.asp`) are resolved
/// against it; absolute and `..`-carrying paths are rejected.
#[derive(Clone)]
pub struct AppRoot {
    root: std::path::PathBuf,
}

impl AppRoot {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Absolute filesystem path of the application root.
    pub fn path(&self) -> &std::path::Path {
        &self.root
    }

    /// Read a root-relative page, rejecting escape attempts.
    pub fn read_page(&self, root_relative: &str) -> AspResult<String> {
        fs::confined_read(&self.root, root_relative)
    }

    /// Resolve an include, honouring its `file` (sibling-relative) or
    /// `virtual` (root-relative) kind, with traversal protection.
    pub fn resolve_include(
        &self,
        kind_and_path: &str,
        parent_dir: Option<&std::path::Path>,
    ) -> AspResult<String> {
        let (kind, path) = kind_and_path
            .split_once(':')
            .ok_or_else(|| AspError::Io(format!("malformed include reference: {kind_and_path}")))?;
        let base: &std::path::Path = match (kind, parent_dir) {
            ("file", Some(dir)) => dir,
            ("file", None) => &self.root,
            ("virtual", _) => &self.root,
            _ => return Err(AspError::Io(format!("unknown include kind: {kind}"))),
        };
        // Virtual paths are root-relative and written with a leading
        // slash; strip it so the confined join stays relative.
        let virtual_path = if kind == "virtual" {
            path.trim_start_matches('/')
        } else {
            path
        };
        fs::confined_join(base, &self.root, virtual_path).and_then(|p| {
            std::fs::read_to_string(&p).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => AspError::IncludeNotFound(path.to_string()),
                _ => AspError::Io(format!("cannot read include: {e}")),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn build_app(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join("rasp-approot-tests")
            .join(format!("{tag}-{}", std::process::id()));
        fs::create_dir_all(root.join("inc")).unwrap();
        fs::write(root.join("hello.asp"), "<%= \"hi\" %>").unwrap();
        fs::write(root.join("inc").join("footer.asp"), "footer text").unwrap();
        root
    }

    #[test]
    fn reads_pages_from_root() {
        let dir = build_app("read");
        let app = AppRoot::new(&dir);
        assert_eq!(app.read_page("hello.asp").unwrap(), "<%= \"hi\" %>");
    }

    #[test]
    fn rejects_escape_attempts() {
        let dir = build_app("escape");
        let app = AppRoot::new(&dir);
        let err = app.read_page("../outside.asp").unwrap_err();
        assert!(matches!(err, AspError::PathEscape(_)));
        assert!(app.read_page("/etc/passwd").is_err());
    }

    #[test]
    fn file_include_resolves_against_parent_dir() {
        let dir = build_app("file-include");
        let app = AppRoot::new(&dir);
        let parent = dir.join("inc");
        let text = app
            .resolve_include("file:footer.asp", Some(&parent))
            .unwrap();
        assert_eq!(text, "footer text");
    }

    #[test]
    fn virtual_include_resolves_against_root() {
        let dir = build_app("virtual-include");
        let app = AppRoot::new(&dir);
        let text = app
            .resolve_include("virtual:/inc/footer.asp", None)
            .unwrap();
        assert_eq!(text, "footer text");
    }
}
