//! Durable storage backends whose driver objects are JS values (`storage.*`).
//!
//! The native `pi_durable` SQLite and JSONL storages run here over the caller's own objects:
//! a JS `SqliteDatabase` (an upstream driver, a `NodeSqliteDatabase`, a test wrapper with
//! injected settlement) is reached through [`JsSqliteDatabase`], and a JS `FileSystem` (a
//! `NodeExecutionEnv` or a fault-injecting wrapper) through [`JsFileSystem`]. This is the
//! in-process form of `pi-durable/examples/{sqlite,jsonl}_stock_bridge.rs`: storage requests go
//! through the same `pi_durable::testing::storage_request` runner, and only the transport differs
//! (direct handles instead of JSON frames).
//!
//! Plain `MemoryStorage` and `openNodeSqliteStorage` stores live in the in-process stock service
//! instead (`memory.*`, `sqlite.*`, see `transport.rs`), so a native Harness can share them.
//!
//! Methods (synchronous: each starts its work in call order and returns a promise):
//! - `storage.sqliteConfig()` → `{ version, migrations: [{ version, statements }] }`;
//! - `storage.sqliteOpen(database, transact, prefix?)` → store: `SqliteStorage.open`;
//! - `storage.sqliteMigrate(database, transact, migrations?)`: `applySqliteMigrations`;
//! - `storage.sqliteTransaction(state, executor)`: run a native transaction body over the
//!   executor the JS driver passed to its `transaction()` callback;
//! - `storage.jsonlOpen(directory, fs, context, cwd, fsId, fsync)` → store: `JsonlStorage.open`
//!   (every filesystem call gets `context` last);
//! - `storage.call(store, method, args, adopt?)`: one `Storage` method (`adopt(seq)` runs on
//!   the JS thread before the commit settles; a throw rejects the commit with that value).
//!
//! `transact(database, state)` is the glue function that calls `database.transaction(executor =>
//! call("storage.sqliteTransaction", state, ref(executor)))`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use pi_chord::sqlite::{
    SqliteDatabase, SqliteError, SqliteExecutor, SqliteFuture, SqliteResult, SqliteRow,
    SqliteTransactionCallback, SqliteTransactionResult, SqliteValue,
};
use pi_durable::env::{FileError, FileErrorCode, FileFuture, FileInfo, FileKind, FileSystem};
use pi_durable::storage::Storage;
use pi_durable::storage::jsonl::{JsonlStorage, JsonlStorageOptions};
use pi_durable::storage::sqlite::{
    CURRENT_SQLITE_SCHEMA_VERSION, SqliteMigration, SqliteStorage, SqliteStorageOptions,
    apply_sqlite_migrations, create_sqlite_migrations, create_sqlite_schema,
};
use pi_durable::{Context, ErrorKind};
use pi_json::strict::{self, JsonString};
use pi_jsrt::JsHandle;

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Dispatch, Methods, call_async, call_sync};
use crate::value::HostValue;

/// The `storage.*` method group.
pub struct StorageDispatch;

/// A native store driven from JS.
pub struct Store(pub Arc<dyn Storage>);

