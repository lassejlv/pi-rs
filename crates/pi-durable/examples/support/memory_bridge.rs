//! Native memory and SQLite storage for stock task tests. Decorated stores keep
//! their original callbacks; plain stores share their backend with the Session.
use super::*;
use pi_durable::storage::StoredEntry;

#[derive(Default)]
pub(super) struct MemoryBridge {
    stores: Mutex<HashMap<u64, Arc<ObservedStorage>>>,
    /// `MemoryStorage.prepareCommit()` results, until applied.
    prepared: Mutex<HashMap<u64, Arc<pi_durable::storage::memory::PreparedMemoryCommit>>>,
    sqlite_stores: Mutex<HashMap<u64, Arc<ObservedStorage>>>,
    pub sqlite_opens: AtomicU64,
    pub sqlite_direct_opens: AtomicU64,
    pub sqlite_calls: Arc<AtomicU64>,
    pub sqlite_commits: Arc<AtomicU64>,
    pub direct_opens: AtomicU64,
    pub calls: Arc<AtomicU64>,
    pub commits: Arc<AtomicU64>,
}
struct ObservedStorage {
    backend: Arc<dyn Storage>,
    /// The concrete store behind `backend`, for memory stores (close state).
    memory: Option<Arc<MemoryStorage>>,
    calls: Arc<AtomicU64>,
    commits: Arc<AtomicU64>,
    decorated: Arc<AtomicBool>,
}
macro_rules! forward {
    ($name:ident($($arg:ident:$ty:ty),*) -> $out:ty) => {
        fn $name(&self,$($arg:$ty,)*context:&Context)->StorageFuture<$out> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.backend.$name($($arg,)* context)
        }
    };
}
impl Storage for ObservedStorage {
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.commits.fetch_add(1, Ordering::Relaxed);
        self.backend.commit(writes, adopt, context)
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.backend.mint_id()
    }
    forward!(conversation(id:ConversationId) -> Option<ConversationRecord>);
    forward!(scan_conversations(query:&ConversationQuery,limit:usize,cursor:Option<&Cursor>) -> Page<ConversationRecord>);
    forward!(entry(id:EntryId) -> Option<StoredEntry>);
    forward!(entry_in(conversation:ConversationId,id:EntryId) -> Option<StoredEntry>);
    forward!(find_latest_head_marker(conversation:ConversationId,at:Option<EntryId>) -> Option<EntryRecord>);
    forward!(scan_entries(query:&EntryQuery,limit:usize,cursor:Option<&Cursor>) -> Page<EntryRecord>);
    forward!(task(id:TaskId) -> Option<TaskRecord>);
    forward!(scan_tasks(query:&TaskQuery,limit:usize,cursor:Option<&Cursor>) -> Page<TaskRecord>);
    forward!(submission(id:SubmissionId) -> Option<SubmissionRecord>);
    forward!(scan_submissions(query:&SubmissionQuery,limit:usize,cursor:Option<&Cursor>) -> Page<SubmissionRecord>);
    forward!(submission_by_request(conversation:ConversationId,request:&JsonString) -> Option<SubmissionRecord>);
    forward!(find_document(address:&DocumentAddress,at:DocumentPoint) -> Option<DocumentRecord>);
    forward!(document(id:DocumentId,at:DocumentPoint) -> Option<StoredDocument>);
    forward!(scan_documents(query:&DocumentQuery,limit:usize,cursor:Option<&Cursor>) -> Page<DocumentRecord>);
    forward!(close() -> ());
}
struct MutableHostStorage {
    native: Arc<dyn Storage>,
    host: Arc<dyn Storage>,
    decorated: Arc<AtomicBool>,
}
impl MutableHostStorage {
    fn backend(&self) -> &dyn Storage {
        if self.decorated.load(Ordering::Acquire) {
            self.host.as_ref()
        } else {
            self.native.as_ref()
        }
    }
}
macro_rules! route {
    ($name:ident($($arg:ident:$ty:ty),*) -> $out:ty) => {
        fn $name(&self,$($arg:$ty,)*context:&Context)->StorageFuture<$out> {
            self.backend().$name($($arg,)* context)
        }
    };
}
impl Storage for MutableHostStorage {
    fn mint_id(&self) -> StorageFuture<i64> {
        self.backend().mint_id()
    }
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        self.backend().commit(writes, adopt, context)
    }
    route!(conversation(id:ConversationId) -> Option<ConversationRecord>);
    route!(scan_conversations(query:&ConversationQuery,limit:usize,cursor:Option<&Cursor>) -> Page<ConversationRecord>);
    route!(entry(id:EntryId) -> Option<StoredEntry>);
    route!(entry_in(conversation:ConversationId,id:EntryId) -> Option<StoredEntry>);
    route!(find_latest_head_marker(conversation:ConversationId,at:Option<EntryId>) -> Option<EntryRecord>);
    route!(scan_entries(query:&EntryQuery,limit:usize,cursor:Option<&Cursor>) -> Page<EntryRecord>);
    route!(task(id:TaskId) -> Option<TaskRecord>);
    route!(scan_tasks(query:&TaskQuery,limit:usize,cursor:Option<&Cursor>) -> Page<TaskRecord>);
    route!(submission(id:SubmissionId) -> Option<SubmissionRecord>);
    route!(scan_submissions(query:&SubmissionQuery,limit:usize,cursor:Option<&Cursor>) -> Page<SubmissionRecord>);
    route!(submission_by_request(conversation:ConversationId,request:&JsonString) -> Option<SubmissionRecord>);
    route!(find_document(address:&DocumentAddress,at:DocumentPoint) -> Option<DocumentRecord>);
    route!(document(id:DocumentId,at:DocumentPoint) -> Option<StoredDocument>);
    route!(scan_documents(query:&DocumentQuery,limit:usize,cursor:Option<&Cursor>) -> Page<DocumentRecord>);
    route!(close() -> ());
}
impl Hub {
    pub(super) fn memory_storage(&self, id: u64) -> Result<Arc<dyn Storage>> {
        self.memory
            .stores
            .lock()
            .get(&id)
            .cloned()
            .map(|store| store as Arc<dyn Storage>)
            .ok_or_else(|| Error::message("Unknown native memory store"))
    }
    pub(super) fn sqlite_storage(&self, id: u64) -> Result<Arc<dyn Storage>> {
        self.memory
            .sqlite_stores
            .lock()
            .get(&id)
            .cloned()
            .map(|store| store as Arc<dyn Storage>)
            .ok_or_else(|| Error::message("Unknown native SQLite store"))
    }
    pub(super) fn sqlite_for_harness(
        self: &Arc<Self>,
        id: u64,
        handle: u64,
    ) -> Result<Arc<dyn Storage>> {
        let store = self
            .memory
            .sqlite_stores
            .lock()
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::message("Unknown native SQLite store"))?;
        Ok(Arc::new(MutableHostStorage {
            decorated: store.decorated.clone(),
            native: store,
            host: Arc::new(remote_storage::RemoteStorage {
                hub: self.clone(),
                handle,
            }),
        }))
    }
    pub(super) fn memory_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        if method == "sqlite.decorate" {
            let id = field(&request, "sqlite")?;
            self.memory
                .sqlite_stores
                .lock()
                .get(&id)
                .ok_or_else(|| Error::message("Unknown native SQLite store"))?
                .decorated
                .store(true, Ordering::Release);
            return Ok(Box::pin(async { Ok(None) }));
        }
        if method == "memory.apply" {
            let prepared = self
                .memory
                .prepared
                .lock()
                .get(&field::<u64>(&request, "prepared")?)
                .cloned()
                .ok_or_else(|| Error::message("Unknown prepared memory commit"))?;
            let seq = prepared.apply()?;
            return Ok(Box::pin(async move { Ok(Some(encode(&seq))) }));
        }
        if method == "memory.prepare" {
            let id: u64 = field(&request, "memory")?;
            let memory = self
                .memory
                .stores
                .lock()
                .get(&id)
                .and_then(|store| store.memory.clone())
                .ok_or_else(|| Error::message("Unknown native memory store"))?;
            let writes: Vec<StorageWrite> = field(&request, "writes")?;
            let seq: Option<Seq> = field(&request, "seq")?;
            let prepared = Arc::new(memory.prepare_commit(&writes, seq)?);
            let handle = self.next.fetch_add(1, Ordering::Relaxed);
            let value = object([
                ("prepared", encode(&handle)),
                ("seq", encode(&prepared.seq())),
                ("writes", encode(&prepared.writes())),
            ]);
            self.memory.prepared.lock().insert(handle, prepared);
            return Ok(Box::pin(async move { Ok(Some(value)) }));
        }
        if method == "memory.closed" || method == "memory.setClosed" {
            let id: u64 = field(&request, "memory")?;
            let memory = self
                .memory
                .stores
                .lock()
                .get(&id)
                .and_then(|store| store.memory.clone())
                .ok_or_else(|| Error::message("Unknown native memory store"))?;
            if method == "memory.setClosed" {
                memory.set_closed(request["closed"].as_bool() == Some(true));
                return Ok(Box::pin(async { Ok(None) }));
            }
            let closed = memory.is_closed();
            return Ok(Box::pin(async move { Ok(Some(encode(&closed))) }));
        }
        if method == "memory.new" {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let memory = Arc::new(MemoryStorage::new());
            self.memory.stores.lock().insert(
                id,
                Arc::new(ObservedStorage {
                    backend: memory.clone(),
                    memory: Some(memory),
                    calls: self.memory.calls.clone(),
                    commits: self.memory.commits.clone(),
                    decorated: Arc::new(AtomicBool::new(false)),
                }),
            );
            return Ok(Box::pin(async move { Ok(Some(encode(&id))) }));
        }
        if method == "sqlite.new" {
            use pi_durable::storage::sqlite::{
                NativeSqliteDatabaseOptions, NativeSqliteStorageOptions, open_native_sqlite_storage,
            };
            let hub = self.clone();
            return Ok(Box::pin(async move {
                let backend = open_native_sqlite_storage(
                    field::<String>(&request, "path")?,
                    NativeSqliteStorageOptions {
                        prefix: field(&request["options"], "prefix")?,
                        database: NativeSqliteDatabaseOptions {
                            wal_auto_checkpoint_pages: request["options"]["walAutoCheckpointPages"]
                                .as_u64()
                                .unwrap_or(1000)
                                as u32,
                            busy_timeout_ms: request["options"]["busyTimeoutMs"]
                                .as_u64()
                                .unwrap_or(5000),
                        },
                    },
                )
                .await?;
                let id = hub.next.fetch_add(1, Ordering::Relaxed);
                hub.memory.sqlite_stores.lock().insert(
                    id,
                    Arc::new(ObservedStorage {
                        backend: Arc::new(backend),
                        memory: None,
                        calls: hub.memory.sqlite_calls.clone(),
                        commits: hub.memory.sqlite_commits.clone(),
                        decorated: Arc::new(AtomicBool::new(false)),
                    }),
                );
                hub.memory.sqlite_opens.fetch_add(1, Ordering::Relaxed);
                Ok(Some(encode(&id)))
            }));
        }
        let storage = if method == "sqlite.call" {
            self.sqlite_storage(field(&request, "sqlite")?)?
        } else {
            self.memory_storage(field(&request, "memory")?)?
        };
        let callback: Option<u64> = field(&request, "adopt")?;
        let weak = Arc::downgrade(self);
        let adoption_error = Arc::new(Mutex::new(None));
        let capture = adoption_error.clone();
        let operation = pi_durable::testing::storage_request(
            storage.as_ref(),
            &field::<String>(&request, "operation")?,
            &field::<Vec<JsonValue>>(&request, "args")?,
            Box::new(move |seq| {
                if let (Some(hub), Some(callback)) = (weak.upgrade(), callback)
                    && let Err(error) = hub.sync(
                        "memory.adopt",
                        object([("callback", encode(&callback)), ("seq", encode(&seq))]),
                    )
                {
                    *capture.lock() = Some(error);
                }
            }),
        );
        Ok(Box::pin(async move {
            let result = operation.await;
            if let Some(error) = adoption_error.lock().take() {
                return Err(error);
            }
            result
        }))
    }
}
impl Hub {
    /// `memory.call` run to completion on the calling thread (the embedded JS thread), as
    /// upstream's MemoryStorage completes each call when it is made. The adoption is not called
    /// back: the result frame carries `adopted` (the sequence the store adopted), and the caller
    /// runs its own `adopt(seq)` before settling, as upstream's commit does.
    #[allow(dead_code)] // Used by the in-process transport only.
    pub(crate) fn memory_call_now(self: &Arc<Self>, request: &JsonValue) -> JsonValue {
        let adopted = Arc::new(Mutex::new(None::<Seq>));
        let capture = adopted.clone();
        let result = (|| -> Result<Option<JsonValue>> {
            let storage = self.memory_storage(field(request, "memory")?)?;
            let mut operation = pi_durable::testing::storage_request(
                storage.as_ref(),
                &field::<String>(request, "operation")?,
                &field::<Vec<JsonValue>>(request, "args")?,
                Box::new(move |seq| *capture.lock() = Some(seq)),
            );
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match operation.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => Err(Error::message(
                    "Native memory storage call did not complete",
                )),
            }
        })();
        let mut frame = match result {
            Ok(Some(value)) => object([("value", value)]),
            Ok(None) => object([("missing", encode(&true))]),
            Err(error) => object([("error", error_value(&error))]),
        };
        if let Some(seq) = *adopted.lock() {
            frame
                .as_object_mut()
                .unwrap()
                .insert("adopted".into(), encode(&seq));
        }
        frame
    }
}
