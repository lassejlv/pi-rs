//! Durable Object SQLite storage for Rust running in a Worker (wasm32, `worker`
//! feature). The host is the JavaScript `ctx.storage` object: workers-rs exposes no
//! SQL API, so this calls `storage.sql.exec(...)` and `storage.transaction(...)`
//! through wasm-bindgen. Workers are single-threaded, so the JavaScript handles are
//! wrapped to satisfy the `Send` bounds of the portable host contract.

use super::{
    DurableObjectSqliteDatabase, DurableObjectSqliteStorage, SqliteStorage, SqliteStorageOptions,
};
use crate::storage::StorageFuture;
use indexmap::IndexMap;
use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use parking_lot::Mutex;
use pi_chord::sqlite::{
    SqliteError, SqliteFuture, SqliteResult, SqliteTransactionResult,
    cloudflare::{
        DurableObjectSqlCursor, DurableObjectSqlRow, DurableObjectSqlValue,
        DurableObjectSqliteHost, DurableObjectTransactionCallback,
    },
};
use std::sync::Arc;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, future_to_promise};

/// A value that is only touched from the Worker's single thread.
struct Local<T>(T);
// SAFETY: wasm32-unknown-unknown Workers run one thread; nothing here is shared across threads.
unsafe impl<T> Send for Local<T> {}
unsafe impl<T> Sync for Local<T> {}

fn error(value: JsValue) -> SqliteError {
    let message = Reflect::get(&value, &JsValue::from_str("message"))
        .ok()
        .and_then(|message| message.as_string())
        .or_else(|| value.as_string())
        .unwrap_or_else(|| "Durable Object SQL failed".into());
    SqliteError::new(message)
}
fn call(target: &JsValue, method: &str, args: &Array) -> SqliteResult<JsValue> {
    let function = Reflect::get(target, &JsValue::from_str(method))
        .map_err(error)?
        .dyn_into::<Function>()
        .map_err(|_| SqliteError::new(format!("{method} is not a function")))?;
    function.apply(target, args).map_err(error)
}
fn to_js(value: &DurableObjectSqlValue) -> JsValue {
    match value {
        DurableObjectSqlValue::Null => JsValue::NULL,
        DurableObjectSqlValue::Number(n) => JsValue::from_f64(*n),
        DurableObjectSqlValue::Text(s) => JsValue::from_str(s),
        DurableObjectSqlValue::Blob(bytes) => Uint8Array::from(bytes.as_slice()).buffer().into(),
    }
}
fn from_js(value: JsValue) -> SqliteResult<DurableObjectSqlValue> {
    if value.is_null() || value.is_undefined() {
        Ok(DurableObjectSqlValue::Null)
    } else if let Some(n) = value.as_f64() {
        Ok(DurableObjectSqlValue::Number(n))
    } else if let Some(s) = value.as_string() {
        Ok(DurableObjectSqlValue::Text(s))
    } else if let Some(buffer) = value.dyn_ref::<ArrayBuffer>() {
        Ok(DurableObjectSqlValue::Blob(
            Uint8Array::new(buffer).to_vec(),
        ))
    } else {
        Err(SqliteError::new(
            "Durable Object SQL returned an unsupported value",
        ))
    }
}
fn to_row(row: &JsValue) -> SqliteResult<DurableObjectSqlRow> {
    let mut out: DurableObjectSqlRow = IndexMap::new();
    for key in Object::keys(row.unchecked_ref::<Object>()).iter() {
        let value = Reflect::get(row, &key).map_err(error)?;
        out.insert(key.as_string().unwrap_or_default(), from_js(value)?);
    }
    Ok(out)
}

struct Cursor(Local<JsValue>);
impl DurableObjectSqlCursor for Cursor {
    fn next(&mut self) -> SqliteResult<Option<DurableObjectSqlRow>> {
        let step = call(&self.0.0, "next", &Array::new())?;
        if Reflect::get(&step, &JsValue::from_str("done"))
            .map_err(error)?
            .is_truthy()
        {
            return Ok(None);
        }
        to_row(&Reflect::get(&step, &JsValue::from_str("value")).map_err(error)?).map(Some)
    }
    fn into_array(self: Box<Self>) -> SqliteResult<Vec<DurableObjectSqlRow>> {
        call(&self.0.0, "toArray", &Array::new())?
            .dyn_into::<Array>()
            .map_err(|_| SqliteError::new("toArray did not return an array"))?
            .iter()
            .map(|row| to_row(&row))
            .collect()
    }
}

