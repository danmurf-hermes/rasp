//! SQLite adapter: the first practical driver (examples and tests).

use asp_core::db::{
    AdoAffected, AdoColumn, AdoError, AdoParam, AdoResult, AdoRow, AdoValue, DatabaseEngine,
};
use rusqlite::types::Value as Sqv;
use rusqlite::{Connection, OpenFlags};

/// SQLite with a single-level transaction stack: nested `BeginTrans`
/// is rejected with a clear count error (SQLite itself only has one
/// level, so faking depth would mislead applications).
pub struct SqliteEngine {
    conn: Option<Connection>,
    open_trans: bool,
}
impl std::fmt::Debug for SqliteEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SqliteEngine({})",
            if self.is_open() { "open" } else { "closed" }
        )
    }
}

impl SqliteEngine {
    /// Open (or create) the database file. `:memory:` opens an
    /// in-memory database; parent directories are created for a new
    /// file so an example can live in a fresh `data/` folder.
    pub fn open(path: &str) -> Result<Self, AdoError> {
        let conn = if path.trim() == ":memory:" {
            Connection::open_in_memory()
        } else {
            if let Some(parent) = std::path::Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AdoError::new(format!("SQLite open failed: {e}")))?;
            }
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
            Connection::open_with_flags(std::path::Path::new(path), flags)
        };
        let conn = conn.map_err(|e| AdoError::new(format!("SQLite open failed: {e}")))?;
        Ok(Self {
            conn: Some(conn),
            open_trans: false,
        })
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, AdoError>,
    ) -> Result<T, AdoError> {
        let Some(conn) = &self.conn else {
            return Err(closed_err());
        };
        f(conn)
    }
}

fn closed_err() -> AdoError {
    AdoError::new("Operation is not allowed when the object is closed")
}

fn sql_err(e: impl std::fmt::Display) -> AdoError {
    AdoError::new(format!("SQL error: {e}"))
}

/// Validate the parameter count against the prepared statement and
/// convert the values for positional binding.
fn bound_values(stmt: &rusqlite::Statement<'_>, params: &[AdoParam]) -> Result<Vec<Sqv>, AdoError> {
    let n_params = stmt.parameter_count();
    if params.len() != n_params {
        return Err(AdoError::new(format!(
            "statement expects {n_params} parameter(s), got {}",
            params.len()
        )));
    }
    Ok(params
        .iter()
        .map(|p| match &p.value {
            AdoValue::Null => Sqv::Null,
            AdoValue::Int(v) => Sqv::Integer(*v),
            AdoValue::Float(v) => Sqv::Real(*v),
            AdoValue::Bool(v) => Sqv::Integer(if *v { 1 } else { 0 }),
            AdoValue::Str(v) => Sqv::Text(v.clone()),
        })
        .collect())
}

/// Convert a rusqlite row value into an [`AdoValue`]. Blobs cannot be
/// represented yet (binary variant data is a later milestone), so they
/// surface as an error rather than silent data loss.
fn convert_value(v: Sqv, column: &str) -> Result<AdoValue, AdoError> {
    match v {
        Sqv::Null => Ok(AdoValue::Null),
        Sqv::Integer(i) => Ok(AdoValue::Int(i)),
        Sqv::Real(f) => Ok(AdoValue::Float(f)),
        Sqv::Text(s) => Ok(AdoValue::Str(s)),
        Sqv::Blob(_) => Err(AdoError::new(format!(
            "column '{column}' returned binary data, which ADODB mapping does not support yet"
        ))),
    }
}

