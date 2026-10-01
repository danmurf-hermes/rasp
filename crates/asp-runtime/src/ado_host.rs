//! The runtime's `AdoHost`: database access through the `asp-db`
//! adapters. SQLite paths are CONFINED to the app root (root-relative,
//! traversal rejected) exactly like FileSystemObject access; network
//! connection strings (Postgres) pass through unconfined.

use asp_core::db::parse_connection_string;
use asp_core::{AppRoot, fs as core_fs};
use asp_db::Engine;
use asp_vbscript::ado::{AdoHost, DbEngineHandle};
use std::cell::RefCell;
use std::rc::Rc;

/// Per-render database host bound to one app root.
pub struct RuntimeAdoHost {
    /// Resolve root-relative SQLite paths against this root.
    app: AppRoot,
}

impl RuntimeAdoHost {
    pub fn new(app: AppRoot) -> Self {
        Self { app }
    }

    /// Resolve a root-relative data-file path for OPEN-or-CREATE.
    /// `confined_components` rejects traversal outright (so `../x`
    /// never reaches the filesystem); the leaf file may not exist yet
    /// (SQLite creates it), so confinement is by construction: every
    /// component is a normal path segment joined onto the canonical
    /// root. Component checks are the gate, not filesystem tests.
    fn resolve_sqlite_path(&self, rel: &str) -> Result<String, String> {
        let parts = core_fs::confined_components(rel).map_err(|e| e.to_string())?;
        let mut resolved = self.app.path().to_path_buf();
        for part in parts {
            resolved.push(part);
        }
        Ok(resolved.to_string_lossy().into_owned())
    }
}

impl AdoHost for RuntimeAdoHost {
    fn db_open(&self, connection_string: &str) -> Result<DbEngineHandle, String> {
        let target = parse_connection_string(connection_string).map_err(|e| e.message)?;
        // Root-relative data files confine to the app root before
        // opening; the page keeps writing the portable root-relative
        // path while the engine sees an absolute one.
        let target = match target {
            asp_core::db::ConnectionTarget::Sqlite(rel) => {
                asp_core::db::ConnectionTarget::Sqlite(self.resolve_sqlite_path(&rel)?)
            }
            other => other,
        };
        let engine = Engine::open(&target).map_err(|e| e.message)?;
        Ok(Rc::new(RefCell::new(engine)))
    }
}
