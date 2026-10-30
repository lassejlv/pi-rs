//! Port of `test/indexeddb-storage.test.ts`, run over the host in-memory
//! IndexedDB model (`fake-indexeddb` in the reference).
use pi_durable::{
    storage::{
        Adopt, Storage, StorageFuture, StoredEntry,
        indexeddb::{IndexedDbStorage, MemoryIdbConnection, MemoryIdbFactory},
    },
    testing::replay_case,
    *,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

type Idb = IndexedDbStorage<MemoryIdbConnection>;

/// Storage-level commits here have no in-memory owner to adopt them.
fn adopt() -> Adopt {
    Box::new(|_| {})
}
fn context() -> Context {
    pi_chord::context::BACKGROUND_CONTEXT.clone()
}
async fn open(factory: &MemoryIdbFactory, name: &str) -> Idb {
    IndexedDbStorage::from_backend(factory.open(name))
        .await
        .unwrap()
}
fn write(value: serde_json::Value) -> StorageWrite {
    serde_json::from_value(value).unwrap()
}

/// Closes and reopens the database after every commit, so each later request
/// reads only what reached the object stores.
struct ReopeningStorage {
    factory: MemoryIdbFactory,
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
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        let current = self.current();
        let commit = current.commit(writes, adopt, context);
        if self.closed.load(Ordering::SeqCst) {
            return commit;
        }
        let (factory, name, slot) = (
            self.factory.clone(),
            self.name.clone(),
            self.current.clone(),
        );
        let context = context.clone();
        Box::pin(async move {
            let result = commit.await;
            current.close(&context).await?;
            *slot.lock() = IndexedDbStorage::from_backend(factory.open(&name)).await?;
            result
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.current().mint_id()
    }
    fn conversation(
        &self,
        id: ConversationId,
        c: &Context,
    ) -> StorageFuture<Option<ConversationRecord>> {
        self.current().conversation(id, c)
    }
    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        c: &Context,
    ) -> StorageFuture<Page<ConversationRecord>> {
        self.current().scan_conversations(query, limit, cursor, c)
    }
    fn entry(&self, id: EntryId, c: &Context) -> StorageFuture<Option<StoredEntry>> {
        self.current().entry(id, c)
    }
    fn entry_in(
        &self,
        conversation: ConversationId,
        id: EntryId,
        c: &Context,
    ) -> StorageFuture<Option<StoredEntry>> {
        self.current().entry_in(conversation, id, c)
    }
    fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        c: &Context,
    ) -> StorageFuture<Option<EntryRecord>> {
        self.current().find_latest_head_marker(conversation, at, c)
    }
    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        c: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        self.current().scan_entries(query, limit, cursor, c)
    }
    fn task(&self, id: TaskId, c: &Context) -> StorageFuture<Option<TaskRecord>> {
        self.current().task(id, c)
    }
    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        c: &Context,
    ) -> StorageFuture<Page<TaskRecord>> {
        self.current().scan_tasks(query, limit, cursor, c)
    }
    fn submission(&self, id: SubmissionId, c: &Context) -> StorageFuture<Option<SubmissionRecord>> {
        self.current().submission(id, c)
    }
    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        c: &Context,
    ) -> StorageFuture<Page<SubmissionRecord>> {
        self.current().scan_submissions(query, limit, cursor, c)
    }
    fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &JsonString,
        c: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        self.current()
            .submission_by_request(conversation, request, c)
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        c: &Context,
    ) -> StorageFuture<Option<DocumentRecord>> {
        self.current().find_document(address, at, c)
    }
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        c: &Context,
    ) -> StorageFuture<Option<StoredDocument>> {
        self.current().document(id, at, c)
    }
    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        c: &Context,
    ) -> StorageFuture<Page<DocumentRecord>> {
        self.current().scan_documents(query, limit, cursor, c)
    }
    fn close(&self, c: &Context) -> StorageFuture<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Box::pin(async { Ok(()) });
        }
        self.current().close(c)
    }
}

