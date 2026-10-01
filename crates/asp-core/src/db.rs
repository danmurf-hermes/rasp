//! Database abstraction shared by the engine and the adapters (M6).
//!
//! The engine (`asp-vbscript`) stays database-blind: it describes work
//! through the types here, and a host implementation (the `asp-db`
//! crate, wired by the runtime) performs it. Nothing in this file may
//! depend on a concrete driver.

/// A value that crosses the ADO surface in either direction: VBScript
/// arguments on the way in, column values on the way out.
///
/// Binary values are deliberately absent: variant `Byte`/binary data
/// is a later milestone, so parameters and columns are limited to the
/// scalar shapes VBScript already has.
#[derive(Debug, Clone, PartialEq)]
pub enum AdoValue {
    /// Uninitialised argument (`Empty`); stored as SQL NULL.
    Null,
    /// Whole number (VBScript `Integer`/`Long`).
    Int(i64),
    /// Floating point (VBScript `Double`).
    Float(f64),
    /// Boolean (stored as its integer truth).
    Bool(bool),
    /// Text.
    Str(String),
}

impl AdoValue {
    /// The SQL type name used in error messages and diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            AdoValue::Null => "Empty",
            AdoValue::Int(_) => "Long",
            AdoValue::Float(_) => "Double",
            AdoValue::Bool(_) => "Boolean",
            AdoValue::Str(_) => "String",
        }
    }
}

/// Column metadata for one result set, in ordinal order.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoColumn {
    pub name: String,
}

impl AdoColumn {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// One result row: values in the same order as the result's columns.
pub type AdoRow = Vec<AdoValue>;

/// A materialised result set. ADO cursors are translated to plain
/// snapshot rows: `MoveNext` walks this vector.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdoResult {
    pub columns: Vec<AdoColumn>,
    pub rows: Vec<AdoRow>,
}

impl AdoResult {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn one_column(name: &str, rows: Vec<AdoValue>) -> Self {
        Self {
            columns: vec![AdoColumn::new(name)],
            rows: rows.into_iter().map(|v| vec![v]).collect(),
        }
    }
}

/// Outcome of an update statement.
///
/// Classic ADO returns `RecordsAffected` through a parameter; the
/// engine exposes the equivalent count here.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoAffected {
    pub rows: u64,
}

/// Outcome of running one statement through [`DatabaseEngine::run`].
#[derive(Debug, Clone, PartialEq)]
pub enum Run {
    /// The statement produced a result set.
    Rows(AdoResult),
    /// The statement updated rows (or executed DDL).
    Affected(u64),
}

/// Build an open recordset state: cursor on the first row, or at BOF
/// for an empty result (ADO's `BOF`/`EOF` both true when empty).
pub fn opened_position(rows: usize) -> i64 {
    if rows == 0 { -1 } else { 0 }
}

/// One positional parameter value for a parameterised statement.
/// Positional order (1-based caller-side) is the whole story: ADO's
/// named-parameter mode stays unsupported (see the plan's `@params`).
#[derive(Debug, Clone, PartialEq)]
pub struct AdoParam {
    pub value: AdoValue,
}

impl From<AdoValue> for AdoParam {
    fn from(value: AdoValue) -> Self {
        Self { value }
    }
}

/// Errors a driver or adapter raises; the engine renders these as
/// runtime errors and an open-failure aborts page rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoError {
    pub message: String,
}

