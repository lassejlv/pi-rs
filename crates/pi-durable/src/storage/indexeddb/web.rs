//! The browser binding of [`IdbBackend`]: `indexedDB` through web-sys, in a
//! window or a worker. Rows cross the boundary as plain JavaScript objects, the
//! same structured-clone values the reference implementation stores; strings
//! keep their UTF-16 code units, lone surrogates included.
use super::{
    backend::{Accept, IdbBackend, IdbFuture, IdbMode, IdbTransaction, Source},
    key::{self, Direction, IdbKey, IdbKeyRange, KeyPath},
    schema::{self, DATABASE_VERSION},
};
use crate::{Error, JsonString, JsonValue, Result};
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use js_sys::{Array, JsString, Object, Reflect};
use std::{cell::RefCell, rc::Rc, sync::Arc};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{
    DomException, Event, IdbCursorDirection, IdbCursorWithValue, IdbDatabase, IdbFactory,
    IdbObjectStore, IdbObjectStoreParameters, IdbOpenDbRequest, IdbRequest, IdbTransactionMode,
};

fn js_error(value: JsValue) -> Error {
    if let Some(exception) = value.dyn_ref::<DomException>() {
        return Error::message(format!("{}: {}", exception.name(), exception.message()));
    }
    if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        return Error::message(String::from(error.message()));
    }
    Error::message(format!("{value:?}"))
}
fn request_error(request: &IdbRequest, fallback: &str) -> Error {
    match request.error() {
        Ok(Some(exception)) => js_error(exception.into()),
        _ => Error::message(fallback),
    }
}

fn js_string(value: &JsonString) -> JsValue {
    JsString::from_char_code(value.units()).into()
}
/// JSON to the value IndexedDB stores. Objects have no prototype, so a
/// `"__proto__"` key stays an ordinary own property.
pub fn to_js(value: &JsonValue) -> JsValue {
    match value {
        JsonValue::Null => JsValue::NULL,
        JsonValue::Bool(value) => JsValue::from_bool(*value),
        JsonValue::Number(value) => JsValue::from_f64(value.as_f64().unwrap_or(f64::NAN)),
        JsonValue::String(value) => js_string(value),
        JsonValue::Array(items) => {
            let array = Array::new();
            for item in items.iter() {
                array.push(&to_js(item));
            }
            array.into()
        }
        JsonValue::Object(map) => {
            let object = Object::create(JsValue::NULL.unchecked_ref::<Object>());
            for (key, value) in map.iter() {
                Reflect::set(&object, &js_string(key), &to_js(value)).expect("plain object");
            }
            object.into()
        }
    }
}
/// A stored value back to JSON; `undefined` properties are absent, as they are
/// after the reference's own JSON boundary.
pub fn from_js(value: &JsValue) -> Result<Option<JsonValue>> {
    if value.is_undefined() {
        return Ok(None);
    }
    if value.is_null() {
        return Ok(Some(JsonValue::Null));
    }
    if let Some(value) = value.as_bool() {
        return Ok(Some(JsonValue::Bool(value)));
    }
    if let Some(value) = value.as_f64() {
        return Ok(Some(key::number(value)));
    }
    if let Some(value) = value.dyn_ref::<JsString>() {
        return Ok(Some(JsonValue::String(JsonString::from_utf16(
            value.iter().collect(),
        ))));
    }
    if Array::is_array(value) {
        let array: &Array = value.unchecked_ref();
        let items = array
            .iter()
            .map(|item| Ok(from_js(&item)?.unwrap_or(JsonValue::Null)))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Some(JsonValue::Array(Arc::new(items))));
    }
    if let Some(object) = value.dyn_ref::<Object>() {
        let mut result = crate::wire::object();
        let map = result.as_object_mut().expect("object");
        for key in Object::keys(object).iter() {
            let property = Reflect::get(object, &key).map_err(js_error)?;
            if let Some(property) = from_js(&property)? {
                let key: &JsString = key.unchecked_ref();
                map.insert(JsonString::from_utf16(key.iter().collect()), property);
            }
        }
        return Ok(Some(result));
    }
    Err(Error::message(
        "IndexedDB returned a value that is not JSON",
    ))
}
fn key_to_js(key: &IdbKey) -> JsValue {
    match key {
        IdbKey::Number(value) => JsValue::from_f64(*value),
        IdbKey::String(value) => js_string(value),
        IdbKey::Array(items) => {
            let array = Array::new();
            for item in items {
                array.push(&key_to_js(item));
            }
            array.into()
        }
    }
}
fn range_to_js(range: &IdbKeyRange) -> Result<JsValue> {
    let range = match (&range.lower, &range.upper) {
        (Some((lower, lower_open)), Some((upper, upper_open))) => {
            web_sys::IdbKeyRange::bound_with_lower_open_and_upper_open(
                &key_to_js(lower),
                &key_to_js(upper),
                *lower_open,
                *upper_open,
            )
        }
        (Some((lower, open)), None) => {
            web_sys::IdbKeyRange::lower_bound_with_open(&key_to_js(lower), *open)
        }
        (None, Some((upper, open))) => {
            web_sys::IdbKeyRange::upper_bound_with_open(&key_to_js(upper), *open)
        }
        (None, None) => return Ok(JsValue::NULL),
    };
    range.map(Into::into).map_err(js_error)
}
fn key_path_to_js(path: KeyPath) -> JsValue {
    match path {
        KeyPath::Single(name) => JsValue::from_str(name),
        KeyPath::Compound(names) => {
            let array = Array::new();
            for name in names {
                array.push(&JsValue::from_str(name));
            }
            array.into()
        }
    }
}

