//! IndexedDB storage, a port of `storage/indexeddb.ts`.
//!
//! The object stores, indexes and row shapes are those of the reference, so a
//! database written by the TypeScript implementation opens here and vice versa.
//! All transaction logic is written against [`IdbBackend`]: [`MemoryIdbFactory`]
//! runs it on the host, and `web::WebIdbFactory` (wasm32, feature `indexeddb`)
//! binds it to the browser's `indexedDB`. One owner must serialize access to
//! each database.
mod backend;
pub mod key;
pub mod memory;
pub mod schema;
#[cfg(all(target_arch = "wasm32", feature = "indexeddb"))]
pub mod web;

pub use backend::*;
pub use key::{Direction, IdbKey, IdbKeyRange, KeyPath};
pub use memory::{MemoryIdbConnection, MemoryIdbFactory, MemoryIdbTransaction};

use super::{
    Adopt, Storage, StorageFuture, StoredEntry, ready,
    scan::{self, ScanStart},
};
use crate::{Context, Error, JsonString, JsonValue, Result, types::*, wire};
use futures::{FutureExt, channel::oneshot, future::Shared};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::delta::{Op, apply_immutable_batches};
use schema::*;
use serde::de::DeserializeOwned;
use std::{collections::HashMap, future::Future, sync::Arc};

struct State {
    closed: bool,
    active: usize,
    drained: Vec<oneshot::Sender<()>>,
    closing: Option<Shared<IdbFuture<'static, ()>>>,
}
struct Inner<B> {
    backend: B,
    state: Mutex<State>,
    /// Where operations run (see [`IdbSpawner`]); the default when `None`.
    spawner: Option<IdbSpawner>,
}
/// Runs one admitted storage operation to completion. An embedder whose IndexedDB objects
/// live on one thread with auto-committing transactions (an embedded JS engine) polls the
/// operations itself, so each request is issued while its transaction is still active.
#[cfg(not(target_arch = "wasm32"))]
pub type IdbSpawner = Arc<dyn Fn(futures::future::BoxFuture<'static, ()>) + Send + Sync>;
#[cfg(target_arch = "wasm32")]
pub type IdbSpawner = Arc<dyn Fn(futures::future::LocalBoxFuture<'static, ()>)>;
/// Durable storage over one IndexedDB connection.
pub struct IndexedDbStorage<B: IdbBackend> {
    inner: Arc<Inner<B>>,
}
impl<B: IdbBackend> Clone for IndexedDbStorage<B> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
// SAFETY: without the atomics target feature wasm32 has exactly one thread, so
// the JavaScript handles inside a browser backend can never be shared or sent.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<B: IdbBackend> Send for IndexedDbStorage<B> {}
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<B: IdbBackend> Sync for IndexedDbStorage<B> {}

/// One admitted operation; `close()` waits until none remain.
struct Active<B: IdbBackend>(Arc<Inner<B>>);
impl<B: IdbBackend> Drop for Active<B> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.active -= 1;
        if state.active == 0 {
            for drained in state.drained.drain(..) {
                let _ = drained.send(());
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn<T: Send + 'static>(
    job: impl Future<Output = Result<T>> + Send + 'static,
) -> StorageFuture<T> {
    super::line::eager(job)
}
#[cfg(not(target_arch = "wasm32"))]
fn failed<T: Send + 'static>(error: Error) -> StorageFuture<T> {
    ready(Err(error))
}
#[cfg(target_arch = "wasm32")]
fn failed<T: 'static>(error: Error) -> StorageFuture<T> {
    Box::pin(AssertSend(async move { Err(error) }))
}
/// A single-threaded future presented as `Send` for the storage contract.
#[cfg(target_arch = "wasm32")]
struct AssertSend<F>(F);
// SAFETY: see the `Send` impls above; wasm32 without atomics has one thread.
#[cfg(target_arch = "wasm32")]
unsafe impl<F> Send for AssertSend<F> {}
#[cfg(target_arch = "wasm32")]
impl<F: Future> Future for AssertSend<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|this| &mut this.0) }.poll(cx)
    }
}
/// Run the operation now on the browser's microtask queue, so a transaction
/// receives its first request before control returns to the event loop.
#[cfg(target_arch = "wasm32")]
fn spawn<T: 'static>(job: impl Future<Output = Result<T>> + 'static) -> StorageFuture<T> {
    let (send, receive) = oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let _ = send.send(job.await);
    });
    Box::pin(AssertSend(async move {
        receive
            .await
            .map_err(|_| Error::message("IndexedDB operation was dropped"))?
    }))
}
#[cfg(all(target_arch = "wasm32", feature = "indexeddb"))]
impl IndexedDbStorage<web::WebIdbConnection> {
    /// `IndexedDbStorage.open(name)` over the global `indexedDB`.
    pub async fn open(name: &str) -> Result<Self> {
        Self::open_with(&web::WebIdbFactory::global()?, name).await
    }
    pub async fn open_with(factory: &web::WebIdbFactory, name: &str) -> Result<Self> {
        Self::from_backend(factory.open(name).await?).await
    }
    /// `IndexedDbStorage.delete(name)`: delete a closed database.
    pub async fn delete(name: &str) -> Result<()> {
        web::WebIdbFactory::global()?.delete(name).await
    }
}

fn closed() -> Error {
    Error::message("IndexedDbStorage is closed")
}
fn missing_metadata() -> Error {
    Error::message("Durable IndexedDB metadata is missing")
}
fn unknown_conversation(id: ConversationId) -> Error {
    Error::request(format!("Unknown conversation: {id}"))
}

async fn settle<T, X: IdbTransaction>(tx: X, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            tx.finish().await?;
            Ok(value)
        }
        Err(error) => {
            // It may already have aborted or committed; preserve the operation failure.
            let _ = tx.abort().await;
            Err(error)
        }
    }
}