impl AdoError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AdoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// One open connection. Statements run while holding the engine's
/// state lock, so `&mut self` is safe (no interior mutability needed).
pub trait DatabaseEngine {
    /// Execute a statement that returns no rows (`INSERT`/`UPDATE`/
    /// `DELETE`/DDL). `params` are positional `?` placeholders.
    fn execute(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoAffected, AdoError>;

    /// Execute a query and snapshot its rows.
    fn query(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoResult, AdoError>;

    /// Run one statement, choosing rows-vs-affected BEFORE executing
    /// so a statement never runs twice (ADO semantics: `Command.Execute`
    /// decides by the statement shape, not by catching a failed query).
    fn run(&mut self, sql: &str, params: &[AdoParam]) -> Result<Run, AdoError> {
        let result = self.query(sql, params)?;
        Ok(Run::Rows(result))
    }

    /// Begin a transaction and return its `begin_trans` depth token.
    fn begin_trans(&mut self) -> Result<u64, AdoError>;

    /// Commit the transaction opened most recently.
    fn commit_trans(&mut self) -> Result<(), AdoError>;

    /// Roll back the most recent open transaction (no-op when none is
    /// open — ADO silently ignores a rollback without `BeginTrans`).
    fn rollback_trans(&mut self) -> Result<(), AdoError>;

    /// Close the connection. Further calls return `ConnectionClosed`.
    fn close(&mut self) -> Result<(), AdoError>;

    /// Whether the connection is still open.
    fn is_open(&self) -> bool;
}

/// Parse a connection string into a driver choice plus its parameters.
///
/// Accepted forms (case-insensitive prefix, value not trimmed):
/// - `Provider=SQLOLEDB;Data Source=…` → Postgres (SQL Server syntax
///   with a Postgres backend, the common lift-and-shift shape)
/// - `Driver={{PostgreSQL Unicode};Server=…;Database=…;Uid=…;Pwd=…}`
/// - `postgres://user:pass@host:port/db[?param=…]`
/// - `mysql://…` → recognised and rejected with a clear message (the
///   adapter is planned, not built)
/// - a bare filesystem path or `file:` URL / `Data Source=…` → SQLite
pub fn parse_connection_string(s: &str) -> Result<ConnectionTarget, AdoError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(AdoError::new("connection string is empty"));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("provider=") {
        let map = parse_kv(trimmed);
        let provider = map
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("provider"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let source = map
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("data source"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if provider.eq_ignore_ascii_case("sqloledb") {
            if map.iter().any(|(k, _)| k.eq_ignore_ascii_case("server")) {
                // SQL Server-style with Server= → Postgres backend.
                let server = kv_get(&map, "Server").unwrap_or_default();
                let database = kv_get(&map, "Database").unwrap_or_default();
                let uid = kv_get(&map, "Uid").unwrap_or_default();
                let pwd = kv_get(&map, "Pwd").unwrap_or_default();
                let port = kv_get(&map, "Port");
                return Ok(ConnectionTarget::Postgres(PostgresTarget {
                    host: server,
                    port: port.and_then(|p| p.trim().parse::<u16>().ok()),
                    database,
                    user: uid,
                    password: pwd,
                }));
            }
            // OLE DB for a file-based provider: treat Data Source as SQLite.
            if !source.is_empty() {
                return Ok(ConnectionTarget::Sqlite(strip_file_prefix(&source)));
            }
            return Err(AdoError::new(
                "Provider=SQLOLEDB requires a Data Source or Server in the connection string",
            ));
        }
        return Err(AdoError::new(format!(
            "connection string Provider '{provider}' is not supported (supported: SQLOLEDB, PostgreSQL ODBC driver, SQLite, postgres:// URL)"
        )));
    }
    if lower.starts_with("driver=") {
        let map = parse_kv(trimmed);
        let driver = kv_get(&map, "Driver").unwrap_or_default();
        if driver.to_ascii_lowercase().contains("postgres") {
            return Ok(ConnectionTarget::Postgres(PostgresTarget {
                host: kv_get(&map, "Server").unwrap_or_default(),
                port: kv_get(&map, "Port").and_then(|p| p.trim().parse::<u16>().ok()),
                database: kv_get(&map, "Database").unwrap_or_default(),
                user: kv_get(&map, "Uid")
                    .or(kv_get(&map, "User Id"))
                    .unwrap_or_default(),
                password: kv_get(&map, "Pwd")
                    .or(kv_get(&map, "Password"))
                    .unwrap_or_default(),
            }));
        }
        return Err(AdoError::new(format!(
            "ODBC driver '{driver}' is not supported (supported: PostgreSQL Unicode)"
        )));
    }
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return Ok(ConnectionTarget::PostgresUrl(trimmed.to_string()));
    }
    if lower.starts_with("mysql://") || lower.starts_with("mariadb://") {
        return Err(AdoError::new(
            "MySQL/MariaDB connections are not supported yet (SQLite and Postgres are built)",
        ));
    }
    if let Some(_rest) = lower.strip_prefix("file:") {
        return Ok(ConnectionTarget::Sqlite(strip_file_prefix(trimmed)));
    }
    if lower.starts_with("data source=") {
        let map = parse_kv(trimmed);
        let source = kv_get(&map, "Data Source").unwrap_or_default();
        return Ok(ConnectionTarget::Sqlite(strip_file_prefix(&source)));
    }
    // Bare path: SQLite.
    Ok(ConnectionTarget::Sqlite(strip_file_prefix(trimmed)))
}

fn strip_file_prefix(s: &str) -> String {
    s.trim_start_matches("file:").to_string()
}

fn kv_get(map: &[(String, String)], key: &str) -> Option<String> {
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

/// Split `A=B;C=D` pairs (values may be `{braced}`).
fn parse_kv(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in s.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.find('=') {
            let key = part[..eq].trim().to_string();
            let mut value = part[eq + 1..].trim().to_string();
            if value.len() >= 2 && value.starts_with('{') && value.ends_with('}') {
                value = value[1..value.len() - 1].to_string();
            }
            out.push((key, value));
        }
    }
    out
}

/// What a connection string resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionTarget {
    Sqlite(String),
    Postgres(PostgresTarget),
    PostgresUrl(String),
}

/// Host/port parameters for a Postgres connection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PostgresTarget {
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    pub password: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_bare_path() {
        assert_eq!(
            parse_connection_string("/tmp/data/app.db").unwrap(),
            ConnectionTarget::Sqlite("/tmp/data/app.db".to_string())
        );
    }

