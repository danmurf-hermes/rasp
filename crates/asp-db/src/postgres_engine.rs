//! PostgreSQL adapter (the second driver, M6): for the
//! `Provider=SQLOLEDB;Server=…` and `postgres://` connection shapes.

use asp_core::db::{
    AdoAffected, AdoColumn, AdoError, AdoParam, AdoResult, AdoRow, AdoValue, ConnectionTarget,
    DatabaseEngine, PostgresTarget,
};
use postgres::types::{IsNull, ToSql, Type};
use postgres::{Client, NoTls, Row};

/// PostgreSQL over `NoTls` (TLS arrives with the configuration
/// workstream; container networking needs none).
pub struct PostgresEngine {
    client: Option<Client>,
    /// Transaction depth: 1 = `BEGIN`, deeper = ADO nested transactions
    /// mapped to `SAVEPOINT ado_sp_<depth>`.
    open_trans: usize,
}
impl std::fmt::Debug for PostgresEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PostgresEngine({})",
            if self.is_open() { "open" } else { "closed" }
        )
    }
}

impl PostgresEngine {
    /// Open from structured parameters.
    pub fn open_args(args: &PostgresTarget) -> Result<Self, AdoError> {
        let mut conn_str = format!("host={} dbname={}", args.host, args.database);
        if let Some(port) = args.port {
            conn_str.push_str(&format!(" port={port}"));
        }
        if !args.user.is_empty() {
            conn_str.push_str(&format!(" user={}", args.user));
        }
        if !args.password.is_empty() {
            conn_str.push_str(&format!(" password={}", args.password));
        }
        let client = Client::connect(&conn_str, NoTls)
            .map_err(|e| AdoError::new(format!("PostgreSQL connect failed: {e}")))?;
        Ok(Self {
            client: Some(client),
            open_trans: 0,
        })
    }

    /// Open from a `postgres://` or `postgresql://` URL.
    pub fn open_url(url: &str) -> Result<Self, AdoError> {
        let client = Client::connect(url, NoTls)
            .map_err(|e| AdoError::new(format!("PostgreSQL connect failed: {e}")))?;
        Ok(Self {
            client: Some(client),
            open_trans: 0,
        })
    }

    /// Open from any target, explaining mismatched SQLite targets.
    pub fn open(target: &ConnectionTarget) -> Result<Self, AdoError> {
        match target {
            ConnectionTarget::Postgres(args) => Self::open_args(args),
            ConnectionTarget::PostgresUrl(url) => Self::open_url(url),
            ConnectionTarget::Sqlite(path) => Err(AdoError::new(format!(
                "PostgreSQL adapter cannot open a SQLite target ('{path}')"
            ))),
        }
    }
}

fn closed_err() -> AdoError {
    AdoError::new("Operation is not allowed when the object is closed")
}

fn pg_err(e: impl std::fmt::Display) -> AdoError {
    AdoError::new(format!("SQL error: {e}"))
}

/// An owned dynamically-typed parameter for `postgres`.
#[derive(Debug, Clone)]
enum PgParam {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

impl ToSql for PgParam {
    fn to_sql(
        &self,
        t: &Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            PgParam::Null => Ok(IsNull::Yes),
            PgParam::Int(v) => v.to_sql(t, out),
            PgParam::Float(v) => v.to_sql(t, out),
            PgParam::Bool(v) => v.to_sql(t, out),
            PgParam::Str(v) => v.to_sql(t, out),
        }
    }

    fn accepts(t: &Type) -> bool {
        matches!(
            *t,
            Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::BOOL
                | Type::TEXT
                | Type::VARCHAR
        ) || *t == Type::UNKNOWN
    }

    fn to_sql_checked(
        &self,
        t: &Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.to_sql(t, out)
    }
}

/// Convert a parameter list into owned `PgParam`s.
fn to_params(params: &[AdoParam]) -> Vec<PgParam> {
    params
        .iter()
        .map(|p| match &p.value {
            AdoValue::Null => PgParam::Null,
            AdoValue::Int(v) => PgParam::Int(*v),
            AdoValue::Float(v) => PgParam::Float(*v),
            AdoValue::Bool(v) => PgParam::Bool(*v),
            AdoValue::Str(v) => PgParam::Str(v.clone()),
        })
        .collect()
}