impl<B: IdbBackend> IndexedDbStorage<B> {
    /// Adopt an opened connection after checking its durable metadata row, as
    /// `IndexedDbStorage.open()` does. The connection is closed on failure.
    pub async fn from_backend(backend: B) -> Result<Self> {
        Self::from_backend_with(backend, None).await
    }
    /// [`Self::from_backend`] with operations run by `spawner`.
    pub async fn from_backend_with_spawner(backend: B, spawner: IdbSpawner) -> Result<Self> {
        Self::from_backend_with(backend, Some(spawner)).await
    }
    async fn from_backend_with(backend: B, spawner: Option<IdbSpawner>) -> Result<Self> {
        let checked = async {
            let tx = backend.transaction(&[META], IdbMode::ReadOnly)?;
            let result = tx.get(META, META_KEY.into()).await;
            settle(tx, result).await?.ok_or_else(missing_metadata)
        }
        .await;
        match checked {
            Ok(_) => Ok(Self {
                inner: Arc::new(Inner {
                    backend,
                    state: Mutex::new(State {
                        closed: false,
                        active: 0,
                        drained: Vec::new(),
                        closing: None,
                    }),
                    spawner,
                }),
            }),
            Err(error) => {
                backend.close();
                Err(error)
            }
        }
    }
    pub fn backend(&self) -> &B {
        &self.inner.backend
    }
    /// Run an operation through the spawner when there is one, else as the platform does.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn<T: Send + 'static>(
        &self,
        job: impl Future<Output = Result<T>> + Send + 'static,
    ) -> StorageFuture<T> {
        let Some(spawner) = &self.inner.spawner else {
            return spawn(job);
        };
        let (send, receive) = oneshot::channel();
        spawner(Box::pin(async move {
            let _ = send.send(job.await);
        }));
        Box::pin(async move {
            receive
                .await
                .map_err(|_| Error::message("IndexedDB operation was dropped"))?
        })
    }
    #[cfg(target_arch = "wasm32")]
    fn spawn<T: 'static>(&self, job: impl Future<Output = Result<T>> + 'static) -> StorageFuture<T> {
        let Some(spawner) = &self.inner.spawner else {
            return spawn(job);
        };
        let (send, receive) = oneshot::channel();
        spawner(Box::pin(async move {
            let _ = send.send(job.await);
        }));
        Box::pin(AssertSend(async move {
            receive
                .await
                .map_err(|_| Error::message("IndexedDB operation was dropped"))?
        }))
    }
    fn assert_open(&self) -> Result<()> {
        if self.inner.state.lock().closed {
            Err(closed())
        } else {
            Ok(())
        }
    }
    /// Create the transaction at call time so overlapping transactions keep call order.
    fn begin(&self, stores: &[&'static str], mode: IdbMode) -> Result<(Active<B>, B::Transaction)> {
        {
            let mut state = self.inner.state.lock();
            if state.closed {
                return Err(closed());
            }
            state.active += 1;
        }
        let active = Active(self.inner.clone());
        let tx = self.inner.backend.transaction(stores, mode)?;
        Ok((active, tx))
    }
    fn run<T, F>(&self, stores: &[&'static str], mode: IdbMode, operation: F) -> StorageFuture<T>
    where
        T: MaybeSend + 'static,
        F: for<'a> FnOnce(&'a B::Transaction) -> IdbFuture<'a, T> + MaybeSend + 'static,
    {
        let (active, tx) = match self.begin(stores, mode) {
            Ok(begun) => begun,
            Err(error) => return failed(error),
        };
        self.spawn(async move {
            let _active = active;
            let result = operation(&tx).await;
            settle(tx, result).await
        })
    }
}

fn record<T: DeserializeOwned>(row: &JsonValue) -> Result<T> {
    wire::field(row, "record").map_err(Error::message)
}
fn records<T: DeserializeOwned>(rows: Vec<JsonValue>) -> Result<Vec<T>> {
    rows.iter().map(record).collect()
}
fn row_field<T: DeserializeOwned>(row: &JsonValue, key: &str) -> Result<T> {
    wire::field(row, key).map_err(Error::message)
}
fn row(fields: impl IntoIterator<Item = (&'static str, Option<JsonValue>)>) -> JsonValue {
    let mut row = wire::object();
    let object = row.as_object_mut().expect("row object");
    for (key, value) in fields {
        if let Some(value) = value {
            object.insert(key.into(), value);
        }
    }
    row
}
fn id_json(id: i64) -> Option<JsonValue> {
    Some(key::number(id as f64))
}
fn flag(value: bool) -> Option<JsonValue> {
    id_json(i64::from(value))
}
/// Rows whose named fields equal the given values; `None` admits every row.
fn filter(fields: Vec<(&'static str, JsonValue)>) -> Option<Accept> {
    (!fields.is_empty()).then(|| -> Accept {
        Box::new(move |row| {
            fields
                .iter()
                .all(|(key, value)| wire::get(row, key) == Some(value))
        })
    })
}
fn direction(order: ScanOrder) -> Direction {
    match order {
        ScanOrder::Ascending => Direction::Next,
        ScanOrder::Descending => Direction::Prev,
    }
}
fn page<T>(mut items: Vec<T>, limit: usize, order: ScanOrder, id: impl Fn(&T) -> i64) -> Page<T> {
    if items.len() <= limit {
        return Page { items, next: None };
    }
    items.truncate(limit);
    let next = items.last().map(|last| scan::next_cursor(id(last), order));
    Page { items, next }
}
fn primary_range(start: &ScanStart) -> Option<IdbKeyRange> {
    let after = start.after?;
    Some(match start.order {
        ScanOrder::Ascending => IdbKeyRange::lower_bound(after.into(), true),
        ScanOrder::Descending => IdbKeyRange::upper_bound(after.into(), true),
    })
}
fn compound(prefix: &[IdbKey], last: impl Into<IdbKey>) -> IdbKey {
    IdbKey::Array(prefix.iter().cloned().chain([last.into()]).collect())
}
fn compound_id_range(
    prefix: &[IdbKey],
    minimum: i64,
    maximum: i64,
    minimum_open: bool,
    maximum_open: bool,
) -> IdbKeyRange {
    IdbKeyRange::bound(
        compound(prefix, minimum),
        compound(prefix, maximum),
        minimum_open,
        maximum_open,
    )
}
async fn scan_compound_rows<X: IdbTransaction>(
    tx: &X,
    source: Source,
    prefix: &[IdbKey],
    start: &ScanStart,
    limit: usize,
    accept: Option<Accept>,
) -> Result<Vec<JsonValue>> {
    let ascending = start.order == ScanOrder::Ascending;
    let minimum = start.after.filter(|_| ascending).unwrap_or(0);
    let maximum = start.after.filter(|_| !ascending).unwrap_or(MAX_KEY);
    tx.values(
        source,
        Some(compound_id_range(
            prefix,
            minimum,
            maximum,
            ascending && start.after.is_some(),
            !ascending && start.after.is_some(),
        )),
        direction(start.order),
        limit,
        accept,
    )
    .await
}
async fn first<X: IdbTransaction>(
    tx: &X,
    source: Source,
    range: IdbKeyRange,
    direction: Direction,
    accept: Option<Accept>,
) -> Result<Option<JsonValue>> {
    Ok(tx
        .values(source, Some(range), direction, 1, accept)
        .await?
        .pop())
}

fn scope_columns(scope: &DocumentScope) -> (&'static str, i64) {
    match scope {
        DocumentScope::Session => ("session", 0),
        DocumentScope::Conversation { conversation_id } => ("conversation", conversation_id.get()),
        DocumentScope::Task { task_id } => ("task", task_id.get()),
    }
}
/// `[kind, scopeKind, ownerId, family, keyValue]`.
fn address_prefix(address: &DocumentAddress) -> Vec<IdbKey> {
    let (scope, owner) = scope_columns(&address.scope);
    vec![
        (&address.kind).into(),
        scope.into(),
        owner.into(),
        i64::from(address.key.is_some()).into(),
        address.key.as_ref().map_or_else(|| "".into(), IdbKey::from),
    ]
}
fn address_range(address: &DocumentAddress) -> IdbKeyRange {
    let prefix = address_prefix(address);
    let bound =
        |n: i64| IdbKey::Array(prefix.iter().cloned().chain([n.into(), n.into()]).collect());
    IdbKeyRange::bound(bound(0), bound(MAX_KEY), false, false)
}
fn alive_at(row: &JsonValue, at: DocumentPoint) -> bool {
    record::<DocumentRecord>(row).is_ok_and(|record| record.alive_at(at))
}

async fn read_conversation<X: IdbTransaction>(
    tx: &X,
    id: ConversationId,
) -> Result<Option<ConversationRecord>> {
    tx.get(CONVERSATIONS, id.get().into())
        .await?
        .as_ref()
        .map(record)
        .transpose()
}
async fn required_conversation<X: IdbTransaction>(
    tx: &X,
    id: ConversationId,
) -> Result<ConversationRecord> {
    read_conversation(tx, id)
        .await?
        .ok_or_else(|| unknown_conversation(id))
}

impl<B: IdbBackend> Storage for IndexedDbStorage<B> {
    fn commit(&self, writes: &[StorageWrite], adopt: Adopt, _: &Context) -> StorageFuture<Seq> {
        if let Err(error) = self.assert_open() {
            return ready(Err(error));
        }
        let actions = match prepare_actions(writes) {
            Ok(actions) => actions,
            Err(error) => return ready(Err(error)),
        };
        let (active, tx) = match self.begin(ALL_STORES, IdbMode::ReadWrite) {
            Ok(begun) => begun,
            Err(error) => return ready(Err(error)),
        };
        let writes = writes.to_vec();
        self.spawn(async move {
            let _active = active;
            let result = commit_in(&tx, &writes, actions).await;
            let seq = settle(tx, result).await?;
            // The transaction completed: the batch is durable.
            adopt(seq);
            Ok(seq)
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        if let Err(error) = self.assert_open() {
            return ready(Err(error));
        }
        self.run(&[META], IdbMode::ReadWrite, |tx| {
            Box::pin(async move {
                let mut metadata = tx
                    .get(META, META_KEY.into())
                    .await?
                    .ok_or_else(missing_metadata)?;
                let next = wire::get(&metadata, "nextId")
                    .and_then(JsonValue::as_f64)
                    .filter(|n| n.fract() == 0.0 && n.abs() <= MAX_KEY as f64)
                    .ok_or_else(|| Error::message("ID space is exhausted"))?
                    as i64;
                wire::put(&mut metadata, "nextId", &(next + 1));
                tx.put(META, metadata).await?;
                Ok(next)
            })
        })
    }
    fn conversation(
        &self,
        id: ConversationId,
        _: &Context,
    ) -> StorageFuture<Option<ConversationRecord>> {
        self.run(&[CONVERSATIONS], IdbMode::ReadOnly, move |tx| {
            Box::pin(read_conversation(tx, id))
        })
    }
    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<ConversationRecord>> {
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Ascending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let query = query.clone();
        self.run(&[CONVERSATIONS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                let accept = || {
                    filter(
                        query
                            .owner_conversation_id
                            .map(|id| ("ownerConversationId", wire::encode(&id)))
                            .into_iter()
                            .collect(),
                    )
                };
                let fetch = limit.saturating_add(1);
                let rows = if let Some(task) = query.owner_task_id {
                    let source = Source::index(CONVERSATIONS, CONVERSATIONS_BY_OWNER_TASK);
                    scan_compound_rows(tx, source, &[task.get().into()], &start, fetch, accept())
                        .await?
                } else if let Some(owner) = query.owner_conversation_id {
                    let source = Source::index(CONVERSATIONS, CONVERSATIONS_BY_OWNER_CONVERSATION);
                    scan_compound_rows(tx, source, &[owner.get().into()], &start, fetch, accept())
                        .await?
                } else {
                    tx.values(
                        Source::store(CONVERSATIONS),
                        primary_range(&start),
                        direction(start.order),
                        fetch,
                        accept(),
                    )
                    .await?
                };
                Ok(page(
                    records(rows)?,
                    limit,
                    start.order,
                    |record: &ConversationRecord| record.id.get(),
                ))
            })
        })
    }
    fn entry(&self, id: EntryId, _: &Context) -> StorageFuture<Option<StoredEntry>> {
        self.run(&[CONVERSATIONS, ENTRIES], IdbMode::ReadOnly, move |tx| {
            Box::pin(read_entry(tx, None, id))
        })
    }
    fn entry_in(
        &self,
        conversation: ConversationId,
        id: EntryId,
        _: &Context,
    ) -> StorageFuture<Option<StoredEntry>> {
        self.run(&[CONVERSATIONS, ENTRIES], IdbMode::ReadOnly, move |tx| {
            Box::pin(read_entry(tx, Some(conversation), id))
        })
    }
    fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        _: &Context,
    ) -> StorageFuture<Option<EntryRecord>> {
        self.run(&[CONVERSATIONS, ENTRIES], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                let mut conversation = required_conversation(tx, conversation).await?;
                let mut upper = at.map_or(MAX_KEY, EntryId::get);
                let source = Source::index(ENTRIES, ENTRY_HEADS_BY_CONVERSATION);
                loop {
                    let range =
                        compound_id_range(&[conversation.id.get().into()], 0, upper, false, false);
                    if let Some(row) = first(tx, source, range, Direction::Prev, None).await? {
                        return record(&row).map(Some);
                    }
                    let Some(parent) = conversation.parent else {
                        return Ok(None);
                    };
                    upper = upper.min(parent.at.get());
                    conversation = required_conversation(tx, parent.conversation_id).await?;
                }
            })
        })
    }
    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Descending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let query = query.clone();
        self.run(&[CONVERSATIONS, ENTRIES], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                let values = match start.order {
                    ScanOrder::Ascending => {
                        read_entries_ascending(tx, &query, limit, start.after).await?
                    }
                    ScanOrder::Descending => {
                        read_entries_descending(tx, &query, limit, start.after).await?
                    }
                };
                Ok(page(values, limit, start.order, |entry: &EntryRecord| {
                    entry.id.get()
                }))
            })
        })
    }
    fn task(&self, id: TaskId, _: &Context) -> StorageFuture<Option<TaskRecord>> {
        self.run(&[TASKS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                tx.get(TASKS, id.get().into())
                    .await?
                    .as_ref()
                    .map(record)
                    .transpose()
            })
        })
    }
    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<TaskRecord>> {
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Ascending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let query = query.clone();
        self.run(&[TASKS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                let mut fields = Vec::new();
                if let Some(id) = query.conversation_id {
                    fields.push(("conversationId", wire::encode(&id)));
                }
                if let Some(kind) = &query.kind {
                    fields.push(("kind", JsonValue::String(kind.clone())));
                }
                if let Some(status) = &query.status {
                    fields.push(("status", wire::encode(status)));
                }
                if let Some(abort) = query.abort_requested {
                    fields.push(("abortRequested", flag(abort).unwrap()));
                }
                if let Some(background) = query.background {
                    fields.push(("background", flag(background).unwrap()));
                }
                let prefix: Option<(&'static str, IdbKey)> = if let Some(status) = &query.status {
                    let status = IdbKey::from_json(&wire::encode(status)).expect("status token");
                    Some((TASKS_BY_STATUS, status))
                } else if let Some(id) = query.conversation_id {
                    Some((TASKS_BY_CONVERSATION, id.get().into()))
                } else if let Some(kind) = &query.kind {
                    Some((TASKS_BY_KIND, kind.into()))
                } else if let Some(abort) = query.abort_requested {
                    Some((TASKS_BY_ABORT_REQUESTED, i64::from(abort).into()))
                } else {
                    query
                        .background
                        .map(|background| (TASKS_BY_BACKGROUND, i64::from(background).into()))
                };
                let fetch = limit.saturating_add(1);
                let rows = match prefix {
                    Some((index, prefix)) => {
                        scan_compound_rows(
                            tx,
                            Source::index(TASKS, index),
                            &[prefix],
                            &start,
                            fetch,
                            filter(fields),
                        )
                        .await?
                    }
                    None => {
                        tx.values(
                            Source::store(TASKS),
                            primary_range(&start),
                            direction(start.order),
                            fetch,
                            filter(fields),
                        )
                        .await?
                    }
                };
                Ok(page(
                    records(rows)?,
                    limit,
                    start.order,
                    |record: &TaskRecord| record.id().get(),
                ))
            })
        })
    }
    fn submission(&self, id: SubmissionId, _: &Context) -> StorageFuture<Option<SubmissionRecord>> {
        self.run(&[SUBMISSIONS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                tx.get(SUBMISSIONS, id.get().into())
                    .await?
                    .as_ref()
                    .map(record)
                    .transpose()
            })
        })
    }
    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<SubmissionRecord>> {
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Ascending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let query = query.clone();
        self.run(&[SUBMISSIONS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                let mut fields = Vec::new();
                if let Some(id) = query.conversation_id {
                    fields.push(("conversationId", wire::encode(&id)));
                }
                if let Some(status) = &query.status {
                    fields.push(("status", wire::encode(status)));
                }
                let fetch = limit.saturating_add(1);
                let rows = if let Some(status) = &query.status {
                    let status = IdbKey::from_json(&wire::encode(status)).expect("status token");
                    let source = Source::index(SUBMISSIONS, SUBMISSIONS_BY_STATUS);
                    scan_compound_rows(tx, source, &[status], &start, fetch, filter(fields)).await?
                } else if let Some(id) = query.conversation_id {
                    let source = Source::index(SUBMISSIONS, SUBMISSIONS_BY_CONVERSATION);
                    scan_compound_rows(
                        tx,
                        source,
                        &[id.get().into()],
                        &start,
                        fetch,
                        filter(fields),
                    )
                    .await?
                } else {
                    tx.values(
                        Source::store(SUBMISSIONS),
                        primary_range(&start),
                        direction(start.order),
                        fetch,
                        filter(fields),
                    )
                    .await?
                };
                Ok(page(
                    records(rows)?,
                    limit,
                    start.order,
                    |record: &SubmissionRecord| record.id().get(),
                ))
            })
        })
    }
    fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &JsonString,
        _: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        let request = request.clone();
        self.run(&[SUBMISSIONS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                tx.index_get(
                    SUBMISSIONS,
                    SUBMISSIONS_BY_REQUEST,
                    IdbKey::Array(vec![conversation.get().into(), (&request).into()]),
                )
                .await?
                .as_ref()
                .map(record)
                .transpose()
            })
        })
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _: &Context,
    ) -> StorageFuture<Option<DocumentRecord>> {
        let range = address_range(address);
        self.run(&[DOCUMENTS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                first(
                    tx,
                    Source::index(DOCUMENTS, DOCUMENTS_BY_ADDRESS),
                    range,
                    Direction::Prev,
                    Some(Box::new(move |row| alive_at(row, at))),
                )
                .await?
                .as_ref()
                .map(record)
                .transpose()
            })
        })
    }
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _: &Context,
    ) -> StorageFuture<Option<StoredDocument>> {
        self.run(
            &[DOCUMENTS, DOCUMENT_REVISIONS],
            IdbMode::ReadOnly,
            move |tx| Box::pin(materialize(tx, id, at)),
        )
    }
    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<DocumentRecord>> {
        let after = match scan::cursor_id(cursor) {
            Ok(after) => after,
            Err(error) => return ready(Err(error)),
        };
        let query = query.clone();
        self.run(&[DOCUMENTS], IdbMode::ReadOnly, move |tx| {
            Box::pin(async move {
                let (scope, owner) = scope_columns(&query.scope);
                let mut prefix: Vec<IdbKey> = vec![scope.into(), owner.into()];
                let index = match &query.kind {
                    None => DOCUMENTS_BY_SCOPE,
                    Some(kind) => {
                        prefix.push(kind.into());
                        DOCUMENTS_BY_SCOPE_KIND
                    }
                };
                let at = query.at;
                let rows = tx
                    .values(
                        Source::index(DOCUMENTS, index),
                        Some(compound_id_range(
                            &prefix,
                            after.unwrap_or(0),
                            MAX_KEY,
                            after.is_some(),
                            false,
                        )),
                        Direction::Next,
                        limit.saturating_add(1),
                        Some(Box::new(move |row| alive_at(row, at))),
                    )
                    .await?;
                Ok(page(
                    records(rows)?,
                    limit,
                    ScanOrder::Ascending,
                    |record: &DocumentRecord| record.id().get(),
                ))
            })
        })
    }
    fn close(&self, _: &Context) -> StorageFuture<()> {
        let mut state = self.inner.state.lock();
        let closing = match &state.closing {
            Some(closing) => closing.clone(),
            None => {
                state.closed = true;
                let drained = (state.active > 0).then(|| {
                    let (send, receive) = oneshot::channel();
                    state.drained.push(send);
                    receive
                });
                let inner = self.inner.clone();
                let closing: IdbFuture<'static, ()> = Box::pin(async move {
                    if let Some(drained) = drained {
                        let _ = drained.await;
                    }
                    inner.backend.close();
                    Ok(())
                });
                let closing = closing.shared();
                state.closing = Some(closing.clone());
                // Close even when nobody waits, as the reference's eager promise does.
                drop(self.spawn(closing.clone()));
                closing
            }
        };
        drop(state);
        self.spawn(closing)
    }
}