fn fixture_cases(text: &str) -> Vec<JsonValue> {
    let fixture: JsonValue = serde_json::from_str(text).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

// registerStorageConformance(..., "IndexedDbStorage", withStorage); traces of
// the reference IndexedDbStorage over fake-indexeddb (fixtures/indexeddb/generate.mjs).
mod conformance {
    use super::*;
    fn cases() -> &'static Vec<JsonValue> {
        static CASES: std::sync::OnceLock<Vec<JsonValue>> = std::sync::OnceLock::new();
        CASES.get_or_init(|| fixture_cases(include_str!("fixtures/storage-indexeddb.json")))
    }
    macro_rules! storage_case {
        ($name:ident, $index:expr) => {
            #[tokio::test]
            async fn $name() {
                let factory = MemoryIdbFactory::new();
                let storage = open(&factory, "conformance").await;
                let case = &cases()[$index];
                let result = replay_case(Arc::new(storage.clone()), case).await;
                storage.close(&context()).await.unwrap();
                result.unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
            }
        };
    }
    include!("fixtures/storage_indexeddb_cases.rs");
}

// registerStorageConformance(..., "IndexedDbStorage across reopen", ...)
mod conformance_across_reopen {
    use super::*;
    fn cases() -> &'static Vec<JsonValue> {
        static CASES: std::sync::OnceLock<Vec<JsonValue>> = std::sync::OnceLock::new();
        CASES.get_or_init(|| fixture_cases(include_str!("fixtures/storage-indexeddb-reopen.json")))
    }
    macro_rules! storage_case {
        ($name:ident, $index:expr) => {
            #[tokio::test]
            async fn $name() {
                let factory = MemoryIdbFactory::new();
                let name = "reopening-conformance";
                let created = open(&factory, name).await;
                created.close(&context()).await.unwrap();
                let storage = Arc::new(ReopeningStorage {
                    current: Arc::new(parking_lot::Mutex::new(open(&factory, name).await)),
                    factory,
                    name: name.into(),
                    closed: AtomicBool::new(false),
                });
                let case = &cases()[$index];
                let result = replay_case(storage.clone(), case).await;
                storage.close(&context()).await.unwrap();
                result.unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
            }
        };
    }
    include!("fixtures/storage_indexeddb_reopen_cases.rs");
}