/// The host contract over a JavaScript `DurableObjectStorage`.
pub struct JsDurableObjectSqliteHost {
    storage: Local<JsValue>,
}
impl JsDurableObjectSqliteHost {
    /// `ctx.storage` of a SQLite-backed Durable Object.
    pub fn new(storage: JsValue) -> Self {
        Self {
            storage: Local(storage),
        }
    }
}
impl DurableObjectSqliteHost for JsDurableObjectSqliteHost {
    fn exec(
        &self,
        sql: &str,
        bindings: &[DurableObjectSqlValue],
    ) -> SqliteResult<Box<dyn DurableObjectSqlCursor>> {
        let storage = &self.storage.0;
        let sql_api = Reflect::get(storage, &JsValue::from_str("sql")).map_err(error)?;
        let args = Array::new();
        args.push(&JsValue::from_str(sql));
        for binding in bindings {
            args.push(&to_js(binding));
        }
        let cursor = call(&sql_api, "exec", &args)?;
        Ok(Box::new(Cursor(Local(cursor))))
    }
    /// `storage.transaction()` commits when the callback's promise resolves and
    /// rolls back when it rejects. A rejection that is not the callback's own is a
    /// rollback failure and is reported next to the callback's error.
    fn transaction_raw(
        &self,
        callback: DurableObjectTransactionCallback,
    ) -> SqliteFuture<SqliteTransactionResult> {
        let storage = self.storage.0.clone();
        let task = Local(async move {
            let outcome: Arc<Mutex<Option<SqliteResult<SqliteTransactionResult>>>> = Arc::default();
            let slot = outcome.clone();
            let closure = Closure::once(move || -> Promise {
                future_to_promise(async move {
                    match callback().await {
                        Ok(value) => {
                            *slot.lock() = Some(Ok(value));
                            Ok(JsValue::UNDEFINED)
                        }
                        Err(failure) => {
                            let reason = JsValue::from_str(&failure.to_string());
                            *slot.lock() = Some(Err(failure));
                            Err(reason)
                        }
                    }
                })
            });
            let args = Array::new();
            args.push(closure.as_ref());
            let settled = match call(&storage, "transaction", &args) {
                Ok(promise) => JsFuture::from(promise.unchecked_into::<Promise>())
                    .await
                    .map_err(error),
                Err(failure) => Err(failure),
            };
            drop(closure);
            let callback_result = outcome.lock().take();
            match (callback_result, settled) {
                (Some(Ok(value)), Ok(_)) => Ok(value),
                (Some(Ok(_)), Err(commit)) => Err(commit),
                (Some(Err(primary)), Ok(_)) => Err(primary),
                (Some(Err(primary)), Err(rollback)) => {
                    if rollback.to_string() == primary.to_string() {
                        Err(primary)
                    } else {
                        Err(SqliteError::rollback(primary, rollback))
                    }
                }
                (None, Err(failure)) => Err(failure),
                (None, Ok(_)) => Err(SqliteError::new("Durable Object transaction did not run")),
            }
        });
        // SAFETY: see `Local`; the future is only polled on the Worker's thread.
        struct Wrapped<F>(Local<F>);
        impl<F: std::future::Future> std::future::Future for Wrapped<F> {
            type Output = F::Output;
            fn poll(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                unsafe { self.map_unchecked_mut(|wrapped| &mut (wrapped.0).0) }.poll(cx)
            }
        }
        Box::pin(Wrapped(task))
    }
}

/// Open durable storage on `ctx.storage` of a SQLite-backed Durable Object,
/// optionally under a table prefix.
pub fn open_js_durable_object_sqlite_storage(
    storage: JsValue,
    options: SqliteStorageOptions,
) -> StorageFuture<SqliteStorage> {
    super::open_durable_object_sqlite_storage(
        Arc::new(DurableObjectSqliteStorage::new(Arc::new(
            JsDurableObjectSqliteHost::new(storage),
        ))),
        options,
    )
}
/// The database for direct SQL next to durable storage, such as host migrations.
pub fn js_durable_object_sqlite_database(storage: JsValue) -> DurableObjectSqliteDatabase {
    DurableObjectSqliteDatabase::new(Arc::new(DurableObjectSqliteStorage::new(Arc::new(
        JsDurableObjectSqliteHost::new(storage),
    ))))
}