async fn read_entry<X: IdbTransaction>(
    tx: &X,
    conversation: Option<ConversationId>,
    id: EntryId,
) -> Result<Option<StoredEntry>> {
    let mut conversation = match conversation {
        Some(conversation) => Some(required_conversation(tx, conversation).await?),
        None => None,
    };
    let Some(row) = tx.get(ENTRIES, id.get().into()).await? else {
        return Ok(None);
    };
    let entry: EntryRecord = record(&row)?;
    if let Some(mut current) = conversation.take() {
        let mut upper = MAX_KEY;
        while current.id != entry.conversation_id {
            let Some(parent) = current.parent else {
                return Ok(None);
            };
            upper = upper.min(parent.at.get());
            current = required_conversation(tx, parent.conversation_id).await?;
        }
        if entry.id.get() > upper {
            return Ok(None);
        }
    }
    Ok(Some(StoredEntry {
        entry,
        commit_seq: row_field(&row, "commitSeq")?,
    }))
}
async fn read_entries_descending<X: IdbTransaction>(
    tx: &X,
    query: &EntryQuery,
    limit: usize,
    after: Option<i64>,
) -> Result<Vec<EntryRecord>> {
    let mut conversation = required_conversation(tx, query.conversation_id).await?;
    let mut upper = query.max_entry_id.map_or(MAX_KEY, EntryId::get);
    if let Some(after) = after {
        upper = upper.min(after - 1);
    }
    let minimum = query.min_entry_id.map_or(0, EntryId::get);
    let mut values = Vec::new();
    let source = Source::index(ENTRIES, ENTRIES_BY_CONVERSATION);
    loop {
        if upper >= minimum {
            let rows = tx
                .values(
                    source,
                    Some(compound_id_range(
                        &[conversation.id.get().into()],
                        minimum,
                        upper,
                        false,
                        false,
                    )),
                    Direction::Prev,
                    limit.saturating_add(1) - values.len(),
                    None,
                )
                .await?;
            values.extend(records::<EntryRecord>(rows)?);
        }
        let Some(parent) = conversation.parent.filter(|_| values.len() <= limit) else {
            break;
        };
        upper = upper.min(parent.at.get());
        if upper < minimum {
            break;
        }
        conversation = required_conversation(tx, parent.conversation_id).await?;
    }
    Ok(values)
}
async fn read_entries_ascending<X: IdbTransaction>(
    tx: &X,
    query: &EntryQuery,
    limit: usize,
    after: Option<i64>,
) -> Result<Vec<EntryRecord>> {
    let mut segments = Vec::new();
    let mut conversation = required_conversation(tx, query.conversation_id).await?;
    let mut upper = query.max_entry_id.map_or(MAX_KEY, EntryId::get);
    let requested_minimum = query.min_entry_id.map_or(0, EntryId::get);
    loop {
        segments.push((conversation.id, upper));
        let Some(parent) = conversation.parent else {
            break;
        };
        upper = upper.min(parent.at.get());
        if upper < requested_minimum {
            break;
        }
        conversation = required_conversation(tx, parent.conversation_id).await?;
    }
    let minimum = after.map_or(requested_minimum, |after| requested_minimum.max(after + 1));
    let mut values = Vec::new();
    let source = Source::index(ENTRIES, ENTRIES_BY_CONVERSATION);
    for (id, upper) in segments.into_iter().rev() {
        if upper < minimum {
            continue;
        }
        let rows = tx
            .values(
                source,
                Some(compound_id_range(
                    &[id.get().into()],
                    minimum,
                    upper,
                    false,
                    false,
                )),
                Direction::Next,
                limit.saturating_add(1) - values.len(),
                None,
            )
            .await?;
        values.extend(records::<EntryRecord>(rows)?);
        if values.len() > limit {
            break;
        }
    }
    Ok(values)
}

