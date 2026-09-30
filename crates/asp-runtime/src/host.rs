//! The runtime's `NativeHost`: filesystem access via the app root
//! (asp-core's confinement helpers) and sub-page rendering for
//! `Server.Execute`/`Transfer`.

use crate::{StateStores, render_page_stores};
use asp_core::AppRoot;
use asp_vbscript::{NativeHost, SubPage};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// Host bound to one application root. Files are addressed with
/// root-relative paths; every operation is confined (asp-core
/// confinement helpers reject `..`, absolute paths, and symlinks that
/// resolve outside the root).
pub struct RuntimeHost {
    app: AppRoot,
    state: RefCell<StateStores>,
    server_variables: HashMap<String, String>,
    request_data: HashMap<String, String>,
}

impl RuntimeHost {
    pub fn new(
        app: AppRoot,
        state: StateStores,
        server_variables: HashMap<String, String>,
        request_data: HashMap<String, String>,
    ) -> Rc<Self> {
        Rc::new(Self {
            app,
            state: RefCell::new(state),
            server_variables,
            request_data,
        })
    }

    /// State stores possibly mutated by `Server.Execute` sub-renders
    /// (the sub page shares the same session/application).
    pub fn take_state(&self) -> StateStores {
        let mut s = self.state.borrow_mut();
        std::mem::take(&mut *s)
    }
}

/// Convert an asp-core error into the host's string form.
fn host_msg(err: asp_core::AspError) -> String {
    match err {
        asp_core::AspError::PathEscape(_) => "path escapes the application root".to_string(),
        asp_core::AspError::PageNotFound(path) => format!("file not found: {path}"),
        asp_core::AspError::Io(msg) => msg,
        other => format!("{other}"),
    }
}

/// Resolve a root-relative path for WRITE operations: the result must
/// stay inside the root, but the file itself may not exist yet.
fn resolve_for_write(app: &AppRoot, path: &str) -> Result<PathBuf, String> {
    let parts = asp_core::fs::confined_components(path).map_err(host_msg)?;
    if parts.is_empty() {
        return Err("path escapes the application root".to_string());
    }
    let last = parts.last().unwrap().clone();
    let mut joined = app.path().to_path_buf();
    for part in &parts {
        joined.push(part);
        if joined.exists() && joined.is_file() && *part != last {
            return Err(format!("not a folder: {}", joined.display()));
        }
    }
    // The deepest existing ancestor must stay inside the root.
    let mut ancestor = joined.clone();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| "path escapes the application root".to_string())?
            .to_path_buf();
    }
    let canonical = ancestor
        .canonicalize()
        .map_err(|e| format!("cannot resolve: {e}"))?;
    let root_canonical = app
        .path()
        .canonicalize()
        .map_err(|e| format!("cannot resolve app root: {e}"))?;
    if !canonical.starts_with(&root_canonical) {
        return Err("path escapes the application root".to_string());
    }
    Ok(joined)
}

fn resolve(app: &AppRoot, path: &str) -> Result<PathBuf, String> {
    asp_core::fs::confined_join(app.path(), app.path(), path).map_err(host_msg)
}

impl NativeHost for RuntimeHost {
    fn map_path(&self, path: &str) -> Result<String, String> {
        // MapPath is a pure string mapping; it must not require the file
        // to exist, so normalise without canonicalise-then-exist checks:
        // strip a leading slash and reject any traversal outright.
        let stripped = path.trim_start_matches('/');
        let parts = asp_core::fs::confined_components(stripped).map_err(host_msg)?;
        let mut out = PathBuf::from(".");
        for p in parts {
            out.push(p);
        }
        Ok(out.to_string_lossy().replace('\\', "/"))
    }

    fn file_exists(&self, path: &str) -> Result<bool, String> {
        match asp_core::fs::confined_join(self.app.path(), self.app.path(), path) {
            Ok(p) => Ok(p.is_file()),
            // A missing file is a `False`, not an error (FSO semantics);
            // escapes and IO errors stay errors.
            Err(asp_core::AspError::PageNotFound(_)) => Ok(false),
            Err(e) => Err(host_msg(e)),
        }
    }