/// Convert a native durable error to the upstream JS class.
pub fn host_error(error: &pi_durable::Error) -> HostError {
    let message = error.message_text();
    match error.kind() {
        ErrorKind::StorageRequest(_) => HostError::typed(ErrorClass::StorageRequestError, message),
        ErrorKind::ReadAfterWrite(method) => HostError::typed(ErrorClass::ReadAfterWrite, message)
            .with_prop("method", method.as_str()),
        ErrorKind::ReadOnly(what) => {
            HostError::typed(ErrorClass::ReadOnlySession, message).with_prop("what", what.as_str())
        }
        ErrorKind::ConversationBusy(id) => HostError::typed(ErrorClass::ConversationBusy, message)
            .with_prop("conversationId", id.get() as f64),
        ErrorKind::SessionFailed { cause } => {
            HostError::typed(ErrorClass::SessionFailed, message).with_cause(host_error(cause))
        }
        ErrorKind::JsonlCorruption { cause, .. } => {
            let error = HostError::typed(ErrorClass::JsonlCorruptionError, message);
            match cause {
                Some(cause) => error.with_cause(host_error(cause)),
                None => error,
            }
        }
        ErrorKind::JsonlPoisoned { cause } => {
            HostError::typed(ErrorClass::JsonlStoragePoisonedError, message)
                .with_cause(host_error(cause))
        }
        ErrorKind::File(file) => file_host_error(file),
        ErrorKind::Aborted(abort) => HostError::new(ErrorClass::AbortError, abort.message.clone()),
        ErrorKind::Caused { cause, .. } | ErrorKind::DocumentCopy { cause, .. } => {
            HostError::typed(ErrorClass::Error, message).with_cause(host_error(cause))
        }
        _ => {
            let name = error.name();
            let host = HostError::typed(ErrorClass::Error, message);
            if name == "Error" {
                host
            } else {
                host.with_prop("name", name)
            }
        }
    }
}

fn file_code_name(code: FileErrorCode) -> &'static str {
    match code {
        FileErrorCode::Aborted => "aborted",
        FileErrorCode::NotFound => "not_found",
        FileErrorCode::PermissionDenied => "permission_denied",
        FileErrorCode::NotDirectory => "not_directory",
        FileErrorCode::IsDirectory => "is_directory",
        FileErrorCode::Invalid => "invalid",
        FileErrorCode::NotSupported => "not_supported",
        FileErrorCode::Unknown => "unknown",
    }
}

fn file_code(name: &str) -> FileErrorCode {
    match name {
        "aborted" => FileErrorCode::Aborted,
        "not_found" => FileErrorCode::NotFound,
        "permission_denied" => FileErrorCode::PermissionDenied,
        "not_directory" => FileErrorCode::NotDirectory,
        "is_directory" => FileErrorCode::IsDirectory,
        "invalid" => FileErrorCode::Invalid,
        "not_supported" => FileErrorCode::NotSupported,
        _ => FileErrorCode::Unknown,
    }
}

/// A native `FileError` as the upstream `FileError` class.
pub fn file_host_error(error: &FileError) -> HostError {
    let host = HostError::new(ErrorClass::FileError, error.to_string())
        .with_prop("code", file_code_name(error.code()));
    match error.path() {
        Some(path) => host.with_prop("path", path_value(path)),
        None => host,
    }
}

fn strict_args(args: &HostValue) -> HostResult<Vec<strict::Value>> {
    match args {
        HostValue::Array(items) => items
            .iter()
            .map(|item| match item {
                HostValue::Undefined => Ok(strict::Value::Null),
                other => other.to_strict_omitting_undefined(),
            })
            .collect(),
        _ => Err(HostError::type_error("storage arguments must be an array")),
    }
}

fn from_strict_option(value: Option<strict::Value>) -> HostValue {
    value.map_or(HostValue::Undefined, |value| HostValue::from_strict(&value))
}

// ---------------------------------------------------------------------------
// Starting work in call order
// ---------------------------------------------------------------------------

