//! ADO-compatible database adapters for RASP (Milestone 6).
//!
//! Concrete drivers live here and nowhere else: the engine talks to
//! [`asp_core::db::DatabaseEngine`] through the runtime's host, so
//! swapping or adding a backend never touches engine code.

pub mod postgres_engine;
pub mod sqlite_engine;

pub use asp_core::db::{ConnectionTarget, parse_connection_string};
pub use postgres_engine::PostgresEngine;
pub use sqlite_engine::SqliteEngine;

use asp_core::db::{AdoAffected, AdoError, AdoParam, AdoResult, DatabaseEngine, Run};

/// The engine a connection string resolved to. Concrete (no Box),
/// so the ADO connection state can carry it behind `RefCell` without
/// orphan-impl gymnastics.
pub enum Engine {
    Sqlite(SqliteEngine),
    Postgres(PostgresEngine),
}

impl Engine {
    /// Open a connection for a parsed connection string.
    pub fn open(target: &ConnectionTarget) -> Result<Self, AdoError> {
        match target {
            ConnectionTarget::Sqlite(path) => Ok(Engine::Sqlite(SqliteEngine::open(path)?)),
            ConnectionTarget::Postgres(args) => {
                Ok(Engine::Postgres(PostgresEngine::open_args(args)?))
            }
            ConnectionTarget::PostgresUrl(url) => {
                Ok(Engine::Postgres(PostgresEngine::open_url(url)?))
            }
        }
    }
}

impl DatabaseEngine for Engine {
    fn execute(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoAffected, AdoError> {
        match self {
            Engine::Sqlite(e) => e.execute(sql, params),
            Engine::Postgres(e) => e.execute(sql, params),
        }
    }

    fn query(&mut self, sql: &str, params: &[AdoParam]) -> Result<AdoResult, AdoError> {
        match self {
            Engine::Sqlite(e) => e.query(sql, params),
            Engine::Postgres(e) => e.query(sql, params),
        }
    }

    fn run(&mut self, sql: &str, params: &[AdoParam]) -> Result<Run, AdoError> {
        match self {
            Engine::Sqlite(e) => e.run(sql, params),
            Engine::Postgres(e) => e.run(sql, params),
        }
    }

    fn begin_trans(&mut self) -> Result<u64, AdoError> {
        match self {
            Engine::Sqlite(e) => e.begin_trans(),
            Engine::Postgres(e) => e.begin_trans(),
        }
    }

    fn commit_trans(&mut self) -> Result<(), AdoError> {
        match self {
            Engine::Sqlite(e) => e.commit_trans(),
            Engine::Postgres(e) => e.commit_trans(),
        }
    }

    fn rollback_trans(&mut self) -> Result<(), AdoError> {
        match self {
            Engine::Sqlite(e) => e.rollback_trans(),
            Engine::Postgres(e) => e.rollback_trans(),
        }
    }

    fn close(&mut self) -> Result<(), AdoError> {
        match self {
            Engine::Sqlite(e) => e.close(),
            Engine::Postgres(e) => e.close(),
        }
    }

    fn is_open(&self) -> bool {
        match self {
            Engine::Sqlite(e) => e.is_open(),
            Engine::Postgres(e) => e.is_open(),
        }
    }
}