    #[test]
    fn sqlite_file_url() {
        assert_eq!(
            parse_connection_string("file:data.sqlite").unwrap(),
            ConnectionTarget::Sqlite("data.sqlite".to_string())
        );
    }

    #[test]
    fn sqlite_data_source() {
        assert_eq!(
            parse_connection_string("Data Source=data/app.sqlite").unwrap(),
            ConnectionTarget::Sqlite("data/app.sqlite".to_string())
        );
    }

    #[test]
    fn sqloledb_without_server_is_sqlite() {
        assert_eq!(
            parse_connection_string("Provider=SQLOLEDB;Data Source=db/app.db").unwrap(),
            ConnectionTarget::Sqlite("db/app.db".to_string())
        );
    }

    #[test]
    fn sqloledb_with_server_is_postgres() {
        let t = parse_connection_string(
            "Provider=SQLOLEDB;Server=db.internal;Database=shop;Uid=app;Pwd=secret;Port=5433",
        )
        .unwrap();
        let ConnectionTarget::Postgres(p) = t else {
            panic!("expected Postgres target");
        };
        assert_eq!(p.host, "db.internal");
        assert_eq!(p.database, "shop");
        assert_eq!(p.user, "app");
        assert_eq!(p.password, "secret");
        assert_eq!(p.port, Some(5433));
    }

    #[test]
    fn odbc_postgres_driver() {
        let t = parse_connection_string(
            "Driver={{PostgreSQL Unicode};Server=localhost;Database=shop;Uid=app;Pwd=secret",
        )
        .unwrap();
        let ConnectionTarget::Postgres(p) = t else {
            panic!("expected Postgres target");
        };
        assert_eq!(p.database, "shop");
        assert_eq!(p.port, None);
    }

    #[test]
    fn postgres_url() {
        assert_eq!(
            parse_connection_string("postgres://u:p@host/db").unwrap(),
            ConnectionTarget::PostgresUrl("postgres://u:p@host/db".to_string())
        );
    }

    #[test]
    fn mysql_rejected_with_clear_message() {
        let err = parse_connection_string("mysql://u:p@host/db").unwrap_err();
        assert!(err.message.contains("MySQL"));
    }

    #[test]
    fn unknown_provider_rejected() {
        let err = parse_connection_string("Provider=Microsoft.Jet.OLEDB.4.0;Data Source=x.mdb")
            .unwrap_err();
        assert!(err.message.contains("not supported"));
    }

    #[test]
    fn empty_rejected() {
        assert!(parse_connection_string("  ").is_err());
    }
}
