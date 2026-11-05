//! Portable SQLite operations, atomic migrations and native/host adapters.

use indexmap::IndexMap;
use std::{any::Any, future::Future, pin::Pin, sync::Arc};

pub mod cloudflare;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
mod queue;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{NativeSqliteDatabase, NativeSqliteDatabaseOptions, open_native_sqlite_database};

#[derive(Clone, Debug, PartialEq)]
pub enum SqliteValue {
    Null,
    Real(f64),
    Integer(i64),
    Text(String),
    Blob(Vec<u8>),
}
impl From<i64> for SqliteValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}
impl From<&str> for SqliteValue {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
pub type SqliteRow = IndexMap<String, SqliteValue>;

#[derive(Debug)]
pub enum SqliteErrorKind {
    Message(String),
    External(Arc<dyn std::error::Error + Send + Sync>),
    Rollback {
        primary: SqliteError,
        rollback: SqliteError,
    },
}
#[derive(Clone, Debug)]
pub struct SqliteError(Arc<SqliteErrorKind>);
impl SqliteError {
    /// Preserve a caller's transaction error across the rollback boundary.
    pub fn external(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Arc::new(SqliteErrorKind::External(Arc::new(error))))
    }
    pub fn new(message: impl Into<String>) -> Self {
        Self(Arc::new(SqliteErrorKind::Message(message.into())))
    }
    pub fn rollback(primary: Self, rollback: Self) -> Self {
        Self(Arc::new(SqliteErrorKind::Rollback { primary, rollback }))
    }
    pub fn kind(&self) -> &SqliteErrorKind {
        &self.0
    }
    pub fn same_error(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl std::fmt::Display for SqliteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_ref() {
            SqliteErrorKind::Message(m) => f.write_str(m),
            SqliteErrorKind::External(error) => error.fmt(f),
            SqliteErrorKind::Rollback { .. } => {
                f.write_str("SQLite transaction failed and rollback failed")
            }
        }
    }
}
impl std::error::Error for SqliteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.kind() {
            SqliteErrorKind::External(error) => Some(error.as_ref()),
            SqliteErrorKind::Rollback { primary, .. } => Some(primary),
            _ => None,
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl From<rusqlite::Error> for SqliteError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new(value.to_string())
    }
}
pub type SqliteResult<T> = Result<T, SqliteError>;
pub type SqliteFuture<T> = Pin<Box<dyn Future<Output = SqliteResult<T>> + Send + 'static>>;
pub type SqliteTransactionResult = Box<dyn Any + Send>;
pub type SqliteTransactionCallback =
    Box<dyn FnOnce(Arc<dyn SqliteExecutor>) -> SqliteFuture<SqliteTransactionResult> + Send>;

/// Operations enqueue when called; cancelling the returned waiter does not cancel
/// SQL already submitted, or release an active transaction's queue barrier.
pub trait SqliteExecutor: Send + Sync {
    fn exec(&self, sql: &str) -> SqliteFuture<()>;
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()>;
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>>;
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>>;
}
pub trait SqliteDatabase: SqliteExecutor {
    fn transaction_raw(
        &self,
        callback: SqliteTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult>;
    fn close(&self) -> SqliteFuture<()>;
}
pub trait SqliteDatabaseExt: SqliteDatabase {
    fn transaction<T, F, Fut>(&self, callback: F) -> SqliteFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn SqliteExecutor>) -> Fut + Send + 'static,
        Fut: Future<Output = SqliteResult<T>> + Send + 'static,
    {
        let result = self.transaction_raw(Box::new(move |tx| {
            Box::pin(async move {
                callback(tx)
                    .await
                    .map(|value| Box::new(value) as SqliteTransactionResult)
            })
        }));
        Box::pin(async move {
            result
                .await?
                .downcast::<T>()
                .map(|result| *result)
                .map_err(|_| SqliteError::new("SQLite transaction result type mismatch"))
        })
    }
}
impl<D: SqliteDatabase + ?Sized> SqliteDatabaseExt for D {}

pub fn validate_sqlite_schema_prefix(prefix: &str) -> SqliteResult<()> {
    let mut bytes = prefix.bytes();
    let valid = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let quoted = pi_json::quote(prefix);
    if !valid {
        return Err(SqliteError::new(format!(
            "Invalid SQLite schema prefix {quoted}"
        )));
    }
    if prefix.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(SqliteError::new(format!(
            "SQLite schema prefix {quoted} is reserved"
        )));
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct SqliteMigration {
    pub version: u32,
    pub statements: Vec<String>,
}
pub fn apply_sqlite_migrations(
    database: &dyn SqliteDatabase,
    migrations: &[SqliteMigration],
    table: &str,
) -> SqliteFuture<()> {
    let validated = validate_sqlite_schema_prefix(table).and_then(|_| {
        if migrations
            .iter()
            .enumerate()
            .all(|(i, m)| m.version as usize == i + 1)
        {
            Ok(())
        } else {
            Err(SqliteError::new(
                "SQLite migrations must have contiguous versions starting at 1",
            ))
        }
    });
    if let Err(error) = validated {
        return Box::pin(async { Err(error) });
    }
    let table = table.to_owned();
    let migrations = migrations.to_vec();
    database.transaction(move |tx|async move {
        tx.exec(&format!("CREATE TABLE IF NOT EXISTS {table} (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), version INTEGER NOT NULL CHECK (version >= 0)) STRICT")).await?;
        tx.run(&format!("INSERT OR IGNORE INTO {table} (singleton, version) VALUES (1, 0)"),&[]).await?;
        let row=tx.get(&format!("SELECT version FROM {table} WHERE singleton = 1"),&[]).await?.ok_or_else(||SqliteError::new("SQLite schema metadata is missing"))?;
        let version=match row.get("version") {Some(SqliteValue::Integer(n))=>*n,Some(SqliteValue::Real(n))=>*n as i64,_=>return Err(SqliteError::new("SQLite schema metadata is missing"))};
        let supported=migrations.last().map_or(0,|m|m.version);
        if version>supported as i64 {return Err(SqliteError::new(format!("SQLite schema version {version} is newer than supported version {supported}")));}
        for migration in migrations {
            if migration.version as i64<=version {continue;}
            for statement in migration.statements {tx.exec(&statement).await?;}
            tx.run(&format!("UPDATE {table} SET version = ? WHERE singleton = 1"),&[SqliteValue::Integer(migration.version as i64)]).await?;
        }
        Ok(())
    })
}
