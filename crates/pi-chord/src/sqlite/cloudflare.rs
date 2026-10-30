//! Structural host contract for SQLite-backed Durable Objects. The host owns its
//! storage and transaction implementation; all adapters of one storage share a queue.

use super::queue::{SerialQueue, panic_error};
use super::*;
use futures::FutureExt;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum DurableObjectSqlValue {
    Null,
    Number(f64),
    Text(String),
    Blob(Vec<u8>),
}
pub type DurableObjectSqlRow = IndexMap<String, DurableObjectSqlValue>;
pub trait DurableObjectSqlCursor: Send {
    fn next(&mut self) -> SqliteResult<Option<DurableObjectSqlRow>>;
    fn into_array(self: Box<Self>) -> SqliteResult<Vec<DurableObjectSqlRow>>;
}
pub type DurableObjectTransactionCallback =
    Box<dyn FnOnce() -> SqliteFuture<SqliteTransactionResult> + Send>;
pub trait DurableObjectSqliteHost: Send + Sync {
    fn exec(
        &self,
        sql: &str,
        bindings: &[DurableObjectSqlValue],
    ) -> SqliteResult<Box<dyn DurableObjectSqlCursor>>;
    /// Commit after callback success; roll back before returning its error. Hosts
    /// must distinguish a rollback failure from the original callback error.
    fn transaction_raw(
        &self,
        callback: DurableObjectTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult>;
}
pub struct DurableObjectSqliteStorage {
    host: Arc<dyn DurableObjectSqliteHost>,
    queue: Arc<SerialQueue>,
}
impl DurableObjectSqliteStorage {
    pub fn new(host: Arc<dyn DurableObjectSqliteHost>) -> Self {
        Self {
            host,
            queue: Arc::default(),
        }
    }
    /// Direct underlying calls intentionally bypass the adapter queue, just as
    /// `ctx.storage.sql.exec` does. They participate in any active host transaction.
    pub fn exec(
        &self,
        sql: &str,
        bindings: &[DurableObjectSqlValue],
    ) -> SqliteResult<Box<dyn DurableObjectSqlCursor>> {
        self.host.exec(sql, bindings)
    }
}
#[derive(Clone)]
pub struct DurableObjectSqliteDatabase {
    storage: Arc<DurableObjectSqliteStorage>,
}
impl DurableObjectSqliteDatabase {
    pub fn new(storage: Arc<DurableObjectSqliteStorage>) -> Self {
        Self { storage }
    }
}

fn bind(params: &[SqliteValue]) -> SqliteResult<Vec<DurableObjectSqlValue>> {
    params
        .iter()
        .map(|value| {
            Ok(match value {
                SqliteValue::Null => DurableObjectSqlValue::Null,
                SqliteValue::Real(n) => DurableObjectSqlValue::Number(*n),
                SqliteValue::Integer(n) => {
                    if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(n) {
                        return Err(SqliteError::new(format!(
                            "Durable Object SQL cannot bind {n} without losing precision"
                        )));
                    }
                    DurableObjectSqlValue::Number(*n as f64)
                }
                SqliteValue::Text(s) => DurableObjectSqlValue::Text(s.clone()),
                SqliteValue::Blob(bytes) => DurableObjectSqlValue::Blob(bytes.clone()),
            })
        })
        .collect()
}
fn row(row: DurableObjectSqlRow) -> SqliteRow {
    row.into_iter()
        .map(|(key, value)| {
            (
                key,
                match value {
                    DurableObjectSqlValue::Null => SqliteValue::Null,
                    DurableObjectSqlValue::Number(n) => SqliteValue::Real(n),
                    DurableObjectSqlValue::Text(s) => SqliteValue::Text(s),
                    DurableObjectSqlValue::Blob(bytes) => SqliteValue::Blob(bytes),
                },
            )
        })
        .collect()
}
#[derive(Clone)]
struct Executor {
    storage: Arc<DurableObjectSqliteStorage>,
    active: Option<Arc<Mutex<bool>>>,
}
impl Executor {
    fn query<T>(
        &self,
        sql: &str,
        params: &[SqliteValue],
        consume: impl FnOnce(Box<dyn DurableObjectSqlCursor>) -> SqliteResult<T>,
    ) -> SqliteResult<T> {
        let active = self.active.as_ref().map(|active| active.lock());
        if active.as_ref().is_some_and(|active| !**active) {
            return Err(SqliteError::new(
                "SQLite transaction handle is no longer active",
            ));
        }
        consume(self.storage.exec(sql, &bind(params)?)?)
    }
    fn result<T: Send + 'static>(&self, result: SqliteResult<T>) -> SqliteFuture<T> {
        Box::pin(async { result })
    }
}
impl SqliteExecutor for Executor {
    fn exec(&self, sql: &str) -> SqliteFuture<()> {
        self.result(self.query(sql, &[], |_| Ok(())))
    }
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()> {
        self.result(self.query(sql, params, |_| Ok(())))
    }
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>> {
        self.result(
            self.query(sql, params, |mut cursor| cursor.next())
                .map(|value| value.map(row)),
        )
    }
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>> {
        self.result(
            self.query(sql, params, |cursor| cursor.into_array())
                .map(|rows| rows.into_iter().map(row).collect()),
        )
    }
}
impl SqliteExecutor for DurableObjectSqliteDatabase {
    fn exec(&self, sql: &str) -> SqliteFuture<()> {
        let storage = self.storage.clone();
        let sql = sql.to_owned();
        self.storage
            .queue
            .submit_sync(move || storage.exec(&sql, &[]).map(|_| ()))
    }
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()> {
        let storage = self.storage.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.storage
            .queue
            .submit_sync(move || storage.exec(&sql, &bind(&params)?).map(|_| ()))
    }
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>> {
        let storage = self.storage.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.storage.queue.submit_sync(move || {
            storage
                .exec(&sql, &bind(&params)?)
                .and_then(|mut cursor| cursor.next())
                .map(|value| value.map(row))
        })
    }
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>> {
        let storage = self.storage.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.storage.queue.submit_sync(move || {
            storage
                .exec(&sql, &bind(&params)?)
                .and_then(|cursor| cursor.into_array())
                .map(|rows| rows.into_iter().map(row).collect())
        })
    }
}
struct Active(Arc<Mutex<bool>>);
impl Drop for Active {
    fn drop(&mut self) {
        *self.0.lock() = false;
    }
}
impl SqliteDatabase for DurableObjectSqliteDatabase {
    fn transaction_raw(
        &self,
        callback: SqliteTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult> {
        let storage = self.storage.clone();
        self.storage.queue.submit(move || {
            let host = storage.host.clone();
            host.transaction_raw(Box::new(move || {
                Box::pin(async move {
                    let active = Arc::new(Mutex::new(true));
                    let guard = Active(active.clone());
                    let executor = Arc::new(Executor {
                        storage,
                        active: Some(active),
                    });
                    let result =
                        std::panic::AssertUnwindSafe(async move { callback(executor).await })
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|panic| Err(panic_error(panic)));
                    drop(guard);
                    result
                })
            }))
        })
    }
    fn close(&self) -> SqliteFuture<()> {
        self.storage.queue.submit_sync(|| Ok(()))
    }
}
