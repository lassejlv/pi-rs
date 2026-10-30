//! Real-browser checks of pi-durable's IndexedDB storage, compiled from the crate
//! for wasm32 with the `indexeddb` feature and driven by `driver.mjs`:
//! - `conformance`: the reference storage conformance traces (plain and across
//!   reopen) replayed against `IndexedDbStorage<WebIdbConnection>`;
//! - `browser`: browser-vs-host-model differential scans, transaction semantics
//!   and row interchange with the reference.
mod browser;
mod harness_turn;
mod session;
mod timers;

use pi_durable::{
    storage::{
        Adopt, Storage, StorageFuture, StoredEntry,
        indexeddb::{
            IndexedDbStorage,
            web::{WebIdbConnection, WebIdbFactory},
        },
    },
    testing::replay_case,
    *,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};
use wasm_bindgen::prelude::*;

type Idb = IndexedDbStorage<WebIdbConnection>;

/// wasm32 without atomics is single-threaded; JavaScript handles never move.
struct AssertSend<F>(F);
// SAFETY: see above.
unsafe impl<F> Send for AssertSend<F> {}
// SAFETY: see above.
unsafe impl<F> Sync for AssertSend<F> {}
impl<F: Future> Future for AssertSend<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<F::Output> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|this| &mut this.0) }.poll(cx)
    }
}

fn context() -> Context {
    pi_chord::context::BACKGROUND_CONTEXT.clone()
}

/// `ReopeningStorage` of test/indexeddb-storage.test.ts: close and reopen the
/// database after every commit.
struct ReopeningStorage {
    factory: AssertSend<WebIdbFactory>,
    name: String,
    current: Arc<parking_lot::Mutex<Idb>>,
    closed: AtomicBool,
}
impl ReopeningStorage {
    fn current(&self) -> Idb {
        self.current.lock().clone()
    }
}
impl Storage for ReopeningStorage {
    fn commit(&self, writes: &[StorageWrite], adopt: Adopt, c: &Context) -> StorageFuture<Seq> {
        let current = self.current();
        let commit = current.commit(writes, adopt, c);
        if self.closed.load(Ordering::SeqCst) {
            return commit;
        }
        let factory = self.factory.0.clone();
        let (name, slot, c) = (self.name.clone(), self.current.clone(), c.clone());
        Box::pin(AssertSend(async move {
            let result = commit.await;
            current.close(&c).await?;
            *slot.lock() = IndexedDbStorage::open_with(&factory, &name).await?;
            result
        }))
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.current().mint_id()
    }
    fn conversation(&self, id: ConversationId, c: &Context) -> StorageFuture<Option<ConversationRecord>> {
        self.current().conversation(id, c)
    }
    fn scan_conversations(&self, q: &ConversationQuery, l: usize, k: Option<&Cursor>, c: &Context) -> StorageFuture<Page<ConversationRecord>> {
        self.current().scan_conversations(q, l, k, c)
    }
    fn entry(&self, id: EntryId, c: &Context) -> StorageFuture<Option<StoredEntry>> {
        self.current().entry(id, c)
    }
    fn entry_in(&self, conversation: ConversationId, id: EntryId, c: &Context) -> StorageFuture<Option<StoredEntry>> {
        self.current().entry_in(conversation, id, c)
    }
    fn find_latest_head_marker(&self, conversation: ConversationId, at: Option<EntryId>, c: &Context) -> StorageFuture<Option<EntryRecord>> {
        self.current().find_latest_head_marker(conversation, at, c)
    }
    fn scan_entries(&self, q: &EntryQuery, l: usize, k: Option<&Cursor>, c: &Context) -> StorageFuture<Page<EntryRecord>> {
        self.current().scan_entries(q, l, k, c)
    }
    fn task(&self, id: TaskId, c: &Context) -> StorageFuture<Option<TaskRecord>> {
        self.current().task(id, c)
    }
    fn scan_tasks(&self, q: &TaskQuery, l: usize, k: Option<&Cursor>, c: &Context) -> StorageFuture<Page<TaskRecord>> {
        self.current().scan_tasks(q, l, k, c)
    }
    fn submission(&self, id: SubmissionId, c: &Context) -> StorageFuture<Option<SubmissionRecord>> {
        self.current().submission(id, c)
    }
    fn scan_submissions(&self, q: &SubmissionQuery, l: usize, k: Option<&Cursor>, c: &Context) -> StorageFuture<Page<SubmissionRecord>> {
        self.current().scan_submissions(q, l, k, c)
    }
    fn submission_by_request(&self, conversation: ConversationId, request: &JsonString, c: &Context) -> StorageFuture<Option<SubmissionRecord>> {
        self.current().submission_by_request(conversation, request, c)
    }
    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint, c: &Context) -> StorageFuture<Option<DocumentRecord>> {
        self.current().find_document(address, at, c)
    }
    fn document(&self, id: DocumentId, at: DocumentPoint, c: &Context) -> StorageFuture<Option<StoredDocument>> {
        self.current().document(id, at, c)
    }
    fn scan_documents(&self, q: &DocumentQuery, l: usize, k: Option<&Cursor>, c: &Context) -> StorageFuture<Page<DocumentRecord>> {
        self.current().scan_documents(q, l, k, c)
    }
    fn close(&self, c: &Context) -> StorageFuture<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Box::pin(async { Ok(()) });
        }
        self.current().close(c)
    }
}

async fn run_case(factory: &WebIdbFactory, case: &JsonValue, name: &str, reopen: bool) -> Result<()> {
    factory.delete(name).await?;
    if reopen {
        let created = IndexedDbStorage::open_with(factory, name).await?;
        created.close(&context()).await?;
        let storage = Arc::new(ReopeningStorage {
            factory: AssertSend(factory.clone()),
            name: name.into(),
            current: Arc::new(parking_lot::Mutex::new(IndexedDbStorage::open_with(factory, name).await?)),
            closed: AtomicBool::new(false),
        });
        let result = replay_case(storage.clone(), case).await;
        storage.close(&context()).await?;
        result
    } else {
        let storage = IndexedDbStorage::open_with(factory, name).await?;
        let result = replay_case(Arc::new(storage.clone()), case).await;
        storage.close(&context()).await?;
        result
    }
}

/// Replay every case of a reference conformance trace file; returns
/// `{"passed": n, "failed": [{"name", "error"}]}`.
#[wasm_bindgen]
pub async fn conformance(fixture: String, reopen: bool) -> std::result::Result<String, JsValue> {
    let fixture: JsonValue = serde_json::from_str(&fixture).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let factory = WebIdbFactory::global().map_err(|e| JsValue::from_str(&e.to_string()))?;
    let mut passed = 0;
    let mut failed = Vec::new();
    for (index, case) in fixture["cases"].as_array().into_iter().flatten().enumerate() {
        let name = format!("conformance-{}-{index}", if reopen { "reopen" } else { "plain" });
        match run_case(&factory, case, &name, reopen).await {
            Ok(()) => passed += 1,
            Err(error) => failed.push(serde_json::json!({
                "name": case["name"].to_utf8(),
                "error": error.to_string(),
            })),
        }
    }
    Ok(serde_json::json!({ "passed": passed, "failed": failed }).to_string())
}