type Handler = Closure<dyn FnMut(Event)>;
/// Detaches request callbacks before their closures are freed.
struct Detach(IdbRequest);
impl Drop for Detach {
    fn drop(&mut self) {
        self.0.set_onsuccess(None);
        self.0.set_onerror(None);
    }
}
async fn settle(request: IdbRequest) -> Result<JsValue> {
    let (send, receive) = oneshot::channel();
    let send = Rc::new(RefCell::new(Some(send)));
    let success: Handler = {
        let (send, request) = (send.clone(), request.clone());
        Closure::new(move |_| {
            if let Some(send) = send.borrow_mut().take() {
                let _ = send.send(request.result().map_err(js_error));
            }
        })
    };
    let failure: Handler = {
        let request = request.clone();
        Closure::new(move |_| {
            if let Some(send) = send.borrow_mut().take() {
                let _ = send.send(Err(request_error(&request, "IndexedDB request failed")));
            }
        })
    };
    request.set_onsuccess(Some(success.as_ref().unchecked_ref()));
    request.set_onerror(Some(failure.as_ref().unchecked_ref()));
    let _detach = Detach(request);
    receive
        .await
        .map_err(|_| Error::message("IndexedDB request was dropped"))?
}
/// `cursorValues()`: walk a cursor request, continuing until `limit` rows are
/// accepted or the cursor ends.
async fn walk(request: IdbRequest, limit: usize, accept: Option<Accept>) -> Result<Vec<JsonValue>> {
    let (send, mut receive) = mpsc::unbounded();
    let success: Handler = {
        let (send, request) = (send.clone(), request.clone());
        Closure::new(move |_| {
            let _ = send.unbounded_send(request.result().map_err(js_error));
        })
    };
    let failure: Handler = {
        let request = request.clone();
        Closure::new(move |_| {
            let _ = send.unbounded_send(Err(request_error(&request, "IndexedDB cursor failed")));
        })
    };
    request.set_onsuccess(Some(success.as_ref().unchecked_ref()));
    request.set_onerror(Some(failure.as_ref().unchecked_ref()));
    let _detach = Detach(request);
    let mut values = Vec::new();
    while let Some(cursor) = receive.next().await {
        let cursor = cursor?;
        if cursor.is_null() {
            break;
        }
        let cursor: IdbCursorWithValue = cursor.unchecked_into();
        let value = from_js(&cursor.value().map_err(js_error)?)?.unwrap_or(JsonValue::Null);
        if accept.as_ref().is_none_or(|accept| accept(&value)) {
            values.push(value);
        }
        if values.len() >= limit {
            break;
        }
        cursor.continue_().map_err(js_error)?;
    }
    Ok(values)
}