/// Call `target[method](...args)` (or `target(...args)`) and await its settled result. On the JS
/// thread inside a host call, the JS function runs *now* (so a driver sees calls in the order
/// the native code makes them, as upstream's drivers do); elsewhere it is queued like
/// [`call_async`].
pub fn call_now(
    target: &JsHandle,
    method: Option<&'static str>,
    args: Vec<HostValue>,
) -> futures::future::BoxFuture<'static, HostResult<HostValue>> {
    use boa_engine::{JsString, JsValue};
    use futures::FutureExt;
    if target.runtime().is_js_thread() {
        let (tx, rx) = futures::channel::oneshot::channel();
        let started = pi_jsrt::ctx::with_current(|ctx| {
            let invoked = (|| -> boa_engine::JsResult<JsValue> {
                let value = pi_jsrt::ctx::handle_value(ctx, target).ok_or_else(|| {
                    boa_engine::JsNativeError::typ().with_message("invalid JS handle")
                })?;
                let mut js_args = Vec::with_capacity(args.len());
                for a in &args {
                    js_args.push(crate::value::to_js(ctx, a)?);
                }
                let (this, f) = match method {
                    None => (JsValue::undefined(), value.clone()),
                    Some(name) => {
                        let obj = value.to_object(ctx)?;
                        (value.clone(), obj.get(JsString::from(name), ctx)?)
                    }
                };
                let f = f.as_callable().ok_or_else(|| {
                    boa_engine::JsNativeError::typ().with_message("not a function")
                })?;
                f.call(&this, &js_args, ctx)
            })();
            pi_jsrt::ctx::when_settled(ctx, invoked, move |ctx, r| {
                let _ = tx.send(match r {
                    Ok(v) => crate::value::from_js(ctx, &v),
                    Err(e) => Err(HostError::from_js(ctx, &e)),
                });
            });
        });
        if started.is_some() {
            return async move {
                rx.await.unwrap_or_else(|_| {
                    Err(HostError::new(ErrorClass::Error, "the runtime stopped"))
                })
            }
            .boxed();
        }
    }
    let target = target.clone();
    async move { call_async(&target, method, args).await }.boxed()
}

/// Poll `future` once now (on the calling thread), so the work it starts begins in call order;
/// the rest runs where the returned future is awaited.
pub fn start_now<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> futures::future::BoxFuture<'static, T> {
    use futures::FutureExt;
    let mut future = future.boxed();
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => async move { value }.boxed(),
        std::task::Poll::Pending => future,
    }
}

/// From a synchronous method (inside its host call): start `future` now and return a promise
/// of its result. Unlike an asynchronous method, work the future starts on its first poll (JS
/// driver calls included) happens before the calling JS code continues.
pub fn promise_now(
    future: impl std::future::Future<Output = HostResult<HostValue>> + Send + 'static,
) -> HostResult<HostValue> {
    let future = start_now(future);
    pi_jsrt::ctx::with_current(|ctx| {
        let promise = pi_jsrt::ctx::spawn_promise(ctx, async move {
            let result = future.await;
            Box::new(move |ctx: &mut boa_engine::Context| match result {
                Ok(value) => crate::value::to_js(ctx, &value),
                Err(error) => Err(error.to_boa(ctx)),
            }) as pi_jsrt::ctx::Settle
        });
        pi_jsrt::ctx::make_handle(ctx, promise).map(HostValue::Handle)
    })
    .flatten()
    .ok_or_else(|| HostError::new(ErrorClass::Error, "the runtime is shutting down"))
}

// ---------------------------------------------------------------------------
// SQLite drivers
// ---------------------------------------------------------------------------

fn sqlite_error(error: HostError) -> SqliteError {
    SqliteError::new(error.message().to_string_lossy())
}

fn sqlite_param(value: &SqliteValue) -> HostValue {
    match value {
        SqliteValue::Null => HostValue::Null,
        SqliteValue::Integer(n) => HostValue::Number(*n as f64),
        SqliteValue::Real(n) => HostValue::Number(*n),
        SqliteValue::Text(s) => HostValue::string(s),
        SqliteValue::Blob(b) => HostValue::Bytes(b.clone()),
    }
}

fn sqlite_row(value: HostValue) -> SqliteResult<SqliteRow> {
    let HostValue::Object(entries) = value else {
        return Err(SqliteError::new("Invalid host SQLite row"));
    };
    let mut row = SqliteRow::new();
    for (key, value) in entries {
        let value = match value {
            HostValue::Null | HostValue::Undefined => SqliteValue::Null,
            HostValue::Number(n) => {
                if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 {
                    SqliteValue::Integer(n as i64)
                } else {
                    SqliteValue::Real(n)
                }
            }
            HostValue::String(s) => SqliteValue::Text(s.to_string_lossy()),
            HostValue::Bytes(b) => SqliteValue::Blob(b),
            _ => return Err(SqliteError::new("Unsupported host SQLite value")),
        };
        row.insert(key.to_string_lossy(), value);
    }
    Ok(row)
}