fn pending_task(id: i64, conversation_id: i64) -> serde_json::Value {
    json!({
        "id": id,
        "conversationId": conversation_id,
        "kind": "test.task",
        "version": 1,
        "input": null,
        "state": { "status": "pending", "checkpoint": { "phase": "ready" } },
        "background": false,
        "abortRequested": false,
    })
}
async fn create_root(storage: &Idb) {
    storage
        .commit(
            &[write(
                json!({ "type": "conversation", "value": { "id": 1 } }),
            )],
            adopt(),
            &context(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn persists_minted_ids_across_reopen() {
    let factory = MemoryIdbFactory::new();
    let storage = open(&factory, "mint-reopen").await;
    assert_eq!(storage.mint_id().await.unwrap(), 2);
    storage.close(&context()).await.unwrap();
    let reopened = open(&factory, "mint-reopen").await;
    assert_eq!(reopened.mint_id().await.unwrap(), 3);
    reopened.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rolls_back_table_writes_when_a_later_document_copy_fails() {
    let factory = MemoryIdbFactory::new();
    let storage = open(&factory, "atomic-copy").await;
    create_root(&storage).await;
    let source_id = storage.mint_id().await.unwrap();
    let source = json!({
        "id": source_id,
        "kind": "source",
        "scope": { "kind": "conversation", "conversationId": 1 },
        "history": "rewindable",
        "fork": "asOf",
    });
    storage
        .commit(
            &[write(json!({
                "type": "document.create",
                "record": source,
                "content": { "kind": "base", "version": 1, "value": { "count": 1 } },
            }))],
            adopt(),
            &context(),
        )
        .await
        .unwrap();
    let task_id = storage.mint_id().await.unwrap();
    let copy_id = storage.mint_id().await.unwrap();
    let mut copy = source.clone();
    copy["id"] = json!(copy_id);
    copy["kind"] = json!("mismatch");
    let error = storage
        .commit(
            &[
                write(json!({ "type": "task", "value": pending_task(task_id, 1) })),
                write(json!({
                    "type": "document.copy",
                    "record": copy,
                    "source": { "id": source_id, "at": "current" },
                })),
            ],
            adopt(),
            &context(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("was rejected"), "{error}");
    assert!(
        storage
            .task(TaskId::from_raw(task_id), &context())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        storage
            .document(
                DocumentId::from_raw(copy_id),
                DocumentPoint::Current,
                &context()
            )
            .await
            .unwrap()
            .is_none()
    );
    storage.close(&context()).await.unwrap();
}

#[tokio::test]
async fn scans_entries_and_head_markers_through_fork_ancestry() {
    let factory = MemoryIdbFactory::new();
    let storage = open(&factory, "fork-ancestry").await;
    create_root(&storage).await;
    let root_first = storage.mint_id().await.unwrap();
    let root_marker = storage.mint_id().await.unwrap();
    storage
        .commit(
            &[
                write(json!({ "type": "entry", "value": { "id": root_first, "conversationId": 1, "kind": "first" } })),
                write(json!({ "type": "entry", "value": {
                    "id": root_marker, "conversationId": 1, "kind": "marker", "head": root_first,
                } })),
            ],
            adopt(),
            &context(),
        )
        .await
        .unwrap();
    let child_id = storage.mint_id().await.unwrap();
    storage
        .commit(
            &[write(json!({ "type": "conversation", "value": {
                "id": child_id, "parent": { "conversationId": 1, "at": root_marker },
            } }))],
            adopt(),
            &context(),
        )
        .await
        .unwrap();
    let child_entry = storage.mint_id().await.unwrap();
    storage
        .commit(
            &[write(json!({ "type": "entry", "value": { "id": child_entry, "conversationId": child_id, "kind": "child" } }))],
            adopt(),
            &context(),
        )
        .await
        .unwrap();
    let query: EntryQuery = serde_json::from_value(json!({ "conversationId": child_id })).unwrap();
    let ids: Vec<i64> = storage
        .scan_entries(&query, 10, None, &context())
        .await
        .unwrap()
        .items
        .iter()
        .map(|entry| entry.id.get())
        .collect();
    assert_eq!(ids, [child_entry, root_marker, root_first]);
    let marker = storage
        .find_latest_head_marker(ConversationId::from_raw(child_id), None, &context())
        .await
        .unwrap();
    assert_eq!(marker.map(|entry| entry.id.get()), Some(root_marker));
    storage.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_one_readwrite_transaction_active_through_many_mixed_writes() {
    let factory = MemoryIdbFactory::new();
    let storage = open(&factory, "large-commit").await;
    create_root(&storage).await;
    let mut writes = Vec::new();
    let mut last = (0, 0, 0, 0);
    for index in 0..40 {
        last = (
            storage.mint_id().await.unwrap(),
            storage.mint_id().await.unwrap(),
            storage.mint_id().await.unwrap(),
            storage.mint_id().await.unwrap(),
        );
        let (entry, task, submission, document) = last;
        writes.extend([
            write(json!({ "type": "entry", "value": {
                "id": entry, "conversationId": 1, "kind": "bulk", "data": { "index": index },
            } })),
            write(json!({ "type": "task", "value": pending_task(task, 1) })),
            write(json!({ "type": "submission", "value": {
                "id": submission, "conversationId": 1, "requestId": format!("bulk-{index}"),
                "type": "input", "status": "queued",
            } })),
            write(json!({
                "type": "document.create",
                "record": { "id": document, "kind": "bulk", "key": index.to_string(), "scope": { "kind": "session" } },
                "content": { "kind": "base", "version": 1, "value": { "index": index } },
            })),
        ]);
    }
    storage.commit(&writes, adopt(), &context()).await.unwrap();
    let (entry, task, submission, document) = last;
    let stored = storage
        .entry(EntryId::from_raw(entry), &context())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pi_chord::json::to_value(&stored.entry.data).unwrap(),
        pi_chord::json::parse(r#"{"index":39}"#).unwrap()
    );
    let expected: TaskRecord = serde_json::from_value(pending_task(task, 1)).unwrap();
    assert_eq!(
        storage
            .task(TaskId::from_raw(task), &context())
            .await
            .unwrap(),
        Some(expected)
    );
    let stored = storage
        .submission(SubmissionId::from_raw(submission), &context())
        .await
        .unwrap()
        .unwrap();
    assert!(stored.info.request_id.unwrap().equals_str("bulk-39"));
    let stored = storage
        .document(
            DocumentId::from_raw(document),
            DocumentPoint::Current,
            &context(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.value.value(),
        &pi_chord::json::parse(r#"{"index":39}"#).unwrap()
    );
    storage.close(&context()).await.unwrap();
}

// Interchange with the reference implementation's on-disk format: the same
// requests must leave identical rows in every object store, and a database the
// reference wrote must read and continue identically here.
mod interchange {
    use super::*;
    use pi_durable::storage::indexeddb::{KeyPath, schema};

    fn fixture() -> &'static JsonValue {
        static FIXTURE: std::sync::OnceLock<JsonValue> = std::sync::OnceLock::new();
        FIXTURE.get_or_init(|| {
            serde_json::from_str(include_str!("fixtures/indexeddb-interchange.json")).unwrap()
        })
    }
    fn phase(name: &str) -> JsonValue {
        let mut case = pi_chord::json::parse("{}").unwrap();
        case.as_object_mut()
            .unwrap()
            .insert("requests".into(), fixture()["phases"][name].clone());
        case
    }
    async fn replay(storage: Arc<dyn Storage>, name: &str) {
        replay_case(storage, &phase(name))
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    fn assert_rows(factory: &MemoryIdbFactory, name: &str, expected: &JsonValue) {
        let expected = expected.as_object().unwrap();
        assert_eq!(expected.len(), schema::SCHEMA.len());
        for store in schema::SCHEMA {
            let rows = factory.rows(name, store.name);
            let reference = expected[&JsonString::from(store.name)]["rows"]
                .as_array()
                .unwrap();
            assert_eq!(rows.len(), reference.len(), "{} row count", store.name);
            for (row, reference) in rows.iter().zip(reference) {
                assert_eq!(row, reference, "{} row", store.name);
            }
        }
    }
    fn load(factory: &MemoryIdbFactory, name: &str, stores: &JsonValue) {
        for (store, value) in stores.as_object().unwrap() {
            factory
                .load(
                    name,
                    &store.to_string_lossy(),
                    value["rows"].as_array().unwrap().clone(),
                )
                .unwrap();
        }
    }

    #[test]
    fn declares_the_reference_object_stores_and_indexes() {
        let key_path = |path: KeyPath| match path {
            KeyPath::Single(name) => serde_json::json!(name),
            KeyPath::Compound(names) => serde_json::json!(names),
        };
        let stores = &fixture()["stores"];
        for store in schema::SCHEMA {
            let reference = &stores[store.name];
            let json = |value: &JsonValue| -> serde_json::Value {
                serde_json::from_str(&value.to_string()).unwrap()
            };
            assert_eq!(
                json(&reference["keyPath"]),
                key_path(store.key_path),
                "{}",
                store.name
            );
            let indexes = json(&reference["indexes"]);
            let indexes = indexes.as_object().unwrap();
            assert_eq!(indexes.len(), store.indexes.len(), "{} indexes", store.name);
            for index in store.indexes {
                assert_eq!(
                    indexes[index.name],
                    key_path(index.key_path),
                    "{}",
                    index.name
                );
            }
        }
    }

    #[tokio::test]
    async fn writes_the_rows_the_reference_writes() {
        let factory = MemoryIdbFactory::new();
        let storage = Arc::new(open(&factory, "interchange").await);
        replay(storage, "write").await;
        assert_rows(&factory, "interchange", &fixture()["stores"]);
        let storage = Arc::new(open(&factory, "interchange").await);
        replay(storage.clone(), "read").await;
        replay(storage, "continue").await;
        assert_rows(&factory, "interchange", &fixture()["finalStores"]);
    }

    #[tokio::test]
    async fn reads_and_continues_a_database_the_reference_wrote() {
        let factory = MemoryIdbFactory::new();
        load(&factory, "reference", &fixture()["stores"]);
        let storage = Arc::new(open(&factory, "reference").await);
        replay(storage.clone(), "read").await;
        replay(storage, "continue").await;
        assert_rows(&factory, "reference", &fixture()["finalStores"]);
    }

    #[tokio::test]
    async fn rejects_a_database_without_durable_metadata() {
        let factory = MemoryIdbFactory::new();
        factory.load("empty", schema::META, Vec::new()).unwrap();
        let error = IndexedDbStorage::from_backend(factory.open("empty"))
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Durable IndexedDB metadata is missing");
    }
}