/// `indexedDB` of the current window or worker, or an explicit factory.
#[derive(Clone)]
pub struct WebIdbFactory {
    factory: IdbFactory,
}
impl WebIdbFactory {
    pub fn new(factory: IdbFactory) -> Self {
        Self { factory }
    }
    pub fn global() -> Result<Self> {
        let factory = Reflect::get(&js_sys::global(), &JsValue::from_str("indexedDB"))
            .ok()
            .and_then(|factory| factory.dyn_into::<IdbFactory>().ok())
            .ok_or_else(|| Error::message("IndexedDB is not available"))?;
        Ok(Self::new(factory))
    }
    /// Open or create a database, creating the durable schema on first open.
    pub async fn open(&self, name: &str) -> Result<WebIdbConnection> {
        let request: IdbOpenDbRequest = self
            .factory
            .open_with_u32(name, DATABASE_VERSION)
            .map_err(js_error)?;
        let upgrade: Closure<dyn FnMut(web_sys::IdbVersionChangeEvent)> = {
            let request = request.clone();
            Closure::new(move |event: web_sys::IdbVersionChangeEvent| {
                if event.old_version() != 0.0 {
                    return;
                }
                let created = request
                    .result()
                    .map(|database| create_schema(database.unchecked_ref()));
                if !matches!(created, Ok(Ok(())))
                    && let Some(transaction) = request.transaction()
                {
                    let _ = transaction.abort();
                }
            })
        };
        request.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
        let opened = settle((*request).clone()).await;
        request.set_onupgradeneeded(None);
        Ok(WebIdbConnection {
            database: opened?.unchecked_into(),
        })
    }
    /// Delete a closed database. Open connections can block deletion.
    pub async fn delete(&self, name: &str) -> Result<()> {
        let request = self.factory.delete_database(name).map_err(js_error)?;
        settle((*request).clone()).await.map(|_| ())
    }
}
/// `createSchema()`.
fn create_schema(database: &IdbDatabase) -> std::result::Result<(), JsValue> {
    for store in schema::SCHEMA {
        let parameters = IdbObjectStoreParameters::new();
        parameters.set_key_path(&key_path_to_js(store.key_path));
        let object_store =
            database.create_object_store_with_optional_parameters(store.name, &parameters)?;
        for index in store.indexes {
            object_store
                .create_index_with_str_sequence(index.name, &key_path_to_js(index.key_path))?;
        }
        if store.name == schema::META {
            object_store.add(&to_js(&schema::initial_meta()))?;
        }
    }
    Ok(())
}