impl DatabaseEngine for SqliteEngine {
    fn execute(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoAffected, AdoError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(sql).map_err(sql_err)?;
            let values = bound_values(&stmt, params)?;
            let count = stmt
                .execute(rusqlite::params_from_iter(values))
                .map_err(sql_err)?;
            Ok(AdoAffected { rows: count as u64 })
        })
    }

    /// SQLite: rows vs affected decided BEFORE stepping (statement
    /// column count from the prepared statement — a SELECT has output
    /// columns; INSERT/UPDATE/DELETE/DDL do not), so every statement
    /// executes exactly once.
    fn run(&mut self, sql: &str, params: &[AdoParam]) -> Result<asp_core::db::Run, AdoError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(sql).map_err(sql_err)?;
            let values = bound_values(&stmt, params)?;
            if stmt.column_count() == 0 {
                let count = stmt
                    .execute(rusqlite::params_from_iter(values))
                    .map_err(sql_err)?;
                Ok(asp_core::db::Run::Affected(count as u64))
            } else {
                let names: Vec<String> =
                    stmt.column_names().iter().map(|s| s.to_string()).collect();
                let mut rows = stmt
                    .query(rusqlite::params_from_iter(values))
                    .map_err(sql_err)?;
                let mut rows_out: Vec<AdoRow> = Vec::new();
                while let Some(row) = rows.next().map_err(sql_err)? {
                    let mut out: AdoRow = Vec::with_capacity(names.len());
                    for (idx, name) in names.iter().enumerate() {
                        let v: Sqv = row.get(idx).map_err(sql_err)?;
                        out.push(convert_value(v, name)?);
                    }
                    rows_out.push(out);
                }
                Ok(asp_core::db::Run::Rows(AdoResult {
                    columns: names.into_iter().map(AdoColumn::new).collect(),
                    rows: rows_out,
                }))
            }
        })
    }

    fn query(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoResult, AdoError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(sql).map_err(sql_err)?;
            let values = bound_values(&stmt, params)?;
            let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(values))
                .map_err(sql_err)?;
            let mut rows_out: Vec<AdoRow> = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                let mut out: AdoRow = Vec::with_capacity(names.len());
                for (idx, name) in names.iter().enumerate() {
                    let v: Sqv = row.get(idx).map_err(sql_err)?;
                    out.push(convert_value(v, name)?);
                }
                rows_out.push(out);
            }
            Ok(AdoResult {
                columns: names.into_iter().map(AdoColumn::new).collect(),
                rows: rows_out,
            })
        })
    }

    fn begin_trans(&mut self) -> Result<u64, AdoError> {
        if self.open_trans {
            return Err(AdoError::new("a transaction is already open"));
        }
        self.with_conn(|conn| conn.execute_batch("BEGIN").map_err(sql_err))?;
        self.open_trans = true;
        Ok(1)
    }

    fn commit_trans(&mut self) -> Result<(), AdoError> {
        if !self.open_trans {
            return Err(AdoError::new("no transaction is open"));
        }
        self.with_conn(|conn| conn.execute_batch("COMMIT").map_err(sql_err))?;
        self.open_trans = false;
        Ok(())
    }

    fn rollback_trans(&mut self) -> Result<(), AdoError> {
        if !self.open_trans {
            // ADO silently ignores a rollback with no open transaction.
            return Ok(());
        }
        self.with_conn(|conn| conn.execute_batch("ROLLBACK").map_err(sql_err))?;
        self.open_trans = false;
        Ok(())
    }

    fn close(&mut self) -> Result<(), AdoError> {
        match self.conn.take() {
            Some(_) => Ok(()),
            None => Err(closed_err()),
        }
    }

    fn is_open(&self) -> bool {
        self.conn.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> SqliteEngine {
        SqliteEngine::open(":memory:").unwrap()
    }

    #[test]
    fn query_round_trip() {
        let mut db = engine();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO t (name, score) VALUES (?, ?)",
            &[
                AdoValue::Str("Alice".into()).into(),
                AdoValue::Float(4.5).into(),
            ],
        )
        .unwrap();
        let rs = db
            .query("SELECT name, score FROM t ORDER BY id", &[])
            .unwrap();
        assert_eq!(rs.columns.len(), 2);
        assert_eq!(rs.columns[0].name, "name");
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0], AdoValue::Str("Alice".to_string()));
        assert_eq!(rs.rows[0][1], AdoValue::Float(4.5));
    }

    #[test]
    fn execute_reports_affected_rows() {
        let mut db = engine();
        db.execute("CREATE TABLE t (id INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO t (id) VALUES (1)", &[]).unwrap();
        db.execute("INSERT INTO t (id) VALUES (2)", &[]).unwrap();
        let affected = db
            .execute("UPDATE t SET id = id + 10 WHERE id > 1", &[])
            .unwrap();
        assert_eq!(affected.rows, 1);
    }

    #[test]
    fn parameter_count_mismatch_is_clear() {
        let mut db = engine();
        db.execute("CREATE TABLE t (a TEXT)", &[]).unwrap();
        let err = db
            .execute(
                "INSERT INTO t (a) VALUES (?)",
                &[
                    AdoValue::Str("a".into()).into(),
                    AdoValue::Str("b".into()).into(),
                ],
            )
            .unwrap_err();
        assert!(err.message.contains("expects 1 parameter(s), got 2"));
    }

    #[test]
    fn sql_error_surfaces_sqlite_message() {
        let mut db = engine();
        let err = db.execute("SELECT * FROM missing_table", &[]).unwrap_err();
        assert!(err.message.starts_with("SQL error:"));
    }

    #[test]
    fn null_params_and_columns() {
        let mut db = engine();
        db.execute("CREATE TABLE t (a TEXT)", &[]).unwrap();
        db.execute("INSERT INTO t (a) VALUES (?)", &[AdoValue::Null.into()])
            .unwrap();
        let rs = db.query("SELECT a FROM t", &[]).unwrap();
        assert_eq!(rs.rows[0][0], AdoValue::Null);
    }

    #[test]
    fn transactions_commit_and_rollback() {
        let mut db = engine();
        db.execute("CREATE TABLE t (a TEXT)", &[]).unwrap();
        assert_eq!(db.begin_trans().unwrap(), 1);
        db.execute("INSERT INTO t (a) VALUES ('x')", &[]).unwrap();
        db.rollback_trans().unwrap();
        assert!(db.query("SELECT a FROM t", &[]).unwrap().rows.is_empty());

        db.begin_trans().unwrap();
        db.execute("INSERT INTO t (a) VALUES ('y')", &[]).unwrap();
        db.commit_trans().unwrap();
        assert_eq!(db.query("SELECT a FROM t", &[]).unwrap().rows.len(), 1);
    }

    #[test]
    fn nested_transaction_rejected() {
        let mut db = engine();
        db.begin_trans().unwrap();
        let err = db.begin_trans().unwrap_err();
        assert!(err.message.contains("already open"));
    }

    #[test]
    fn rollback_without_trans_is_silent() {
        let mut db = engine();
        db.rollback_trans().unwrap();
        let err = db.commit_trans().unwrap_err();
        assert!(err.message.contains("no transaction is open"));
    }

    #[test]
    fn close_is_terminal() {
        let mut db = engine();
        db.close().unwrap();
        assert!(!db.is_open());
        let err = db.query("SELECT 1", &[]).unwrap_err();
        assert_eq!(
            err.message,
            "Operation is not allowed when the object is closed"
        );
        assert_eq!(db.close().unwrap_err().message, closed_err().message);
    }

    #[test]
    fn blob_columns_rejected_clearly() {
        let mut db = engine();
        db.execute("CREATE TABLE t (b BLOB)", &[]).unwrap();
        db.execute("INSERT INTO t (b) VALUES (x'0102')", &[])
            .unwrap();
        let err = db.query("SELECT b FROM t", &[]).unwrap_err();
        assert!(err.message.contains("binary data"));
    }

    #[test]
    fn open_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("rasp-sqlite-test-{}", std::process::id()));
        let path = dir.join("nested/db.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = SqliteEngine::open(path.to_str().unwrap()).unwrap();
        db.execute("CREATE TABLE t (a)", &[]).unwrap();
        assert!(path.is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn create_if_not_exists_twice_is_ok() {
    let mut db = SqliteEngine::open(":memory:").unwrap();
    db.execute("CREATE TABLE IF NOT EXISTS t (a TEXT)", &[])
        .unwrap();
    assert!(
        db.execute("CREATE TABLE IF NOT EXISTS t (a TEXT)", &[])
            .is_ok()
    );
}

#[test]
fn run_classifies_insert_as_affected() {
    let mut db = SqliteEngine::open(":memory:").unwrap();
    db.execute("CREATE TABLE t (a TEXT)", &[]).unwrap();
    let run = db
        .run(
            "INSERT INTO t (a) VALUES (?)",
            &[AdoValue::Str("x".into()).into()],
        )
        .unwrap();
    match run {
        asp_core::db::Run::Affected(n) => assert_eq!(n, 1),
        other => panic!("expected Affected, got {other:?}"),
    }
}