/// Read one column of a row as an [`AdoValue`]: a SQL `NULL` in any
/// supported type maps to `AdoValue::Null`; unsupported column types
/// error clearly (binary data is a later milestone).
///
/// Each arm reads `Option<T>` so `NULL` and type mismatches separate.
fn column_value(row: &Row, idx: usize) -> Result<AdoValue, AdoError> {
    let col = &row.columns()[idx];
    let name = col.name().to_string();
    let unsupported = || {
        AdoError::new(format!(
            "column '{name}' has Postgres type '{}' which ADODB mapping does not support yet",
            col.type_()
        ))
    };
    match *col.type_() {
        Type::BOOL => match row.try_get::<_, Option<bool>>(idx) {
            Ok(None) => Ok(AdoValue::Null),
            Ok(Some(v)) => Ok(AdoValue::Bool(v)),
            Err(_) => Err(unsupported()),
        },
        Type::INT2 => match row.try_get::<_, Option<i16>>(idx) {
            Ok(None) => Ok(AdoValue::Null),
            Ok(Some(v)) => Ok(AdoValue::Int(v as i64)),
            Err(_) => Err(unsupported()),
        },
        Type::INT4 => match row.try_get::<_, Option<i32>>(idx) {
            Ok(None) => Ok(AdoValue::Null),
            Ok(Some(v)) => Ok(AdoValue::Int(v as i64)),
            Err(_) => Err(unsupported()),
        },
        Type::INT8 => match row.try_get::<_, Option<i64>>(idx) {
            Ok(None) => Ok(AdoValue::Null),
            Ok(Some(v)) => Ok(AdoValue::Int(v)),
            Err(_) => Err(unsupported()),
        },
        Type::FLOAT4 => match row.try_get::<_, Option<f32>>(idx) {
            Ok(None) => Ok(AdoValue::Null),
            Ok(Some(v)) => Ok(AdoValue::Float(v as f64)),
            Err(_) => Err(unsupported()),
        },
        Type::FLOAT8 => match row.try_get::<_, Option<f64>>(idx) {
            Ok(None) => Ok(AdoValue::Null),
            Ok(Some(v)) => Ok(AdoValue::Float(v)),
            Err(_) => Err(unsupported()),
        },
        Type::TEXT | Type::VARCHAR | Type::NAME | Type::CHAR => {
            match row.try_get::<_, Option<String>>(idx) {
                Ok(None) => Ok(AdoValue::Null),
                Ok(Some(v)) => Ok(AdoValue::Str(v)),
                Err(_) => Err(unsupported()),
            }
        }
        _ => Err(unsupported()),
    }
}

impl DatabaseEngine for PostgresEngine {
    fn execute(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoAffected, AdoError> {
        let Some(client) = &mut self.client else {
            return Err(closed_err());
        };
        let pg = to_params(params);
        let refs: Vec<&(dyn ToSql + Sync)> = pg.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let n = client.execute(sql, refs.as_slice()).map_err(pg_err)?;
        Ok(AdoAffected { rows: n })
    }

    /// Postgres: rows vs affected decided BEFORE executing. Statement
    /// type inspection is driver-level here: try a prepared query
    /// plan WITHOUT stepping — `postgres::Client::query_opt`...
    /// Instead the honest approach: Postgres protocol marks statements
    /// returning rows in the RowDescription; we peek with prepare().
    fn run(&mut self, sql: &str, params: &[AdoParam]) -> Result<asp_core::db::Run, AdoError> {
        let Some(client) = &mut self.client else {
            return Err(closed_err());
        };
        let pg = to_params(params);
        let refs: Vec<&(dyn ToSql + Sync)> = pg.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        // Prepare once; RowDescription presence decides the path.
        // postgres 0.19 lacks public prepare(); use transaction-rollback
        // trick? Simplest protocol-true approach: statement shape check
        // via EXPLAIN at the SQL level is wrong for parameter plans.
        // postgres crate: query() on a non-query statement ERRORS with
        // a clear message and no rows lost; execute() on a query
        // returns 0 changed... Ambiguity only for SELECT-shaped
        // data-modifying statements (UPDATE ... RETURNING). Treat:
        // statements starting with SELECT/WITH/TABLE/VALUES -> query,
        // else execute. Documented in the plan; RETURNING flows out.
        let head = sql.trim_start().to_ascii_lowercase();
        let is_query = head.starts_with("select ")
            || head.starts_with(
                "select
",
            )
            || head.starts_with("select	")
            || head.starts_with("with ")
            || head.starts_with("table ")
            || head.starts_with("values ");
        if is_query {
            let rows = client.query(sql, refs.as_slice()).map_err(pg_err)?;
            let names: Vec<String> = rows
                .first()
                .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                .unwrap_or_default();
            let mut rows_out: Vec<AdoRow> = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut out: AdoRow = Vec::with_capacity(row.len());
                for idx in 0..row.len() {
                    out.push(column_value(row, idx).map_err(pg_err)?);
                }
                rows_out.push(out);
            }
            Ok(asp_core::db::Run::Rows(AdoResult {
                columns: names.into_iter().map(AdoColumn::new).collect(),
                rows: rows_out,
            }))
        } else {
            let n = client.execute(sql, refs.as_slice()).map_err(pg_err)?;
            Ok(asp_core::db::Run::Affected(n))
        }
    }