pub struct WebIdbConnection {
    database: IdbDatabase,
}
impl IdbBackend for WebIdbConnection {
    type Transaction = WebIdbTransaction;
    fn transaction(&self, stores: &[&'static str], mode: IdbMode) -> Result<Self::Transaction> {
        let names = Array::new();
        for store in stores {
            names.push(&JsValue::from_str(store));
        }
        let mode = match mode {
            IdbMode::ReadOnly => IdbTransactionMode::Readonly,
            IdbMode::ReadWrite => IdbTransactionMode::Readwrite,
        };
        let transaction = self
            .database
            .transaction_with_str_sequence_and_mode(&names, mode)
            .map_err(js_error)?;
        Ok(WebIdbTransaction::new(transaction))
    }
    fn close(&self) {
        self.database.close();
    }
}

pub struct WebIdbTransaction {
    transaction: web_sys::IdbTransaction,
    done: oneshot::Receiver<Result<()>>,
    _handlers: [Handler; 3],
}
impl WebIdbTransaction {
    /// Listen for the outcome at creation, as `transactionResult()` does.
    fn new(transaction: web_sys::IdbTransaction) -> Self {
        let (send, done) = oneshot::channel();
        let send = Rc::new(RefCell::new(Some(send)));
        let settle = |fallback: Option<&'static str>| -> Handler {
            let (send, transaction) = (send.clone(), transaction.clone());
            Closure::new(move |_| {
                let outcome = match fallback {
                    None => Ok(()),
                    Some(fallback) => Err(transaction
                        .error()
                        .map_or_else(|| Error::message(fallback), |error| js_error(error.into()))),
                };
                if let Some(send) = send.borrow_mut().take() {
                    let _ = send.send(outcome);
                }
            })
        };
        let handlers = [
            settle(None),
            settle(Some("IndexedDB transaction was aborted")),
            settle(Some("IndexedDB transaction failed")),
        ];
        transaction.set_oncomplete(Some(handlers[0].as_ref().unchecked_ref()));
        transaction.set_onabort(Some(handlers[1].as_ref().unchecked_ref()));
        transaction.set_onerror(Some(handlers[2].as_ref().unchecked_ref()));
        Self {
            transaction,
            done,
            _handlers: handlers,
        }
    }
    fn store(&self, store: &str) -> Result<IdbObjectStore> {
        self.transaction.object_store(store).map_err(js_error)
    }
    async fn finished(self) -> Result<()> {
        let outcome = self
            .done
            .await
            .map_err(|_| Error::message("IndexedDB transaction was dropped"));
        self.transaction.set_oncomplete(None);
        self.transaction.set_onabort(None);
        self.transaction.set_onerror(None);
        outcome?
    }
}
impl IdbTransaction for WebIdbTransaction {
    fn get(&self, store: &'static str, key: IdbKey) -> IdbFuture<'_, Option<JsonValue>> {
        Box::pin(async move {
            let request = self.store(store)?.get(&key_to_js(&key)).map_err(js_error)?;
            from_js(&settle(request).await?)
        })
    }
    fn index_get(
        &self,
        store: &'static str,
        index: &'static str,
        key: IdbKey,
    ) -> IdbFuture<'_, Option<JsonValue>> {
        Box::pin(async move {
            let index = self.store(store)?.index(index).map_err(js_error)?;
            let request = index.get(&key_to_js(&key)).map_err(js_error)?;
            from_js(&settle(request).await?)
        })
    }
    fn add(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()> {
        Box::pin(async move {
            let request = self.store(store)?.add(&to_js(&row)).map_err(js_error)?;
            settle(request).await.map(|_| ())
        })
    }
    fn put(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()> {
        Box::pin(async move {
            let request = self.store(store)?.put(&to_js(&row)).map_err(js_error)?;
            settle(request).await.map(|_| ())
        })
    }
    fn delete(&self, store: &'static str, range: IdbKeyRange) -> IdbFuture<'_, ()> {
        Box::pin(async move {
            let request = self
                .store(store)?
                .delete(&range_to_js(&range)?)
                .map_err(js_error)?;
            settle(request).await.map(|_| ())
        })
    }
    fn values(
        &self,
        source: Source,
        range: Option<IdbKeyRange>,
        direction: Direction,
        limit: usize,
        accept: Option<Accept>,
    ) -> IdbFuture<'_, Vec<JsonValue>> {
        Box::pin(async move {
            let range = range.as_ref().map_or(Ok(JsValue::NULL), range_to_js)?;
            let direction = match direction {
                Direction::Next => IdbCursorDirection::Next,
                Direction::Prev => IdbCursorDirection::Prev,
            };
            let store = self.store(source.store)?;
            let request = match source.index {
                Some(index) => store
                    .index(index)
                    .map_err(js_error)?
                    .open_cursor_with_range_and_direction(&range, direction),
                None => store.open_cursor_with_range_and_direction(&range, direction),
            }
            .map_err(js_error)?;
            walk(request, limit, accept).await
        })
    }
    fn finish(self) -> IdbFuture<'static, ()> {
        Box::pin(self.finished())
    }
    fn abort(self) -> IdbFuture<'static, ()> {
        // It may already have aborted or committed.
        let _ = self.transaction.abort();
        Box::pin(async move {
            let _ = self.finished().await;
            Ok(())
        })
    }
}
