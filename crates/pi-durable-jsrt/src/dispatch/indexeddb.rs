//! `@earendil-works/pi-durable/storage/indexeddb`: the native `IndexedDbStorage` transaction logic
//! (`pi_durable::storage::indexeddb`) over the caller's own IndexedDB objects (`idb.*`).
//!
//! [`JsIdbBackend`] implements the native `IdbBackend` with a glue port
//! (`js/storage/indexeddb.js`) that issues each request on the JS `IDBDatabase` and reports its
//! outcome back synchronously from the request's own event handler (`idb.settle`). IndexedDB
//! transactions commit once no request is pending at the end of a task, so the storage's
//! operations are polled on the JS thread ([`JsThreadExecutor`]): a request's settlement wakes the
//! operation inside that event handler, where its next request is issued while the transaction
//! is still active, as the browser binding (`web.rs`, `spawn_local`) does.
//!
//! Methods:
//! - `idb.schema()` → the upstream schema the glue creates on `upgradeneeded`;
//! - `idb.adopt(port, database)` → promise of a store for `storage.call` (the `IndexedDbStorage`
//!   over an opened, upgraded `IDBDatabase`; its metadata row is checked as upstream's `open`);
//! - `idb.settle(slot, ok, value)`: complete one pending request or transaction outcome;
//! - `idb.accept(filter, row)` → whether a cursor row passes the storage's native filter.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::task::{ArcWake, waker_ref};
use parking_lot::Mutex;
use pi_durable::storage::indexeddb::{
    Accept, Direction, IdbBackend, IdbFuture, IdbKey, IdbKeyRange, IdbMode, IdbSpawner,
    IdbTransaction, IndexedDbStorage, Source,
};
use pi_durable::{Error, JsonValue, Result};
use pi_jsrt::{JsHandle, JsRuntime};

use crate::dispatch::storage::{Store, host_error, promise_now};
use crate::errors::{ErrorClass, HostError};
use crate::host::{Args, Dispatch, Methods};
use crate::value::HostValue;

/// The `idb.*` method group.
pub struct IndexedDbDispatch;

// ---------------------------------------------------------------------------
// Polling on the JS thread
// ---------------------------------------------------------------------------

struct Task {
    future: Mutex<Option<BoxFuture<'static, ()>>>,
    queued: AtomicBool,
    runtime: JsRuntime,
}