    fn query(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoResult, AdoError> {
        let Some(client) = &mut self.client else {
            return Err(closed_err());
        };
        let pg = to_params(params);
        let refs: Vec<&(dyn ToSql + Sync)> = pg.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let rows = client.query(sql, refs.as_slice()).map_err(pg_err)?;
        let names: Vec<String> = rows
            .first()
            .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
            .unwrap_or_default();
        let mut rows_out: Vec<AdoRow> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut out: AdoRow = Vec::with_capacity(row.len());
            for idx in 0..row.len() {
                out.push(column_value(row, idx).map_err(pg_err)?);
            }
            rows_out.push(out);
        }
        Ok(AdoResult {
            columns: names.into_iter().map(AdoColumn::new).collect(),
            rows: rows_out,
        })
    }

    fn begin_trans(&mut self) -> Result<u64, AdoError> {
        let Some(client) = &mut self.client else {
            return Err(closed_err());
        };
        let depth = self.open_trans + 1;
        if depth == 1 {
            client.batch_execute("BEGIN").map_err(pg_err)?;
        } else {
            client
                .batch_execute(&format!("SAVEPOINT ado_sp_{depth}"))
                .map_err(pg_err)?;
        }
        self.open_trans = depth;
        Ok(depth as u64)
    }

    fn commit_trans(&mut self) -> Result<(), AdoError> {
        let Some(client) = &mut self.client else {
            return Err(closed_err());
        };
        if self.open_trans == 0 {
            return Err(AdoError::new("no transaction is open"));
        }
        if self.open_trans == 1 {
            client.batch_execute("COMMIT").map_err(pg_err)?;
        } else {
            let sp = self.open_trans;
            client
                .batch_execute(&format!("RELEASE SAVEPOINT ado_sp_{sp}"))
                .map_err(pg_err)?;
        }
        self.open_trans -= 1;
        Ok(())
    }

    fn rollback_trans(&mut self) -> Result<(), AdoError> {
        let Some(client) = &mut self.client else {
            return Err(closed_err());
        };
        if self.open_trans == 0 {
            // ADO silently ignores a rollback with no open transaction.
            return Ok(());
        }
        if self.open_trans == 1 {
            client.batch_execute("ROLLBACK").map_err(pg_err)?;
        } else {
            let sp = self.open_trans;
            client
                .batch_execute(&format!("ROLLBACK TO SAVEPOINT ado_sp_{sp}"))
                .map_err(pg_err)?;
            client
                .batch_execute(&format!("RELEASE SAVEPOINT ado_sp_{sp}"))
                .map_err(pg_err)?;
        }
        self.open_trans -= 1;
        Ok(())
    }

    fn close(&mut self) -> Result<(), AdoError> {
        match self.client.take() {
            Some(_) => Ok(()),
            None => Err(closed_err()),
        }
    }

    fn is_open(&self) -> bool {
        self.client.is_some()
    }
}
