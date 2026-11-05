use super::queue::{SerialQueue, panic_error};
use super::*;
use futures::FutureExt;
use parking_lot::Mutex;
use rusqlite::{
    Connection,
    types::{Value as BindValue, ValueRef},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
};

type Work = Box<dyn FnOnce(&mut Connection) + Send>;
enum Command {
    Work(Work),
    Close(tokio::sync::oneshot::Sender<SqliteResult<()>>),
}
#[derive(Clone)]
struct Worker {
    sender: mpsc::Sender<Command>,
    connection: Arc<Mutex<Option<Connection>>>,
    /// Commands sent and not yet run, changed only while the connection is locked.
    queued: Arc<AtomicUsize>,
    busy_timeout: std::time::Duration,
    /// Write-ahead (or in-memory) journaling: a transaction holding the write lock then never
    /// waits for another connection, so its statements and COMMIT may run in place. In rollback
    /// journal modes COMMIT waits for readers on the busy handler, so everything stays on the worker.
    in_place: bool,
}
impl Worker {
    /// Begin in place when nothing is queued ahead and the write lock is free. A contended begin
    /// waits on the busy handler, which must not block the caller's thread, so it returns false
    /// for the worker to begin instead.
    fn try_begin(&self) -> SqliteResult<bool> {
        if !self.in_place {
            return Ok(false);
        }
        let mut connection = self.connection.lock();
        if self.queued.load(Ordering::Acquire) != 0 {
            return Ok(false);
        }
        let connection = connection
            .as_mut()
            .ok_or_else(|| SqliteError::new("database is not open"))?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        let begun = connection.execute_batch("BEGIN IMMEDIATE");
        connection.busy_timeout(self.busy_timeout)?;
        match begun {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
    /// Run on the caller's thread. Only statements and COMMIT of an open transaction in
    /// `in_place` mode come here: it already holds the write lock, so nothing waits on the busy
    /// handler, and the queue barrier admits no other work. The caller is blocked only for the
    /// statement's own work (for COMMIT, the WAL append and at most a passive checkpoint). A round
    /// trip to the worker per statement dominates a small commit's latency.
    fn inline<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> SqliteResult<T>,
    ) -> SqliteResult<T> {
        let mut connection = self.connection.lock();
        let connection = connection
            .as_mut()
            .ok_or_else(|| SqliteError::new("database is not open"))?;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(connection)))
            .unwrap_or_else(|panic| Err(panic_error(panic)))
    }
    fn call<T, F>(&self, operation: F) -> SqliteFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> SqliteResult<T> + Send + 'static,
    {
        let (send, receive) = tokio::sync::oneshot::channel();
        let command = Command::Work(Box::new(move |connection| {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(connection)))
                    .unwrap_or_else(|panic| Err(panic_error(panic)));
            let _ = send.send(result);
        }));
        self.queued.fetch_add(1, Ordering::AcqRel);
        let sent = self.sender.send(command);
        if sent.is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
        }
        Box::pin(async move {
            sent.map_err(|_| SqliteError::new("database is not open"))?;
            receive
                .await
                .map_err(|_| SqliteError::new("SQLite worker ended"))?
        })
    }
    fn close(&self) -> SqliteFuture<()> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let sent = self.sender.send(Command::Close(send));
        Box::pin(async move {
            sent.map_err(|_| SqliteError::new("database is not open"))?;
            receive
                .await
                .map_err(|_| SqliteError::new("SQLite worker ended"))?
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSqliteDatabaseOptions {
    pub wal_auto_checkpoint_pages: u32,
    pub busy_timeout_ms: u64,
}
impl Default for NativeSqliteDatabaseOptions {
    fn default() -> Self {
        Self {
            wal_auto_checkpoint_pages: 1000,
            busy_timeout_ms: 5000,
        }
    }
}
#[derive(Clone)]
pub struct NativeSqliteDatabase {
    worker: Worker,
    queue: Arc<SerialQueue>,
    closed: Arc<AtomicBool>,
}

pub async fn open_native_sqlite_database(
    path: impl AsRef<Path>,
    options: NativeSqliteDatabaseOptions,
) -> SqliteResult<NativeSqliteDatabase> {
    let path = path.as_ref().to_owned();
    let (sender, receiver) = mpsc::channel();
    let (opened, ready) = tokio::sync::oneshot::channel();
    let shared = Arc::new(Mutex::new(None::<Connection>));
    let connection_slot = shared.clone();
    let queued = Arc::new(AtomicUsize::new(0));
    let worker_queued = queued.clone();
    std::thread::Builder::new()
        .name("pi-sqlite".into())
        .spawn(move || {
            let connection = (|| -> SqliteResult<Connection> {
                if path != Path::new(":memory:")
                    && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
                {
                    std::fs::create_dir_all(parent).map_err(|e| SqliteError::new(e.to_string()))?;
                }
                let connection = Connection::open(path)?;
                // Match the reference's connection-wide Map, including statements
                // reused after more than rusqlite's default 16 distinct queries.
                connection.set_prepared_statement_cache_capacity(usize::MAX);
                connection
                    .busy_timeout(std::time::Duration::from_millis(options.busy_timeout_ms))?;
                connection.pragma_update(None, "journal_mode", "WAL")?;
                connection.pragma_update(None, "synchronous", "NORMAL")?;
                connection.pragma_update(
                    None,
                    "wal_autocheckpoint",
                    options.wal_auto_checkpoint_pages,
                )?;
                Ok(connection)
            })();
            let connection = connection.and_then(|connection| {
                let mode: String =
                    connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
                Ok((connection, matches!(mode.as_str(), "wal" | "memory")))
            });
            match connection {
                Ok((connection, in_place)) => {
                    *connection_slot.lock() = Some(connection);
                    let _ = opened.send(Ok(in_place));
                }
                Err(error) => {
                    let _ = opened.send(Err(error));
                    return;
                }
            }
            while let Ok(command) = receiver.recv() {
                match command {
                    Command::Work(work) => {
                        let mut connection = connection_slot.lock();
                        if let Some(connection) = connection.as_mut() {
                            work(connection);
                        }
                        worker_queued.fetch_sub(1, Ordering::AcqRel);
                    }
                    Command::Close(done) => {
                        let Some(connection) = connection_slot.lock().take() else {
                            let _ = done.send(Ok(()));
                            return;
                        };
                        connection.flush_prepared_statement_cache();
                        let checkpoint = connection
                            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                            .map_err(SqliteError::from);
                        let close = connection
                            .close()
                            .map_err(|(_, error)| SqliteError::from(error));
                        let _ = done.send(checkpoint.and(close));
                        return;
                    }
                }
            }
        })
        .map_err(|e| SqliteError::new(e.to_string()))?;
    let in_place = ready
        .await
        .map_err(|_| SqliteError::new("SQLite worker ended while opening"))??;
    Ok(NativeSqliteDatabase {
        worker: Worker {
            sender,
            connection: shared,
            queued,
            busy_timeout: std::time::Duration::from_millis(options.busy_timeout_ms),
            in_place,
        },
        queue: Arc::default(),
        closed: Arc::new(AtomicBool::new(false)),
    })
}

fn bind(params: Vec<SqliteValue>) -> Vec<BindValue> {
    params
        .into_iter()
        .map(|value| match value {
            SqliteValue::Null => BindValue::Null,
            SqliteValue::Real(v) => BindValue::Real(v),
            SqliteValue::Integer(v) => BindValue::Integer(v),
            SqliteValue::Text(v) => BindValue::Text(v),
            SqliteValue::Blob(v) => BindValue::Blob(v),
        })
        .collect()
}
fn rows(
    connection: &mut Connection,
    sql: &str,
    params: Vec<SqliteValue>,
    first_only: bool,
) -> SqliteResult<Vec<SqliteRow>> {
    let mut statement = connection.prepare_cached(sql)?;
    let names: Vec<_> = statement.column_names().map(str::to_owned).collect();
    let params = bind(params);
    let mut cursor = statement.query(rusqlite::params_from_iter(params))?;
    let mut result = Vec::new();
    while let Some(row) = cursor.next()? {
        let mut value = SqliteRow::new();
        for (index, name) in names.iter().enumerate() {
            value.insert(
                name.clone(),
                match row.get_ref(index)? {
                    ValueRef::Null => SqliteValue::Null,
                    ValueRef::Integer(v) => SqliteValue::Integer(v),
                    ValueRef::Real(v) => SqliteValue::Real(v),
                    ValueRef::Text(v) => SqliteValue::Text(String::from_utf8_lossy(v).into_owned()),
                    ValueRef::Blob(v) => SqliteValue::Blob(v.to_vec()),
                },
            );
        }
        result.push(value);
        if first_only {
            break;
        }
    }
    Ok(result)
}
#[derive(Clone)]
struct Executor {
    worker: Worker,
    active: Option<Arc<Mutex<bool>>>,
}
impl Executor {
    fn call<T, F>(&self, operation: F) -> SqliteFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> SqliteResult<T> + Send + 'static,
    {
        // The lease check and worker submission form one operation. Commands
        // accepted before retirement must run before COMMIT, even if their
        // waiters are dropped. A later call must never join another transaction.
        let active = self.active.as_ref().map(|active| active.lock());
        if active.as_ref().is_some_and(|active| !**active) {
            return Box::pin(async {
                Err(SqliteError::new(
                    "SQLite transaction handle is no longer active",
                ))
            });
        }
        if active.is_some() && self.worker.in_place {
            let result = self.worker.inline(operation);
            return Box::pin(async move { result });
        }
        self.worker.call(operation)
    }
}
impl SqliteExecutor for Executor {
    fn exec(&self, sql: &str) -> SqliteFuture<()> {
        let sql = sql.to_owned();
        self.call(move |c| c.execute_batch(&sql).map_err(Into::into))
    }
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.call(move |c| {
            c.prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(bind(params)))?;
            Ok(())
        })
    }
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.call(move |c| Ok(rows(c, &sql, params, true)?.into_iter().next()))
    }
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.call(move |c| rows(c, &sql, params, false))
    }
}
impl NativeSqliteDatabase {
    fn executor(&self) -> Executor {
        Executor {
            worker: self.worker.clone(),
            active: None,
        }
    }
}
impl SqliteExecutor for NativeSqliteDatabase {
    fn exec(&self, sql: &str) -> SqliteFuture<()> {
        let sql = sql.to_owned();
        let executor = self.executor();
        self.queue.submit(move || executor.exec(&sql))
    }
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        let executor = self.executor();
        self.queue.submit(move || executor.run(&sql, &params))
    }
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        let executor = self.executor();
        self.queue.submit(move || executor.get(&sql, &params))
    }
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        let executor = self.executor();
        self.queue.submit(move || executor.all(&sql, &params))
    }
}
struct Cleanup {
    worker: Worker,
    needs_rollback: Arc<AtomicBool>,
    active: Arc<Mutex<bool>>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        *self.active.lock() = false;
        let needs_rollback = self.needs_rollback.clone();
        // This work is submitted even if BEGIN's waiter was dropped. The worker
        // checks the flag after BEGIN, so an interrupted begin cannot leak a transaction.
        drop(self.worker.call(move |connection| {
            if needs_rollback.swap(false, Ordering::AcqRel) {
                connection.execute_batch("ROLLBACK")?;
            }
            Ok(())
        }));
    }
}
impl SqliteDatabase for NativeSqliteDatabase {
    fn transaction_raw(
        &self,
        callback: SqliteTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult> {
        let worker = self.worker.clone();
        self.queue.submit(move || {
            Box::pin(async move {
                let active = Arc::new(Mutex::new(true));
                let needs_rollback = Arc::new(AtomicBool::new(false));
                let cleanup = Cleanup {
                    worker: worker.clone(),
                    needs_rollback: needs_rollback.clone(),
                    active: active.clone(),
                };
                if worker.try_begin()? {
                    needs_rollback.store(true, Ordering::Release);
                } else {
                    worker
                        .call({
                            let needs_rollback = needs_rollback.clone();
                            move |c| {
                                c.execute_batch("BEGIN IMMEDIATE")?;
                                needs_rollback.store(true, Ordering::Release);
                                Ok(())
                            }
                        })
                        .await?;
                }
                let executor = Arc::new(Executor {
                    worker: worker.clone(),
                    active: Some(active.clone()),
                });
                let result = std::panic::AssertUnwindSafe(async move { callback(executor).await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|panic| Err(panic_error(panic)));
                *active.lock() = false;
                let result = match result {
                    Ok(value) => {
                        let commit =
                            |c: &mut Connection| c.execute_batch("COMMIT").map_err(Into::into);
                        let committed = if worker.in_place {
                            worker.inline(commit)
                        } else {
                            worker.call(commit).await
                        };
                        match committed {
                            Ok(()) => Ok(value),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
                let result = if let Err(error) = result {
                    match worker
                        .call(|c| c.execute_batch("ROLLBACK").map_err(Into::into))
                        .await
                    {
                        Ok(()) => Err(error),
                        Err(rollback) => Err(SqliteError::rollback(error, rollback)),
                    }
                } else {
                    result
                };
                needs_rollback.store(false, Ordering::Release);
                drop(cleanup);
                result
            })
        })
    }
    fn close(&self) -> SqliteFuture<()> {
        let closed = self.closed.clone();
        let worker = self.worker.clone();
        self.queue.submit(move || {
            Box::pin(async move {
                if closed.swap(true, Ordering::AcqRel) {
                    return Ok(());
                }
                worker.close().await
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retains_prepared_statements_across_more_than_sixteen_distinct_queries() {
        let database = open_native_sqlite_database(":memory:", Default::default())
            .await
            .unwrap();
        for pass in 0..2 {
            for index in 0..40 {
                database
                    .get(&format!("SELECT {index} AS value"), &[])
                    .await
                    .unwrap();
            }
            let statements = database
                .worker
                .call(|connection| {
                    // The worker owns this connection and no statement is executing.
                    // SQLite exposes its live prepared statements for this check.
                    let mut statements = Vec::new();
                    unsafe {
                        let handle = connection.handle();
                        let mut statement =
                            rusqlite::ffi::sqlite3_next_stmt(handle, std::ptr::null_mut());
                        while !statement.is_null() {
                            let sql =
                                std::ffi::CStr::from_ptr(rusqlite::ffi::sqlite3_sql(statement));
                            statements.push(sql.to_string_lossy().into_owned());
                            statement = rusqlite::ffi::sqlite3_next_stmt(handle, statement);
                        }
                    }
                    Ok(statements)
                })
                .await
                .unwrap();
            for index in 0..40 {
                assert!(
                    statements.contains(&format!("SELECT {index} AS value")),
                    "missing cached statement {index} after pass {pass}"
                );
            }
        }
        database.close().await.unwrap();
    }
}