thread_local! {
    static READY: RefCell<VecDeque<Arc<Task>>> = const { RefCell::new(VecDeque::new()) };
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

impl ArcWake for Task {
    fn wake_by_ref(task: &Arc<Self>) {
        if task.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        if task.runtime.is_js_thread() {
            READY.with(|ready| ready.borrow_mut().push_back(task.clone()));
            if !DRAINING.with(Cell::get) {
                // Woken outside a drain (not from a request's event handler): poll from a task.
                let _ = task
                    .runtime
                    .post_task(|ctx| pi_jsrt::ctx::host_call(ctx, drain));
            }
        } else {
            let task = task.clone();
            let runtime = task.runtime.clone();
            let _ = runtime.post_task(move |ctx| {
                READY.with(|ready| ready.borrow_mut().push_back(task));
                pi_jsrt::ctx::host_call(ctx, drain);
            });
        }
    }
}

/// Poll every woken operation; must run on the JS thread inside a host call (requests are
/// issued through `pi_jsrt::ctx::with_current`).
fn drain() {
    if DRAINING.with(|draining| draining.replace(true)) {
        return;
    }
    while let Some(task) = READY.with(|ready| ready.borrow_mut().pop_front()) {
        task.queued.store(false, Ordering::Release);
        let mut slot = task.future.lock();
        if let Some(mut future) = slot.take() {
            let waker = waker_ref(&task);
            let mut cx = std::task::Context::from_waker(&waker);
            if future.as_mut().poll(&mut cx).is_pending() {
                *slot = Some(future);
            }
        }
    }
    DRAINING.with(|draining| draining.set(false));
}

/// An [`IdbSpawner`] polling on `runtime`'s JS thread: at once when called there (inside a host
/// call), then whenever woken.
fn js_thread_spawner(runtime: JsRuntime) -> IdbSpawner {
    Arc::new(move |future: BoxFuture<'static, ()>| {
        let task = Arc::new(Task {
            future: Mutex::new(Some(future)),
            queued: AtomicBool::new(false),
            runtime: runtime.clone(),
        });
        ArcWake::wake_by_ref(&task);
        if runtime.is_js_thread() && pi_jsrt::ctx::with_current(|_| ()).is_some() {
            drain();
        }
    })
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

fn key_value(key: &IdbKey) -> HostValue {
    match key {
        IdbKey::Number(n) => HostValue::Number(*n),
        IdbKey::String(s) => HostValue::String(s.clone()),
        IdbKey::Array(items) => HostValue::Array(items.iter().map(key_value).collect()),
    }
}

fn range_value(range: &IdbKeyRange) -> HostValue {
    let bound = |bound: &Option<(IdbKey, bool)>| match bound {
        Some((key, open)) => (key_value(key), HostValue::Bool(*open)),
        None => (HostValue::Undefined, HostValue::Bool(false)),
    };
    let (lower, lower_open) = bound(&range.lower);
    let (upper, upper_open) = bound(&range.upper);
    HostValue::object([
        ("lower", lower),
        ("upper", upper),
        ("lowerOpen", lower_open),
        ("upperOpen", upper_open),
    ])
}

fn row_value(row: &JsonValue) -> HostValue {
    HostValue::from_strict(row)
}

fn json(value: HostValue) -> Result<Option<JsonValue>> {
    match value {
        HostValue::Undefined => Ok(None),
        value => value
            .to_strict_omitting_undefined()
            .map(Some)
            .map_err(|e| Error::message(e.message().to_string_lossy())),
    }
}

fn js_failure(value: &HostValue) -> Error {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(HostValue::as_string)
            .map(|s| s.to_string_lossy())
    };
    match (text("name"), text("message")) {
        (Some(name), Some(message)) if name != "Error" => {
            Error::message(format!("{name}: {message}"))
        }
        (_, Some(message)) => Error::message(message),
        _ => Error::message("IndexedDB request failed"),
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// One pending request or transaction outcome, settled by `idb.settle`.
struct Slot(Mutex<Option<oneshot::Sender<Result<HostValue>>>>);

fn slot() -> (HostValue, oneshot::Receiver<Result<HostValue>>) {
    let (send, receive) = oneshot::channel();
    (HostValue::native(Slot(Mutex::new(Some(send)))), receive)
}

/// Call `port[method](...args)` now; requires the JS thread inside a host call.
fn port_call(port: &JsHandle, method: &str, args: Vec<HostValue>) -> Result<HostValue> {
    crate::host::call_sync(port, Some(method), args)
        .map_err(|e| Error::message(e.message().to_string_lossy()))
}

async fn outcome(receive: oneshot::Receiver<Result<HostValue>>) -> Result<HostValue> {
    receive
        .await
        .map_err(|_| Error::message("IndexedDB request was dropped"))?
}

/// An `IDBDatabase` opened by the glue.
pub struct JsIdbBackend {
    port: JsHandle,
    database: JsHandle,
}

/// One `IDBTransaction`.
pub struct JsIdbTransaction {
    port: JsHandle,
    transaction: JsHandle,
    done: oneshot::Receiver<Result<HostValue>>,
}

impl IdbBackend for JsIdbBackend {
    type Transaction = JsIdbTransaction;

    fn transaction(&self, stores: &[&'static str], mode: IdbMode) -> Result<Self::Transaction> {
        let (done_slot, done) = slot();
        let names = stores.iter().map(|s| HostValue::string(s)).collect();
        let mode = match mode {
            IdbMode::ReadOnly => "readonly",
            IdbMode::ReadWrite => "readwrite",
        };
        let transaction = port_call(
            &self.port,
            "transaction",
            vec![
                HostValue::Handle(self.database.clone()),
                HostValue::Array(names),
                HostValue::string(mode),
                done_slot,
            ],
        )?;
        let HostValue::Handle(transaction) = transaction else {
            return Err(Error::message("IndexedDB transaction was not created"));
        };
        Ok(JsIdbTransaction {
            port: self.port.clone(),
            transaction,
            done,
        })
    }

    fn close(&self) {
        let _ = port_call(
            &self.port,
            "close",
            vec![HostValue::Handle(self.database.clone())],
        );
    }
}

impl JsIdbTransaction {
    fn request(&self, method: &'static str, mut args: Vec<HostValue>) -> IdbFuture<'_, HostValue> {
        let (settle, receive) = slot();
        args.insert(0, HostValue::Handle(self.transaction.clone()));
        args.push(settle);
        let issued = port_call(&self.port, method, args);
        Box::pin(async move {
            issued?;
            outcome(receive).await
        })
    }
}

impl IdbTransaction for JsIdbTransaction {
    fn get(&self, store: &'static str, key: IdbKey) -> IdbFuture<'_, Option<JsonValue>> {
        let request = self.request(
            "get",
            vec![
                HostValue::string(store),
                HostValue::Undefined,
                key_value(&key),
            ],
        );
        Box::pin(async move { json(request.await?) })
    }
    fn index_get(
        &self,
        store: &'static str,
        index: &'static str,
        key: IdbKey,
    ) -> IdbFuture<'_, Option<JsonValue>> {
        let request = self.request(
            "get",
            vec![
                HostValue::string(store),
                HostValue::string(index),
                key_value(&key),
            ],
        );
        Box::pin(async move { json(request.await?) })
    }
    fn add(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()> {
        let request = self.request("add", vec![HostValue::string(store), row_value(&row)]);
        Box::pin(async move { request.await.map(|_| ()) })
    }
    fn put(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()> {
        let request = self.request("put", vec![HostValue::string(store), row_value(&row)]);
        Box::pin(async move { request.await.map(|_| ()) })
    }
    fn delete(&self, store: &'static str, range: IdbKeyRange) -> IdbFuture<'_, ()> {
        let request = self.request(
            "delete",
            vec![HostValue::string(store), range_value(&range)],
        );
        Box::pin(async move { request.await.map(|_| ()) })
    }
    fn values(
        &self,
        source: Source,
        range: Option<IdbKeyRange>,
        direction: Direction,
        limit: usize,
        accept: Option<Accept>,
    ) -> IdbFuture<'_, Vec<JsonValue>> {
        let filter = accept.map(|accept| HostValue::native(Filter(accept)));
        let request = self.request(
            "values",
            vec![
                HostValue::string(source.store),
                source.index.map_or(HostValue::Undefined, HostValue::string),
                range.as_ref().map_or(HostValue::Undefined, range_value),
                HostValue::string(match direction {
                    Direction::Next => "next",
                    Direction::Prev => "prev",
                }),
                HostValue::Number(limit as f64),
                filter.unwrap_or_default(),
            ],
        );
        Box::pin(async move {
            match request.await? {
                HostValue::Array(rows) => rows
                    .into_iter()
                    .map(|row| Ok(json(row)?.unwrap_or(JsonValue::Null)))
                    .collect(),
                _ => Err(Error::message("IndexedDB cursor returned no rows")),
            }
        })
    }
    fn finish(self) -> IdbFuture<'static, ()> {
        Box::pin(async move { outcome(self.done).await.map(|_| ()) })
    }
    fn abort(self) -> IdbFuture<'static, ()> {
        let aborted = port_call(
            &self.port,
            "abort",
            vec![HostValue::Handle(self.transaction.clone())],
        );
        Box::pin(async move {
            aborted?;
            // An abort settles the outcome as a failure, which is what was asked for.
            let _ = outcome(self.done).await;
            Ok(())
        })
    }
}

/// A cursor row filter of the native storage.
struct Filter(Accept);

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

impl Dispatch for IndexedDbDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("idb.adopt", |call, args: Args| {
            let backend = JsIdbBackend {
                port: args.handle(0)?,
                database: args.handle(1)?,
            };
            let spawner = js_thread_spawner(call.runtime().clone());
            let operations = spawner.clone();
            let (send, receive) = oneshot::channel();
            spawner(Box::pin(async move {
                let _ = send
                    .send(IndexedDbStorage::from_backend_with_spawner(backend, operations).await);
            }));
            promise_now(async move {
                let storage = receive
                    .await
                    .map_err(|_| HostError::new(ErrorClass::Error, "IndexedDB open was dropped"))?
                    .map_err(|e| host_error(&e))?;
                Ok(HostValue::native(Store(Arc::new(storage))))
            })
        });
        m.sync("idb.settle", |_, args: Args| {
            let slot = args.native::<Slot>(0)?;
            let result = if args.bool(1)? {
                Ok(args.get(2).clone())
            } else {
                Err(js_failure(args.get(2)))
            };
            if let Some(send) = slot.0.lock().take() {
                let _ = send.send(result);
            }
            // The woken operation issues its next request now, inside this event handler.
            drain();
            Ok(HostValue::Undefined)
        });
        // The upstream schema (database version, stores, key paths, indexes, initial metadata row).
        m.sync("idb.schema", |_, _| {
            use pi_durable::storage::indexeddb::{KeyPath, schema};
            let path = |path: KeyPath| match path {
                KeyPath::Single(name) => HostValue::string(name),
                KeyPath::Compound(names) => {
                    HostValue::Array(names.iter().map(|n| HostValue::string(n)).collect())
                }
            };
            let stores = schema::SCHEMA
                .iter()
                .map(|store| {
                    HostValue::object([
                        ("name", HostValue::string(store.name)),
                        ("keyPath", path(store.key_path)),
                        (
                            "indexes",
                            HostValue::Array(
                                store
                                    .indexes
                                    .iter()
                                    .map(|index| {
                                        HostValue::object([
                                            ("name", HostValue::string(index.name)),
                                            ("keyPath", path(index.key_path)),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                    ])
                })
                .collect();
            Ok(HostValue::object([
                (
                    "version",
                    HostValue::Number(schema::DATABASE_VERSION as f64),
                ),
                ("meta", HostValue::string(schema::META)),
                (
                    "initialMeta",
                    HostValue::from_strict(&schema::initial_meta()),
                ),
                ("stores", HostValue::Array(stores)),
            ]))
        });
        m.sync("idb.accept", |_, args: Args| {
            let filter = args.native::<Filter>(0)?;
            let row = args
                .get(1)
                .to_strict_omitting_undefined()
                .unwrap_or(JsonValue::Null);
            Ok(HostValue::Bool((filter.0)(&row)))
        });
    }
}