async fn materialize<X: IdbTransaction>(
    tx: &X,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>> {
    let Some(row) = tx.get(DOCUMENTS, id.get().into()).await? else {
        return Ok(None);
    };
    let record: DocumentRecord = record(&row)?;
    if at != DocumentPoint::Current && record.create.semantics.current_only() {
        return Err(Error::request(format!(
            "Document {id} does not retain historical content"
        )));
    }
    if !record.alive_at(at) {
        return Ok(None);
    }
    let upper = match at {
        DocumentPoint::Current => MAX_KEY,
        DocumentPoint::At(seq) => seq.get(),
    };
    let base_key = |seq: i64| IdbKey::Array(vec![id.get().into(), "base".into(), seq.into()]);
    let base = first(
        tx,
        Source::index(DOCUMENT_REVISIONS, DOCUMENT_REVISIONS_BY_KIND),
        IdbKeyRange::bound(base_key(0), base_key(upper), false, false),
        Direction::Prev,
        None,
    )
    .await?
    .filter(|base| wire::get(base, "kind").is_some_and(|kind| kind.string_equals("base")))
    .ok_or_else(|| Error::message(format!("Document {id} is missing a required base")))?;
    let version: u64 = row_field(&base, "version")?;
    let base_seq: i64 = row_field(&base, "seq")?;
    let value: JsonObject = row_field(&base, "content")?;
    let tail = if base_seq >= upper {
        Vec::new()
    } else {
        let key = |seq: i64| IdbKey::Array(vec![id.get().into(), seq.into()]);
        tx.values(
            Source::store(DOCUMENT_REVISIONS),
            Some(IdbKeyRange::bound(key(base_seq), key(upper), true, false)),
            Direction::Next,
            usize::MAX,
            None,
        )
        .await?
    };
    let mut batches = Vec::<Vec<Op>>::with_capacity(tail.len());
    for revision in &tail {
        let delta = wire::get(revision, "kind").is_some_and(|kind| kind.string_equals("delta"));
        if !delta || row_field::<u64>(revision, "version")? != version {
            return Err(Error::message(format!(
                "Document {id} crosses a stored version boundary without a base"
            )));
        }
        batches.push(row_field(revision, "content")?);
    }
    let value = apply_immutable_batches(Some(value.value()), batches.iter().map(Vec::as_slice))?;
    Ok(Some(StoredDocument {
        record,
        version,
        value: JsonObject::try_from_value(value).map_err(Error::message)?,
        deltas_since_base: tail.len(),
    }))
}

#[derive(Default)]
struct Action {
    create: Option<DocumentCreate>,
    copy: Option<DocumentCopySource>,
    content: Option<DocumentContent>,
    retire: bool,
}
type Actions = IndexMap<DocumentId, Action>;

fn write_id(write: &StorageWrite) -> Option<(&'static str, i64)> {
    Some(match write {
        StorageWrite::Conversation { value } => ("conversation", value.id.get()),
        StorageWrite::Entry { value } => ("entry", value.id.get()),
        StorageWrite::Task { value } => ("task", value.id().get()),
        StorageWrite::Submission { value } => ("submission", value.id().get()),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            ("document", record.id.get())
        }
        StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => return None,
    })
}
fn more_than_one_content(id: DocumentId) -> Error {
    Error::message(format!("Document {id} has more than one content command"))
}
fn prepare_actions(writes: &[StorageWrite]) -> Result<Actions> {
    let mut actions = Actions::new();
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, content } => {
                let action = actions.entry(record.id).or_default();
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return Err(more_than_one_content(record.id));
                }
                action.create = Some(record.clone());
                action.content = Some(DocumentContent::Base {
                    version: content.version,
                    value: content.value.clone(),
                });
            }
            StorageWrite::DocumentCopy { record, source } => {
                let action = actions.entry(record.id).or_default();
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return Err(more_than_one_content(record.id));
                }
                action.create = Some(record.clone());
                action.copy = Some(source.clone());
            }
            StorageWrite::DocumentChange { id, content } => {
                let action = actions.entry(*id).or_default();
                if action.content.is_some() || action.copy.is_some() {
                    return Err(more_than_one_content(*id));
                }
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentRetire { id } => {
                let action = actions.entry(*id).or_default();
                if action.retire {
                    return Err(Error::message(format!(
                        "Document {id} is retired more than once"
                    )));
                }
                action.retire = true;
            }
            _ => {}
        }
    }
    Ok(actions)
}