/// A JS `SqliteDatabase` (or the executor its `transaction()` passes to a callback).
#[derive(Clone)]
pub struct JsSqliteDatabase {
    target: JsHandle,
    /// The glue's `transact(database, state)`; `None` for transaction executors.
    transact: Option<JsHandle>,
}

impl JsSqliteDatabase {
    /// A database whose transactions run through `transact(database, state)`.
    pub fn new(target: JsHandle, transact: Option<JsHandle>) -> Self {
        JsSqliteDatabase { target, transact }
    }

    fn call(
        &self,
        method: &'static str,
        sql: &str,
        params: &[SqliteValue],
    ) -> SqliteFuture<HostValue> {
        let target = self.target.clone();
        let mut args = vec![HostValue::string(sql)];
        args.extend(params.iter().map(sqlite_param));
        let call = call_now(&target, Some(method), args);
        Box::pin(async move { call.await.map_err(sqlite_error) })
    }
}

impl SqliteExecutor for JsSqliteDatabase {
    fn exec(&self, sql: &str) -> SqliteFuture<()> {
        let call = self.call("exec", sql, &[]);
        Box::pin(async move { call.await.map(|_| ()) })
    }
    fn run(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<()> {
        let call = self.call("run", sql, params);
        Box::pin(async move { call.await.map(|_| ()) })
    }
    fn get(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Option<SqliteRow>> {
        let call = self.call("get", sql, params);
        Box::pin(async move {
            match call.await? {
                HostValue::Undefined | HostValue::Null => Ok(None),
                row => sqlite_row(row).map(Some),
            }
        })
    }
    fn all(&self, sql: &str, params: &[SqliteValue]) -> SqliteFuture<Vec<SqliteRow>> {
        let call = self.call("all", sql, params);
        Box::pin(async move {
            match call.await? {
                HostValue::Array(rows) => rows.into_iter().map(sqlite_row).collect(),
                _ => Err(SqliteError::new("Invalid host SQLite rows")),
            }
        })
    }
}

/// One native transaction body waiting for the JS driver's executor.
pub struct SqliteTransactionState {
    callback: Mutex<Option<SqliteTransactionCallback>>,
    result: Mutex<Option<SqliteResult<SqliteTransactionResult>>>,
}

impl SqliteDatabase for JsSqliteDatabase {
    fn transaction_raw(
        &self,
        callback: SqliteTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult> {
        let Some(transact) = self.transact.clone() else {
            return Box::pin(async {
                Err(SqliteError::new(
                    "A SQLite executor cannot open a transaction",
                ))
            });
        };
        let state = Arc::new(SqliteTransactionState {
            callback: Mutex::new(Some(callback)),
            result: Mutex::new(None),
        });
        // Enter the driver's `transaction()` now: the storage calls it in operation order.
        let settled = call_now(
            &transact,
            None,
            vec![
                HostValue::Handle(self.target.clone()),
                HostValue::Native(crate::NativeRef(state.clone())),
            ],
        );
        Box::pin(async move {
            let settled = settled.await;
            let result = state.result.lock().take();
            match (result, settled) {
                (Some(Err(error)), _) => Err(error),
                (_, Err(error)) => Err(sqlite_error(error)),
                (Some(Ok(value)), Ok(_)) => Ok(value),
                (None, Ok(_)) => Err(SqliteError::new("SQLite host skipped transaction callback")),
            }
        })
    }
    fn close(&self) -> SqliteFuture<()> {
        let call = call_now(&self.target, Some("close"), Vec::new());
        Box::pin(async move { call.await.map(|_| ()).map_err(sqlite_error) })
    }
}

fn migrations(value: &HostValue) -> HostResult<Option<Vec<SqliteMigration>>> {
    let HostValue::Array(items) = value else {
        return Ok(None);
    };
    items
        .iter()
        .map(|item| {
            let version = item
                .get("version")
                .and_then(HostValue::as_f64)
                .ok_or_else(|| HostError::type_error("a migration needs a numeric version"))?;
            let statements = match item.get("statements") {
                Some(HostValue::Array(statements)) => statements
                    .iter()
                    .map(|s| {
                        s.as_string()
                            .map(JsonString::to_string_lossy)
                            .ok_or_else(|| {
                                HostError::type_error("migration statements must be strings")
                            })
                    })
                    .collect::<HostResult<Vec<_>>>()?,
                _ => return Err(HostError::type_error("a migration needs statements")),
            };
            Ok(SqliteMigration {
                version: version as u32,
                statements,
            })
        })
        .collect::<HostResult<Vec<_>>>()
        .map(Some)
}

// ---------------------------------------------------------------------------
// Filesystems
// ---------------------------------------------------------------------------

/// A JS `FileSystem` (its methods return `Result` objects: `{ ok, value }` / `{ ok, error }`).
pub struct JsFileSystem {
    target: JsHandle,
    /// The JS context every call passes last (the one `JsonlStorage.open` got).
    context: HostValue,
    cwd: PathBuf,
    id: String,
}

impl JsFileSystem {
    /// The filesystem `target`, whose `cwd` and `id` were read when it was adopted.
    pub fn new(target: JsHandle, context: HostValue, cwd: PathBuf, id: String) -> Self {
        JsFileSystem {
            target,
            context,
            cwd,
            id,
        }
    }

    fn call(
        &self,
        method: &'static str,
        mut args: Vec<HostValue>,
        _context: &Context,
    ) -> FileFuture<HostValue> {
        args.push(self.context.clone());
        let call = call_now(&self.target, Some(method), args);
        Box::pin(async move {
            let result = call.await.map_err(|e| {
                FileError::new(FileErrorCode::Unknown, e.message().to_string_lossy(), None)
            })?;
            if result.get("ok").and_then(HostValue::as_bool) == Some(true) {
                return Ok(result.get("value").cloned().unwrap_or_default());
            }
            let error = result.get("error").cloned().unwrap_or_default();
            let text = |key: &str| {
                error
                    .get(key)
                    .and_then(HostValue::as_string)
                    .map(JsonString::to_string_lossy)
            };
            Err(FileError::new(
                file_code(text("code").as_deref().unwrap_or("unknown")),
                text("message").unwrap_or_else(|| "Filesystem bridge failure".into()),
                text("path").map(PathBuf::from),
            ))
        })
    }

    fn unit(
        &self,
        method: &'static str,
        args: Vec<HostValue>,
        context: &Context,
    ) -> FileFuture<()> {
        let call = self.call(method, args, context);
        Box::pin(async move { call.await.map(|_| ()) })
    }

    fn path(
        &self,
        method: &'static str,
        args: Vec<HostValue>,
        context: &Context,
    ) -> FileFuture<PathBuf> {
        let call = self.call(method, args, context);
        Box::pin(async move {
            match call.await? {
                HostValue::String(s) => Ok(PathBuf::from(s.to_string_lossy())),
                _ => Err(FileError::new(
                    FileErrorCode::Invalid,
                    "Expected a path",
                    None,
                )),
            }
        })
    }
}

fn path_value(path: &Path) -> HostValue {
    HostValue::string(&path.to_string_lossy())
}

impl FileSystem for JsFileSystem {
    fn id(&self) -> &str {
        &self.id
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
    fn absolute_path(&self, path: &Path, context: &Context) -> FileFuture<PathBuf> {
        self.path("absolutePath", vec![path_value(path)], context)
    }
    fn join_path(&self, parts: &[PathBuf], context: &Context) -> FileFuture<PathBuf> {
        let parts = parts.iter().map(|p| path_value(p)).collect();
        self.path("joinPath", vec![HostValue::Array(parts)], context)
    }
    fn read_binary_file(&self, path: &Path, context: &Context) -> FileFuture<Vec<u8>> {
        let call = self.call("readBinaryFile", vec![path_value(path)], context);
        Box::pin(async move {
            match call.await? {
                HostValue::Bytes(bytes) => Ok(bytes),
                _ => Err(FileError::new(
                    FileErrorCode::Invalid,
                    "Expected bytes",
                    None,
                )),
            }
        })
    }
    fn write_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()> {
        self.unit(
            "writeFile",
            vec![path_value(path), HostValue::Bytes(content.to_vec())],
            context,
        )
    }
    fn append_file(&self, path: &Path, content: &[u8], context: &Context) -> FileFuture<()> {
        self.unit(
            "appendFile",
            vec![path_value(path), HostValue::Bytes(content.to_vec())],
            context,
        )
    }
    fn truncate_file(&self, path: &Path, size: u64, context: &Context) -> FileFuture<()> {
        self.unit(
            "truncateFile",
            vec![path_value(path), HostValue::Number(size as f64)],
            context,
        )
    }
    fn flush_file(&self, path: &Path, context: &Context) -> FileFuture<()> {
        self.unit("flushFile", vec![path_value(path)], context)
    }
    fn rename_file(&self, source: &Path, destination: &Path, context: &Context) -> FileFuture<()> {
        self.unit(
            "renameFile",
            vec![path_value(source), path_value(destination)],
            context,
        )
    }
    fn list_dir(&self, path: &Path, context: &Context) -> FileFuture<Vec<FileInfo>> {
        let call = self.call("listDir", vec![path_value(path)], context);
        Box::pin(async move {
            let HostValue::Array(items) = call.await? else {
                return Err(FileError::new(
                    FileErrorCode::Invalid,
                    "Expected a listing",
                    None,
                ));
            };
            items
                .into_iter()
                .map(|item| {
                    let text = |key: &str| {
                        item.get(key)
                            .and_then(HostValue::as_string)
                            .map(JsonString::to_string_lossy)
                    };
                    let number =
                        |key: &str| item.get(key).and_then(HostValue::as_f64).unwrap_or(0.0);
                    Ok(FileInfo {
                        name: text("name").unwrap_or_default().into(),
                        path: text("path").unwrap_or_default().into(),
                        kind: match text("kind").as_deref() {
                            Some("file") => FileKind::File,
                            Some("directory") => FileKind::Directory,
                            Some("symlink") => FileKind::Symlink,
                            _ => {
                                return Err(FileError::new(
                                    FileErrorCode::Invalid,
                                    "Unknown file kind",
                                    None,
                                ));
                            }
                        },
                        size: number("size") as u64,
                        mtime_ms: number("mtimeMs"),
                    })
                })
                .collect()
        })
    }
    fn create_dir(&self, path: &Path, recursive: bool, context: &Context) -> FileFuture<()> {
        let options = HostValue::object([("recursive", HostValue::Bool(recursive))]);
        self.unit("createDir", vec![path_value(path), options], context)
    }
    fn remove(
        &self,
        path: &Path,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> FileFuture<()> {
        let options = HostValue::object([
            ("recursive", HostValue::Bool(recursive)),
            ("force", HostValue::Bool(force)),
        ]);
        self.unit("remove", vec![path_value(path), options], context)
    }
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

fn store(storage: impl Storage + 'static) -> HostValue {
    HostValue::native(Store(Arc::new(storage)))
}

fn durable(error: pi_durable::Error) -> HostError {
    host_error(&error)
}

impl Dispatch for StorageDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("storage.sqliteConfig", |_, _| {
            let schema = create_sqlite_schema(None)
                .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))?;
            let migrations = create_sqlite_migrations(&schema)
                .into_iter()
                .map(|m| {
                    HostValue::object([
                        ("version", HostValue::Number(m.version as f64)),
                        (
                            "statements",
                            HostValue::Array(
                                m.statements.iter().map(|s| HostValue::string(s)).collect(),
                            ),
                        ),
                    ])
                })
                .collect();
            Ok(HostValue::object([
                (
                    "version",
                    HostValue::Number(CURRENT_SQLITE_SCHEMA_VERSION as f64),
                ),
                ("migrations", HostValue::Array(migrations)),
            ]))
        });
        m.sync("storage.sqliteOpen", |_, args: Args| {
            promise_now(async move {
                let database = JsSqliteDatabase::new(args.handle(0)?, Some(args.handle(1)?));
                let prefix = args.get(2).as_string().map(JsonString::to_string_lossy);
                let storage =
                    SqliteStorage::open(Arc::new(database), SqliteStorageOptions { prefix })
                        .await
                        .map_err(durable)?;
                Ok(store(storage))
            })
        });
        m.sync("storage.sqliteMigrate", |_, args: Args| {
            promise_now(async move {
                let database = JsSqliteDatabase::new(args.handle(0)?, Some(args.handle(1)?));
                let migrations = migrations(args.get(2))?;
                apply_sqlite_migrations(&database, migrations.as_deref())
                    .await
                    .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))?;
                Ok(HostValue::Undefined)
            })
        });
        m.sync("storage.sqliteTransaction", |_, args: Args| {
            promise_now(async move {
                let state = args.native::<SqliteTransactionState>(0)?;
                let executor = JsSqliteDatabase::new(args.handle(1)?, None);
                let callback = state.callback.lock().take().ok_or_else(|| {
                    HostError::new(ErrorClass::Error, "SQLite transaction callback already ran")
                })?;
                let result = callback(Arc::new(executor)).await;
                let failure = result
                    .as_ref()
                    .err()
                    .map(|e| HostError::new(ErrorClass::Error, e.to_string()));
                *state.result.lock() = Some(result);
                match failure {
                    Some(error) => Err(error),
                    None => Ok(HostValue::Undefined),
                }
            })
        });
        m.sync("storage.jsonlOpen", |_, args: Args| {
            promise_now(async move {
                let fs = JsFileSystem::new(
                    args.handle(1)?,
                    args.get(2).clone(),
                    PathBuf::from(args.text(3)?),
                    args.text(4)?,
                );
                let options = JsonlStorageOptions {
                    fsync: args.get(5).as_bool().unwrap_or(false),
                };
                let storage =
                    JsonlStorage::open(args.text(0)?, Arc::new(fs), &Context::default(), options)
                        .await
                        .map_err(durable)?;
                Ok(store(storage))
            })
        });
        // The backend call starts here, on the JS thread in call order, as upstream's storages
        // begin each operation when it is called; only its settlement is awaited.
        m.sync("storage.call", |_, args: Args| {
            let failure = Arc::new(Mutex::new(None::<HostError>));
            let started = (|| -> HostResult<_> {
                let storage = args.native::<Store>(0)?;
                let method = args.text(1)?;
                let values = strict_args(args.get(2))?;
                let adopt = args.get(3).as_handle().cloned();
                let capture = failure.clone();
                Ok(pi_durable::testing::storage_request(
                    storage.0.as_ref(),
                    &method,
                    &values,
                    Box::new(move |seq| {
                        if let Some(adopt) = &adopt
                            && let Err(error) =
                                call_sync(adopt, None, vec![HostValue::Number(seq.get() as f64)])
                        {
                            *capture.lock() = Some(error);
                        }
                    }),
                ))
            })();
            let started = started?;
            promise_now(async move {
                let result = started.await;
                if let Some(error) = failure.lock().take() {
                    return Err(error);
                }
                result.map(from_strict_option).map_err(durable)
            })
        });
    }
}