    fn folder_exists(&self, path: &str) -> Result<bool, String> {
        match asp_core::fs::confined_join(self.app.path(), self.app.path(), path) {
            Ok(p) => Ok(p.is_dir()),
            Err(asp_core::AspError::PageNotFound(_)) => Ok(false),
            Err(e) => Err(host_msg(e)),
        }
    }

    fn read_text_file(&self, path: &str) -> Result<String, String> {
        let p = resolve(&self.app, path)?;
        std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))
    }

    fn create_text_file(&self, path: &str, content: &str, overwrite: bool) -> Result<(), String> {
        let p = resolve_for_write(&self.app, path)?;
        if p.exists() && !overwrite {
            return Err(format!("file already exists: {path}"));
        }
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        std::fs::write(&p, content).map_err(|e| format!("cannot write {}: {e}", p.display()))
    }

    fn append_text_file(&self, path: &str, content: &str) -> Result<(), String> {
        use std::io::Write;
        let p = resolve_for_write(&self.app, path)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map_err(|e| format!("cannot append {}: {e}", p.display()))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("cannot append {}: {e}", p.display()))
    }

    fn delete_file(&self, path: &str, force: bool) -> Result<(), String> {
        let p = resolve(&self.app, path)?;
        if !p.exists() {
            return Err(format!("file not found: {path}"));
        }
        if !force {
            // Without force, refuse read-only files (best-effort mirror
            // of FSO semantics using permissions).
            let meta = std::fs::metadata(&p).map_err(|e| format!("{e}"))?;
            if meta.permissions().readonly() {
                return Err(format!("file is read-only: {path}"));
            }
        }
        std::fs::remove_file(&p).map_err(|e| format!("cannot delete {}: {e}", p.display()))
    }

    fn delete_folder(&self, path: &str) -> Result<(), String> {
        let p = resolve(&self.app, path)?;
        if !p.is_dir() {
            return Err(format!("folder not found: {path}"));
        }
        std::fs::remove_dir_all(&p).map_err(|e| format!("cannot delete {}: {e}", p.display()))
    }

    fn create_folder(&self, path: &str) -> Result<(), String> {
        let p = resolve(&self.app, path)?;
        if p.exists() {
            return Err(format!("folder already exists: {path}"));
        }
        std::fs::create_dir(&p).map_err(|e| format!("cannot create {}: {e}", p.display()))
    }

    fn copy_file(&self, from: &str, to: &str, overwrite: bool) -> Result<(), String> {
        let from = resolve(&self.app, from)?;
        let to = resolve(&self.app, to)?;
        if to.exists() && !overwrite {
            return Err(format!("file already exists: {to:?}"));
        }
        std::fs::copy(&from, &to)
            .map(|_| ())
            .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))
    }

    fn move_file(&self, from: &str, to: &str) -> Result<(), String> {
        let from = resolve(&self.app, from)?;
        let to = resolve(&self.app, to)?;
        if to.exists() {
            return Err(format!("file already exists: {to:?}"));
        }
        std::fs::rename(&from, &to)
            .map_err(|e| format!("cannot move {} to {}: {e}", from.display(), to.display()))
    }

    fn list_folder(&self, path: &str) -> Result<Vec<String>, String> {
        let stripped = path.trim_start_matches('/');
        let p = if stripped.is_empty() || stripped == "." {
            // The root itself is a legal listing target.
            self.app.path().to_path_buf()
        } else {
            resolve(&self.app, path)?
        };
        if !p.is_dir() {
            return Err(format!("folder not found: {path}"));
        }
        let mut names: Vec<String> = std::fs::read_dir(&p)
            .map_err(|e| format!("cannot list {}: {e}", p.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    fn render_sub_page(&self, path: &str) -> Result<SubPage, String> {
        // Share the CURRENT state stores with the sub page; its output
        // body/status/cookies are handed back to the caller to merge.
        let state = self.state.borrow().clone();
        let (out, exit_state) = render_page_stores(
            &self.app,
            path.trim_start_matches('/'),
            self.request_data.clone(),
            self.server_variables.clone(),
            &state,
        )
        .map_err(host_msg)?;
        *self.state.borrow_mut() = exit_state;
        Ok(SubPage {
            body: out.body,
            status: out.status.unwrap_or(200),
            set_cookies: out
                .cookies
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect(),
        })
    }
}