async fn commit_in<X: IdbTransaction>(
    tx: &X,
    writes: &[StorageWrite],
    actions: Actions,
) -> Result<Seq> {
    let metadata = tx
        .get(META, META_KEY.into())
        .await?
        .ok_or_else(missing_metadata)?;
    let seq: Seq = row_field(&metadata, "nextSeq")?;
    check_global_ids(tx, writes).await?;
    check_document_actions(tx, &actions).await?;
    for write in writes {
        apply_table_write(tx, write, seq).await?;
    }
    apply_document_actions(tx, actions, seq).await?;
    let next_id = writes
        .iter()
        .filter_map(write_id)
        .map(|(_, id)| id + 1)
        .fold(row_field::<i64>(&metadata, "nextId")?, i64::max);
    tx.put(META, meta_row(next_id, seq.get() + 1)).await?;
    Ok(seq)
}
async fn check_global_ids<X: IdbTransaction>(tx: &X, writes: &[StorageWrite]) -> Result<()> {
    let mut claimed = HashMap::new();
    for (table, id) in writes.iter().filter_map(write_id) {
        let existing = match tx.get(RECORD_IDS, id.into()).await? {
            Some(row) => Some(row_field::<String>(&row, "recordType")?),
            None => None,
        };
        let earlier = claimed.get(&id).copied();
        if matches!(table, "conversation" | "entry" | "document") {
            if let Some(existing) = existing {
                return Err(Error::message(format!(
                    "ID {id} already belongs to {existing}"
                )));
            }
            if earlier.is_some() {
                return Err(Error::message(format!("ID {id} is written more than once")));
            }
        } else {
            if let Some(existing) = existing.filter(|existing| existing != table) {
                return Err(Error::message(format!(
                    "ID {id} already belongs to {existing}"
                )));
            }
            if earlier.is_some_and(|earlier| earlier != table) {
                return Err(Error::message(format!(
                    "ID {id} is written as two record types"
                )));
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}
async fn read_document<X: IdbTransaction>(tx: &X, id: DocumentId) -> Result<Option<JsonValue>> {
    tx.get(DOCUMENTS, id.get().into()).await
}
async fn current_document_id<X: IdbTransaction>(
    tx: &X,
    address: &DocumentAddress,
) -> Result<Option<JsonValue>> {
    first(
        tx,
        Source::index(DOCUMENTS, DOCUMENTS_BY_ADDRESS),
        address_range(address),
        Direction::Prev,
        Some(Box::new(|row| wire::get(row, "retiredAt").is_none())),
    )
    .await
}
async fn check_document_actions<X: IdbTransaction>(tx: &X, actions: &Actions) -> Result<()> {
    let mut live_counts = HashMap::<DocumentAddress, i64>::new();
    for (id, action) in actions {
        if let Some(copy) = &action.copy
            && actions.contains_key(&copy.id)
        {
            return Err(Error::message(format!(
                "Document copy {id} source is changed in the copy batch"
            )));
        }
        let existing = read_document(tx, *id)
            .await?
            .as_ref()
            .map(record::<DocumentRecord>)
            .transpose()?;
        if action.create.is_none() && existing.is_none() {
            return Err(Error::message(format!("Unknown document: {id}")));
        }
        if action.create.is_some() && existing.is_some() {
            return Err(Error::message(format!("Document {id} already exists")));
        }
        if existing
            .as_ref()
            .is_some_and(|record| record.retired_at.is_some())
        {
            return Err(Error::message(format!("Document {id} is retired")));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let previous = first(
                tx,
                Source::store(DOCUMENT_REVISIONS),
                compound_id_range(&[id.get().into()], 0, MAX_KEY, false, false),
                Direction::Prev,
                None,
            )
            .await?
            .ok_or_else(|| Error::message(format!("Document {id} delta has no base")))?;
            if row_field::<u64>(&previous, "version")? != *version {
                return Err(Error::message(format!(
                    "Document {id} version transition requires a base"
                )));
            }
        }
        let address = match (&action.create, &existing) {
            (Some(create), _) => create.address(),
            (None, Some(existing)) => existing.create.address(),
            (None, None) => unreachable!("validated above"),
        };
        let mut live = match live_counts.get(&address) {
            Some(live) => *live,
            None => i64::from(current_document_id(tx, &address).await?.is_some()),
        };
        if action.retire && existing.is_some() {
            live -= 1;
        }
        if action.create.is_some() && !action.retire {
            live += 1;
        }
        live_counts.insert(address, live);
    }
    if live_counts.values().any(|live| *live > 1) {
        return Err(Error::message(
            "Document address already has a current incarnation",
        ));
    }
    Ok(())
}
async fn claim_id<X: IdbTransaction>(tx: &X, id: i64, record_type: &str) -> Result<()> {
    tx.put(
        RECORD_IDS,
        row([
            ("id", id_json(id)),
            ("recordType", Some(JsonValue::String(record_type.into()))),
        ]),
    )
    .await
}
async fn apply_table_write<X: IdbTransaction>(
    tx: &X,
    write: &StorageWrite,
    seq: Seq,
) -> Result<()> {
    match write {
        StorageWrite::Conversation { value } => {
            claim_id(tx, value.id.get(), "conversation").await?;
            let owner = value.owner.as_ref();
            tx.add(
                CONVERSATIONS,
                row([
                    ("id", id_json(value.id.get())),
                    (
                        "ownerConversationId",
                        owner.and_then(|owner| id_json(owner.conversation_id.get())),
                    ),
                    (
                        "ownerTaskId",
                        owner.and_then(|owner| id_json(owner.task_id.get())),
                    ),
                    ("record", Some(wire::encode(value))),
                ]),
            )
            .await
        }
        StorageWrite::Entry { value } => {
            claim_id(tx, value.id.get(), "entry").await?;
            let conversation = value.conversation_id.get();
            tx.add(
                ENTRIES,
                row([
                    ("id", id_json(value.id.get())),
                    ("conversationId", id_json(conversation)),
                    (
                        "headConversationId",
                        value.head.and_then(|_| id_json(conversation)),
                    ),
                    ("commitSeq", id_json(seq.get())),
                    ("record", Some(wire::encode(value))),
                ]),
            )
            .await
        }
        StorageWrite::Task { value } => {
            claim_id(tx, value.id().get(), "task").await?;
            tx.put(
                TASKS,
                row([
                    ("id", id_json(value.id().get())),
                    ("conversationId", id_json(value.info.conversation_id.get())),
                    ("kind", Some(JsonValue::String(value.info.kind.clone()))),
                    ("status", Some(wire::encode(&value.state.status()))),
                    ("abortRequested", flag(value.info.abort_requested)),
                    ("background", flag(value.info.background)),
                    ("record", Some(wire::encode(&**value))),
                ]),
            )
            .await
        }
        StorageWrite::Submission { value } => {
            claim_id(tx, value.id().get(), "submission").await?;
            tx.put(
                SUBMISSIONS,
                row([
                    ("id", id_json(value.id().get())),
                    ("conversationId", id_json(value.info.conversation_id.get())),
                    (
                        "requestId",
                        value.info.request_id.clone().map(JsonValue::String),
                    ),
                    ("status", Some(wire::encode(&value.state.status()))),
                    ("record", Some(wire::encode(value))),
                ]),
            )
            .await
        }
        StorageWrite::DocumentCreate { .. }
        | StorageWrite::DocumentCopy { .. }
        | StorageWrite::DocumentChange { .. }
        | StorageWrite::DocumentRetire { .. } => Ok(()),
    }
}
fn document_row(record: &DocumentRecord) -> JsonValue {
    let (scope, owner) = scope_columns(&record.create.semantics.scope());
    let key = record.create.key.as_ref();
    row([
        ("id", id_json(record.id().get())),
        ("kind", Some(JsonValue::String(record.create.kind.clone()))),
        ("scopeKind", Some(JsonValue::String(scope.into()))),
        ("ownerId", id_json(owner)),
        ("family", flag(key.is_some())),
        (
            "keyValue",
            Some(JsonValue::String(key.cloned().unwrap_or_default())),
        ),
        ("createdAt", id_json(record.created_at.get())),
        (
            "retiredAt",
            record.retired_at.and_then(|seq| id_json(seq.get())),
        ),
        ("record", Some(wire::encode(record))),
    ])
}
async fn delete_revisions<X: IdbTransaction>(tx: &X, id: DocumentId) -> Result<()> {
    tx.delete(
        DOCUMENT_REVISIONS,
        compound_id_range(&[id.get().into()], 0, MAX_KEY, false, false),
    )
    .await
}
async fn copied_content<X: IdbTransaction>(
    tx: &X,
    copy: &DocumentCopySource,
    create: &DocumentCreate,
) -> Result<DocumentContent> {
    let stored = materialize(tx, copy.id, copy.at).await?.ok_or_else(|| {
        Error::message(format!("Fork source document {} cannot be read", copy.id))
    })?;
    let compatible = match (&stored.record.create.semantics, &create.semantics) {
        (
            DocumentSemantics::Conversation { history: a, .. },
            DocumentSemantics::Conversation { history: b, .. },
        ) => a == b,
        _ => false,
    };
    if !compatible
        || stored.record.create.kind != create.kind
        || stored.record.create.key != create.key
    {
        return Err(Error::message(format!(
            "Fork source document {} does not match the copied record",
            copy.id
        )));
    }
    Ok(DocumentContent::Base {
        version: stored.version,
        value: stored.value,
    })
}
async fn apply_document_actions<X: IdbTransaction>(
    tx: &X,
    actions: Actions,
    seq: Seq,
) -> Result<()> {
    for (id, action) in actions {
        let mut content = action.content;
        if let Some(copy) = &action.copy {
            let create = action.create.as_ref().expect("a copy creates its record");
            content = Some(
                copied_content(tx, copy, create)
                    .await
                    .map_err(|cause| Error::document_copy(id, cause))?,
            );
        }
        let created = action.create.is_some();
        let mut record = match action.create {
            Some(create) => {
                let record = DocumentRecord {
                    create,
                    created_at: seq,
                    retired_at: action.retire.then_some(seq),
                };
                claim_id(tx, id.get(), "document").await?;
                tx.add(DOCUMENTS, document_row(&record)).await?;
                record
            }
            None => {
                let row = read_document(tx, id)
                    .await?
                    .ok_or_else(|| Error::message(format!("Unknown document: {id}")))?;
                record(&row)?
            }
        };
        if let Some(content) = content {
            if matches!(content, DocumentContent::Base { .. })
                && record.create.semantics.current_only()
            {
                delete_revisions(tx, id).await?;
            }
            let (kind, stored) = match &content {
                DocumentContent::Base { value, .. } => ("base", value.value().clone()),
                DocumentContent::Delta { ops, .. } => ("delta", wire::encode(&**ops)),
            };
            tx.add(
                DOCUMENT_REVISIONS,
                row([
                    ("documentId", id_json(id.get())),
                    ("seq", id_json(seq.get())),
                    ("kind", Some(JsonValue::String(kind.into()))),
                    ("version", id_json(content.version() as i64)),
                    ("content", Some(stored)),
                ]),
            )
            .await?;
        }
        if action.retire {
            if !created {
                record.retired_at = Some(seq);
                let mut row = read_document(tx, id)
                    .await?
                    .ok_or_else(|| Error::message(format!("Unknown document: {id}")))?;
                wire::put(&mut row, "retiredAt", &seq);
                row.as_object_mut()
                    .expect("document row")
                    .insert("record".into(), wire::encode(&record));
                tx.put(DOCUMENTS, row).await?;
            }
            if record.create.semantics.current_only() {
                delete_revisions(tx, id).await?;
            }
        }
    }
    Ok(())
}
