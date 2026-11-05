use super::{forks::prepare_fork_copies, types::*};
use crate::{
    storage::{StorageFuture, ready},
    *,
};
use futures::{
    FutureExt,
    future::{Shared, join_all},
};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::delta::{Change, Prepared, Tracker, track};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct LoadedDocument {
    pub record: DocumentRecord,
    pub stored_version: u64,
    pub value_version: u64,
    pub deltas_since_base: usize,
    pub tracker: Arc<Mutex<Tracker>>,
    pub typed: HashMap<std::any::TypeId, Arc<dyn std::any::Any + Send + Sync>>,
}
pub(crate) type Loaded = Arc<Mutex<LoadedDocument>>;
pub(crate) trait TransactionHost: Send + Sync {
    #[cfg(feature = "stock-test-probes")]
    fn transaction_probe(&self, point: TransactionProbePoint) -> Result<()>;
    fn storage(&self) -> Arc<dyn Storage>;
    fn now(&self) -> f64;
    fn cached(&self, address: &str) -> Option<Loaded>;
    fn load(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
        context: Context,
    ) -> StorageFuture<Option<Loaded>>;
    fn install(&self, address: String, document: Loaded);
    fn evict(&self, address: &str, id: DocumentId);
    fn conversation_created(
        &self,
        tx: Transaction,
        record: ConversationRecord,
    ) -> StorageFuture<()>;
}

struct DraftCell {
    change: Mutex<Change>,
    sealed: Arc<AtomicBool>,
}
/// A revocable transaction-local document. Updates cannot leak mutable borrows
/// beyond the callback, and every retained handle is revoked at settlement.
pub struct DocDraft<T> {
    cell: Arc<DraftCell>,
    marker: PhantomData<fn() -> T>,
}
impl<T> Clone for DocDraft<T> {
    fn clone(&self) -> Self {
        Self {
            cell: self.cell.clone(),
            marker: PhantomData,
        }
    }
}
impl<T> DocDraft<T> {
    /// Look at the draft's current JSON without decoding it into `T`.
    pub fn with_json<R>(&self, inspect: impl FnOnce(&JsonValue) -> R) -> Result<R> {
        let change = self.cell.change.lock();
        if self.cell.sealed.load(Ordering::Acquire) {
            return Err(settled());
        }
        Ok(inspect(change.current()?))
    }
    pub fn same_draft(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cell, &other.cell)
    }
}
impl<T: DocumentState> DocDraft<T> {
    pub fn read(&self) -> Result<T> {
        let change = self.cell.change.lock();
        if self.cell.sealed.load(Ordering::Acquire) {
            return Err(settled());
        }
        T::from_state(change.current()?)
    }
    pub fn update<R>(&self, update: impl FnOnce(&mut T) -> Result<R>) -> Result<R> {
        let mut change = self.cell.change.lock();
        if self.cell.sealed.load(Ordering::Acquire) {
            return Err(settled());
        }
        let draft = change.state()?;
        let mut value = T::from_state(draft)?;
        let result = update(&mut value)?;
        let value = value.to_state()?;
        JsonObject::try_from_value(value.clone()).map_err(Error::message)?;
        *draft = value;
        Ok(result)
    }
}

impl<T> DocDraft<T> {
    /// Record explicit JSON edits before persistence, preserving structural no-ops.
    pub fn apply_ops(&self, ops: &[pi_chord::delta::Op]) -> Result<()> {
        let mut change = self.cell.change.lock();
        if self.cell.sealed.load(Ordering::Acquire) {
            return Err(settled());
        }
        change.apply_ops_checked(ops, |value| {
            if value.is_object() {
                Ok(())
            } else {
                Err(pi_chord::delta::Error::Type(
                    "Expected a JSON object".into(),
                ))
            }
        })?;
        Ok(())
    }
}

type Acquisition = Shared<StorageFuture<Arc<DraftCell>>>;
#[derive(Clone)]
enum Target {
    Loaded(Loaded),
    Created {
        record: DocumentCreate,
        version: u64,
        tracker: Arc<Mutex<Tracker>>,
    },
    ForkCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
    RetireOnly(DocumentRecord),
}
struct DocumentEntry {
    address: DocumentAddress,
    definition: Mutex<Option<Arc<AnyDocDefinition>>>,
    target: Mutex<Option<Target>>,
    acquisition: Mutex<Option<Acquisition>>,
    retire: AtomicBool,
}
impl DocumentEntry {
    fn new(
        address: DocumentAddress,
        definition: Option<Arc<AnyDocDefinition>>,
        target: Option<Target>,
    ) -> Self {
        Self {
            address,
            definition: Mutex::new(definition),
            target: Mutex::new(target),
            acquisition: Mutex::new(None),
            retire: AtomicBool::new(false),
        }
    }
}
struct PlannedChange {
    tracker: Arc<Mutex<Tracker>>,
    prepared: Prepared,
    version: u64,
    loaded: Option<Loaded>,
    definition: Arc<AnyDocDefinition>,
}
struct DocumentPlan {
    record: DocumentCreate,
    committed: Option<DocumentRecord>,
    retire: bool,
    content: Option<StorageWrite>,
    change: Option<PlannedChange>,
    conversation_id: Option<ConversationId>,
}
impl DocumentPlan {
    fn publishes(&self) -> bool {
        self.retire
            || self.change.as_ref().is_some_and(|c| c.loaded.is_none())
            || self.content.is_some()
    }
}
#[derive(Clone)]
struct TaskCandidate {
    created: bool,
    record: TaskRecord,
}
enum SubmissionChange {
    Place(EntryId),
    Settle(SubmissionSettlement),
}
#[derive(Default)]
struct State {
    sealed: bool,
    has_table_write: bool,
    pending: usize,
    drains: Vec<Shared<futures::future::BoxFuture<'static, ()>>>,
    writes: Vec<StorageWrite>,
    created_conversations: HashSet<ConversationId>,
    fork_conversations: HashSet<ConversationId>,
    fork_documents: HashSet<DocumentId>,
    tasks: IndexMap<TaskId, TaskCandidate>,
    committed_tasks: HashMap<TaskId, Shared<StorageFuture<Option<TaskRecord>>>>,
    submissions: IndexMap<SubmissionId, SubmissionRecord>,
    submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    documents: Vec<Arc<DocumentEntry>>,
    latest: HashMap<String, Arc<DocumentEntry>>,
    plans: Vec<DocumentPlan>,
}
struct Inner {
    host: Arc<dyn TransactionHost>,
    context: Mutex<Context>,
    scope: TransactionScope,
    sealed: Arc<AtomicBool>,
    state: Mutex<State>,
}
#[derive(Clone)]
pub struct Transaction {
    inner: Arc<Inner>,
}
enum Operation {
    Read(&'static str),
    Write,
    Document,
}
fn settled() -> Error {
    Error::message("Transaction has settled")
}
impl Transaction {
    /// Check a retained handle before decoding an erased caller's next input.
    pub fn check_active(&self) -> Result<()> {
        self.assert_open()
    }
    pub(crate) fn new(
        host: Arc<dyn TransactionHost>,
        context: Context,
        scope: TransactionScope,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                context: Mutex::new(context),
                scope,
                sealed: Arc::new(AtomicBool::new(false)),
                state: Mutex::new(State::default()),
            }),
        }
    }
    fn assert_open(&self) -> Result<()> {
        if self.inner.sealed.load(Ordering::Acquire) {
            Err(settled())
        } else {
            Ok(())
        }
    }
    fn storage(&self) -> Arc<dyn Storage> {
        self.inner.host.storage()
    }
    fn context(&self) -> Context {
        self.inner.context.lock().clone()
    }
    fn start<T, F, Fut>(&self, operation: Operation, callback: F) -> StorageFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let (send, receive) = tokio::sync::oneshot::channel();
        let (drain_send, drain_receive) = tokio::sync::oneshot::channel();
        {
            let mut state = self.inner.state.lock();
            if state.sealed {
                return ready(Err(settled()));
            }
            match operation {
                Operation::Read(method) if state.has_table_write => {
                    return ready(Err(Error::read_after_write(method)));
                }
                Operation::Write => state.has_table_write = true,
                _ => {}
            }
            state.pending += 1;
            state.drains.push(
                async move {
                    let _ = drain_receive.await;
                }
                .boxed()
                .shared(),
            );
        }
        let tx = self.clone();
        pi_chord::rt::spawn(async move {
            let result = std::panic::AssertUnwindSafe(async { callback(tx.clone()).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|p| Err(super::panic_error(p)));
            // Promise settlement precedes observation by the callback's awaiter.
            tx.inner.state.lock().pending -= 1;
            let _ = drain_send.send(());
            let _ = send.send(result);
        });
        Box::pin(async move {
            receive
                .await
                .map_err(|_| Error::message("Transaction operation stopped"))?
        })
    }

    pub fn conversation(&self, id: ConversationId) -> StorageFuture<Option<ConversationRecord>> {
        self.start(Operation::Read("conversation"), move |tx| async move {
            tx.storage().conversation(id, &tx.context()).await
        })
    }
    pub fn entry(&self, id: EntryId) -> StorageFuture<Option<EntryRecord>> {
        self.start(Operation::Read("entry"), move |tx| async move {
            Ok(tx
                .storage()
                .entry(id, &tx.context())
                .await?
                .map(|s| s.entry))
        })
    }
    pub fn task(&self, id: TaskId) -> StorageFuture<Option<TaskRecord>> {
        self.start(Operation::Read("task"), move |tx| async move {
            tx.committed_task(id).await
        })
    }
    pub fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<Page<ConversationRecord>> {
        self.start(Operation::Read("scanConversations"), move |tx| async move {
            tx.storage()
                .scan_conversations(&query, limit, cursor.as_ref(), &tx.context())
                .await
        })
    }
    pub fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<Page<EntryRecord>> {
        self.start(Operation::Read("scanEntries"), move |tx| async move {
            tx.storage()
                .scan_entries(&query, limit, cursor.as_ref(), &tx.context())
                .await
        })
    }
    pub fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<Page<TaskRecord>> {
        self.start(Operation::Read("scanTasks"), move |tx| async move {
            tx.storage()
                .scan_tasks(&query, limit, cursor.as_ref(), &tx.context())
                .await
        })
    }
    pub fn latest_head_marker(
        &self,
        conversation: ConversationId,
    ) -> StorageFuture<Option<EntryRecord>> {
        self.start(Operation::Read("latestHeadMarker"), move |tx| async move {
            tx.storage()
                .find_latest_head_marker(conversation, None, &tx.context())
                .await
        })
    }
    pub fn submission(&self, id: SubmissionId) -> StorageFuture<Option<SubmissionRecord>> {
        self.start(Operation::Read("submission"), move |tx| async move {
            tx.storage().submission(id, &tx.context()).await
        })
    }
    pub fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: JsonString,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        self.start(
            Operation::Read("submissionByRequest"),
            move |tx| async move {
                tx.storage()
                    .submission_by_request(conversation, &request, &tx.context())
                    .await
            },
        )
    }
    fn committed_task(&self, id: TaskId) -> Shared<StorageFuture<Option<TaskRecord>>> {
        self.inner
            .state
            .lock()
            .committed_tasks
            .entry(id)
            .or_insert_with(|| self.storage().task(id, &self.context()).shared())
            .clone()
    }
    async fn current_task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        let candidate = self.inner.state.lock().tasks.get(&id).cloned();
        if let Some(candidate) = candidate {
            return Ok(Some(candidate.record));
        }
        self.committed_task(id).await
    }
    async fn require_conversation(&self, id: ConversationId) -> Result<()> {
        if self.inner.state.lock().created_conversations.contains(&id) {
            return Ok(());
        }
        if self
            .storage()
            .conversation(id, &self.context())
            .await?
            .is_none()
        {
            return Err(Error::message(format!("Conversation {id} does not exist")));
        }
        Ok(())
    }
    pub fn create_conversation(
        &self,
        ownership: ConversationOwnership,
    ) -> StorageFuture<ConversationRecord> {
        self.start(Operation::Write, move |tx| async move {
            tx.stage_conversation(None, ownership, None).await
        })
    }
    pub fn create_root_conversation(&self) -> StorageFuture<ConversationRecord> {
        self.start(Operation::Write, move |tx| async move {
            tx.stage_conversation(
                None,
                ConversationOwnership::Ownerless,
                Some(ROOT_CONVERSATION_ID),
            )
            .await
        })
    }
    pub fn fork_conversation(
        &self,
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> StorageFuture<ConversationRecord> {
        self.start(Operation::Write, move |tx| async move {
            tx.stage_conversation(
                Some(ConversationParent {
                    conversation_id: parent,
                    at,
                }),
                ownership,
                None,
            )
            .await
        })
    }
    async fn stage_conversation(
        &self,
        parent: Option<ConversationParent>,
        ownership: ConversationOwnership,
        reserved: Option<ConversationId>,
    ) -> Result<ConversationRecord> {
        let id = match reserved {
            Some(id) => id,
            None => ConversationId::from_raw(self.storage().mint_id().await?),
        };
        self.assert_open()?;
        let owner = match ownership {
            ConversationOwnership::Ownerless => None,
            ConversationOwnership::Task(task_id) => {
                let task = self.current_task(task_id).await?.ok_or_else(|| {
                    Error::message(format!("Conversation owner task {task_id} does not exist"))
                })?;
                self.assert_open()?;
                Some(ConversationOwner {
                    task_id,
                    conversation_id: task.info.conversation_id,
                })
            }
        };
        let copies = if let Some(parent) = &parent {
            prepare_fork_copies(
                self.storage(),
                parent.conversation_id,
                parent.at,
                id,
                self.context(),
            )
            .await?
        } else {
            Vec::new()
        };
        self.assert_open()?;
        let record = ConversationRecord { id, parent, owner };
        {
            let mut state = self.inner.state.lock();
            for (record, source) in copies {
                state.fork_documents.insert(source.id);
                let address = record.address();
                let entry = Arc::new(DocumentEntry::new(
                    address.clone(),
                    None,
                    Some(Target::ForkCopy { record, source }),
                ));
                state.latest.insert(address_id(&address), entry.clone());
                state.documents.push(entry);
            }
            if let Some(parent) = &record.parent {
                state.fork_conversations.insert(parent.conversation_id);
            }
            state.created_conversations.insert(id);
            state.writes.push(StorageWrite::Conversation {
                value: record.clone(),
            });
        }
        self.inner
            .host
            .conversation_created(self.clone(), record.clone())
            .await?;
        self.assert_open()?;
        Ok(record)
    }
    pub fn append_entry(
        &self,
        conversation_id: ConversationId,
        draft: EntryDraft,
    ) -> StorageFuture<EntryRecord> {
        self.start(Operation::Write, move |tx| async move {
            tx.require_conversation(conversation_id).await?;
            tx.assert_open()?;
            let id = EntryId::from_raw(tx.storage().mint_id().await?);
            tx.assert_open()?;
            let record = EntryRecord {
                id,
                conversation_id,
                kind: draft.kind,
                model: draft.model,
                data: draft.data,
                edits: draft.edits,
                by_task_id: tx.inner.scope.task_id,
                head: draft.head.map(|h| match h {
                    EntryHead::ThisEntry => id,
                    EntryHead::Entry(id) => id,
                }),
            };
            tx.inner.state.lock().writes.push(StorageWrite::Entry {
                value: record.clone(),
            });
            Ok(record)
        })
    }
    /// Kernel task creation, also used by definition-backed creation in the scheduler.
    pub fn create_task_record(
        &self,
        kind: JsonString,
        version: u64,
        input: JsonValue,
        initial: impl FnOnce(&JsonValue) -> Result<JsonValue> + Send + 'static,
        mut options: TaskOptions,
    ) -> StorageFuture<TaskId> {
        self.start(Operation::Write, move |tx| async move {
            if let Some(ids) = &options.submission_ids {
                let mut links = Vec::new();
                options.trace = None;
                for (index, id) in ids.iter().enumerate() {
                    let staged = tx.inner.state.lock().submissions.get(id).cloned();
                    let record = match staged {
                        Some(s) => Some(s),
                        None => tx.storage().submission(*id, &tx.context()).await?,
                    };
                    let trace = record.and_then(|s| s.info.trace);
                    if index == 0 {
                        options.trace = Some(trace.map_or(TaskTrace::Root, TaskTrace::Carrier));
                    } else if let Some(trace) = trace {
                        links.push(trace);
                    }
                }
                options.links = if links.is_empty() { None } else { Some(links) };
            }
            let owner = match options.ownership {
                TaskOwnership::Conversation => None,
                TaskOwnership::Task(id) => {
                    let owner = tx
                        .current_task(id)
                        .await?
                        .ok_or_else(|| Error::message(format!("Task owner {id} does not exist")))?;
                    tx.assert_open()?;
                    if options.background {
                        return Err(Error::from(pi_chord::delta::Error::Type(
                            "A child task cannot be background".into(),
                        )));
                    }
                    if options
                        .conversation_id
                        .is_some_and(|c| c != owner.info.conversation_id)
                    {
                        return Err(Error::message(format!(
                            "A child task lives in its owner's conversation {}",
                            owner.info.conversation_id
                        )));
                    }
                    Some(owner)
                }
            };
            let conversation_id = owner
                .as_ref()
                .map(|t| t.info.conversation_id)
                .or(options.conversation_id)
                .or(tx.inner.scope.conversation_id)
                .ok_or_else(|| {
                    Error::from(pi_chord::delta::Error::Type(
                        "Tx.createTask() requires options.conversationId".into(),
                    ))
                })?;
            tx.require_conversation(conversation_id).await?;
            tx.assert_open()?;
            let checkpoint = initial(&input)?;
            let id = TaskId::from_raw(tx.storage().mint_id().await?);
            tx.assert_open()?;
            let trace = match options.trace {
                Some(TaskTrace::Root) => None,
                Some(TaskTrace::Carrier(c)) => Some(c),
                None => super::inject_trace(&tx.context())?,
            };
            let record = TaskRecord {
                info: TaskRecordInfo {
                    id,
                    conversation_id,
                    kind,
                    version,
                    input,
                    trace,
                    links: options.links,
                    owner: owner.map(|o| o.id()),
                    background: options.background,
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: options.abandon_on_restart.then_some(True),
                    started_at: None,
                    ended_at: None,
                    restarts: None,
                },
                state: TaskState::pending(checkpoint),
            };
            tx.inner.state.lock().tasks.insert(
                id,
                TaskCandidate {
                    created: true,
                    record,
                },
            );
            Ok(id)
        })
    }
    pub fn create_submission(&self, create: SubmissionCreate) -> StorageFuture<SubmissionRecord> {
        self.start(Operation::Write, move |tx| async move {
            tx.require_conversation(create.conversation_id).await?;
            tx.assert_open()?;
            let id = SubmissionId::from_raw(tx.storage().mint_id().await?);
            tx.assert_open()?;
            let record = SubmissionRecord {
                info: SubmissionRecordInfo {
                    id,
                    conversation_id: create.conversation_id,
                    trace: create.trace,
                    request_id: create.request_id,
                },
                state: create.state,
            };
            tx.inner.state.lock().submissions.insert(id, record.clone());
            Ok(record)
        })
    }
    pub fn settle_submission(
        &self,
        id: SubmissionId,
        settlement: SubmissionSettlement,
    ) -> Result<()> {
        self.stage_submission_change(id, SubmissionChange::Settle(settlement))
    }
    pub fn place_submission(&self, id: SubmissionId, entry: EntryId) -> Result<()> {
        self.stage_submission_change(id, SubmissionChange::Place(entry))
    }
    fn stage_submission_change(&self, id: SubmissionId, change: SubmissionChange) -> Result<()> {
        let mut state = self.inner.state.lock();
        if state.sealed {
            return Err(settled());
        }
        state.has_table_write = true;
        state.submission_changes.push((id, change));
        Ok(())
    }
    pub fn set_task(&self, mut record: TaskRecord) -> Result<()> {
        let mut state = self.inner.state.lock();
        if state.sealed {
            return Err(settled());
        }
        state.has_table_write = true;
        let candidate = state.tasks.get(&record.id());
        if candidate.is_some_and(|c| c.record.state.status() == TaskStatus::Terminal) {
            return Err(Error::message(format!(
                "Task {} already has a terminal candidate",
                record.id()
            )));
        }
        if candidate.is_some_and(|c| c.record.info.conversation_id != record.info.conversation_id) {
            return Err(Error::message(format!(
                "Task {} cannot change conversations",
                record.id()
            )));
        }
        record.info.started_at = candidate
            .and_then(|c| c.record.info.started_at)
            .or(record.info.started_at)
            .or_else(|| {
                (record.state.status() == TaskStatus::Running).then(|| self.inner.host.now())
            });
        record.info.ended_at = candidate
            .and_then(|c| c.record.info.ended_at)
            .or(record.info.ended_at)
            .or_else(|| {
                (record.state.status() == TaskStatus::Terminal).then(|| self.inner.host.now())
            });
        let created = candidate.is_some_and(|c| c.created);
        state
            .tasks
            .insert(record.id(), TaskCandidate { created, record });
        Ok(())
    }
    pub fn staged_tasks(&self) -> Vec<TaskRecord> {
        self.inner
            .state
            .lock()
            .tasks
            .values()
            .map(|t| t.record.clone())
            .collect()
    }
    pub fn staged_conversations(&self) -> Vec<ConversationRecord> {
        self.inner
            .state
            .lock()
            .writes
            .iter()
            .filter_map(|w| match w {
                StorageWrite::Conversation { value } => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn doc<T: DocumentState, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
    ) -> StorageFuture<DocDraft<T>> {
        self.typed_doc(token.definition.clone(), token.address(owner), None)
    }
    /// Erased definition boundary used by portable extension hosts.
    pub fn doc_at(
        &self,
        definition: &Arc<AnyDocDefinition>,
        address: DocumentAddress,
        seed: Option<JsonValue>,
    ) -> StorageFuture<DocDraft<JsonObject>> {
        if let Err(error) = self
            .assert_open()
            .and_then(|()| definition.check_address(&address))
        {
            return ready(Err(error));
        }
        self.typed_doc(definition.clone(), address, seed)
    }
    pub fn retire_at(
        &self,
        definition: &Arc<AnyDocDefinition>,
        address: DocumentAddress,
    ) -> StorageFuture<()> {
        if let Err(error) = definition.check_address(&address) {
            return ready(Err(error));
        }
        self.retire_raw(definition.clone(), address)
    }
    pub fn doc_family<T: DocumentState, I: Serialize, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: impl Into<JsonString>,
        seed: I,
    ) -> StorageFuture<DocDraft<T>> {
        let seed = match pi_chord::json::to_value(&seed) {
            Ok(v) => v,
            Err(e) => return ready(Err(Error::message(e))),
        };
        self.typed_doc(
            token.definition.clone(),
            token.address(owner, key),
            Some(seed),
        )
    }
    fn typed_doc<T: Send + 'static>(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
        seed: Option<JsonValue>,
    ) -> StorageFuture<DocDraft<T>> {
        let future = self.doc_raw(definition, address, seed);
        Box::pin(async move {
            Ok(DocDraft {
                cell: future.await?,
                marker: PhantomData,
            })
        })
    }
    fn doc_raw(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
        seed: Option<JsonValue>,
    ) -> StorageFuture<Arc<DraftCell>> {
        let id = address_id(&address);
        let (entry, skip_load) = {
            let mut state = self.inner.state.lock();
            if state.sealed {
                return ready(Err(settled()));
            }
            if let DocumentScope::Task { task_id } = address.scope
                && state
                    .tasks
                    .get(&task_id)
                    .is_some_and(|t| t.record.state.status() == TaskStatus::Terminal)
            {
                return ready(Err(Error::message(format!("Task {task_id} is terminal"))));
            }
            let previous = state.latest.get(&id).cloned();
            let skip = previous
                .as_ref()
                .is_some_and(|e| e.retire.load(Ordering::Acquire));
            let entry = if let Some(entry) = previous.filter(|_| !skip) {
                entry
            } else {
                let entry = Arc::new(DocumentEntry::new(address, Some(definition.clone()), None));
                state.latest.insert(id, entry.clone());
                state.documents.push(entry.clone());
                entry
            };
            (entry, skip)
        };
        let mut acquisition = entry.acquisition.lock();
        if let Some(future) = &*acquisition {
            let future = future.clone();
            return Box::pin(future);
        }
        *entry.definition.lock() = Some(definition.clone());
        let entry_copy = entry.clone();
        let future = self
            .start(Operation::Document, move |tx| async move {
                tx.acquire(entry_copy, definition, seed, skip_load).await
            })
            .shared();
        *acquisition = Some(future.clone());
        Box::pin(future)
    }
    async fn acquire(
        &self,
        entry: Arc<DocumentEntry>,
        definition: Arc<AnyDocDefinition>,
        seed: Option<JsonValue>,
        skip_load: bool,
    ) -> Result<Arc<DraftCell>> {
        let target = entry.target.lock().clone();
        let target = if let Some(Target::ForkCopy { record, source }) = target {
            let stored = self
                .storage()
                .document(source.id, source.at, &self.context())
                .await?
                .ok_or_else(|| {
                    Error::message(format!("Fork source document {} does not exist", source.id))
                })?;
            self.assert_open()?;
            if stored.record.create.kind != record.kind
                || stored.record.create.key != record.key
                || !matches!(stored.record.create.semantics,DocumentSemantics::Conversation {ref history,..}
                    if matches!(&record.semantics,DocumentSemantics::Conversation {history:target,..} if history==target))
            {
                return Err(Error::message(format!(
                    "Fork source document {} does not match its copy",
                    source.id
                )));
            }
            let value = definition.materialize_value(&record, stored.version, stored.value)?;
            Target::Created {
                record,
                version: definition.version(),
                tracker: Arc::new(Mutex::new(track(value.into_value()))),
            }
        } else {
            let loaded = if skip_load {
                None
            } else {
                self.inner
                    .host
                    .load(definition.clone(), entry.address.clone(), self.context())
                    .await?
            };
            self.assert_open()?;
            if let Some(loaded) = loaded {
                Target::Loaded(loaded)
            } else {
                match entry.address.scope {
                    DocumentScope::Conversation { conversation_id } => {
                        self.require_conversation(conversation_id).await?
                    }
                    DocumentScope::Task { task_id } => {
                        let task = self.current_task(task_id).await?.ok_or_else(|| {
                            Error::message(format!("Task {task_id} does not exist"))
                        })?;
                        if task.state.status() == TaskStatus::Terminal {
                            return Err(Error::message(format!("Task {task_id} is terminal")));
                        }
                    }
                    DocumentScope::Session => {}
                }
                self.assert_open()?;
                let value = definition.initial(seed.as_ref())?;
                let id = DocumentId::from_raw(self.storage().mint_id().await?);
                self.assert_open()?;
                Target::Created {
                    record: definition.create(&entry.address, id)?,
                    version: definition.version(),
                    tracker: Arc::new(Mutex::new(track(value.into_value()))),
                }
            }
        };
        let tracker = match &target {
            Target::Created { tracker, .. } => tracker.clone(),
            Target::Loaded(l) => l.lock().tracker.clone(),
            _ => unreachable!(),
        };
        let cell = Arc::new(DraftCell {
            change: Mutex::new(tracker.lock().begin_change()),
            sealed: self.inner.sealed.clone(),
        });
        *entry.target.lock() = Some(target);
        Ok(cell)
    }
    pub fn retire_doc<T, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
    ) -> StorageFuture<()> {
        self.retire_raw(token.definition.clone(), token.address(owner))
    }
    pub fn retire_doc_family<T, I, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: impl Into<JsonString>,
    ) -> StorageFuture<()> {
        self.retire_raw(token.definition.clone(), token.address(owner, key))
    }
    fn retire_raw(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
    ) -> StorageFuture<()> {
        let id = address_id(&address);
        let entry = {
            let mut state = self.inner.state.lock();
            if state.sealed {
                return ready(Err(settled()));
            }
            if let Some(entry) = state.latest.get(&id) {
                if entry.retire.swap(true, Ordering::AcqRel) {
                    return ready(Ok(()));
                }
                entry.clone()
            } else {
                let entry = Arc::new(DocumentEntry::new(
                    address.clone(),
                    Some(definition.clone()),
                    None,
                ));
                entry.retire.store(true, Ordering::Release);
                state.latest.insert(id.clone(), entry.clone());
                state.documents.push(entry.clone());
                entry
            }
        };
        self.start(Operation::Document, move |tx| async move {
            let acquisition = entry.acquisition.lock().clone();
            if let Some(acquisition) = acquisition {
                acquisition.await?;
                return Ok(());
            }
            let target = entry.target.lock().clone();
            if let Some(Target::ForkCopy { record, .. }) = target {
                definition.check_record_scope(&record)?;
                return Ok(());
            }
            let cached = tx.inner.host.cached(&id).map(|l| l.lock().record.clone());
            let record = match cached {
                Some(record) => Some(record),
                None => {
                    tx.storage()
                        .find_document(&address, DocumentPoint::Current, &tx.context())
                        .await?
                }
            };
            tx.assert_open()?;
            if let Some(record) = record {
                definition.check_record_scope(&record.create)?;
                *entry.target.lock() = Some(Target::RetireOnly(record));
            }
            Ok(())
        })
    }

    pub(crate) async fn settle_failure(&self) {
        let drains = self.seal();
        self.discard();
        join_all(drains).await;
        self.discard();
        *self.inner.context.lock() = Context::default();
    }
    fn seal(&self) -> Vec<Shared<futures::future::BoxFuture<'static, ()>>> {
        let mut state = self.inner.state.lock();
        state.sealed = true;
        self.inner.sealed.store(true, Ordering::Release);
        std::mem::take(&mut state.drains)
    }
    pub(crate) async fn settle_success(&self) -> Result<Vec<StorageWrite>> {
        let (pending, drains) = {
            let mut state = self.inner.state.lock();
            state.sealed = true;
            self.inner.sealed.store(true, Ordering::Release);
            (state.pending, std::mem::take(&mut state.drains))
        };
        if pending > 0 {
            self.discard();
            join_all(drains).await;
            self.discard();
            *self.inner.context.lock() = Context::default();
            return Err(Error::message(
                "Session commit callback settled before its pending Tx operations",
            ));
        }
        let result = self.assemble().await;
        *self.inner.context.lock() = Context::default();
        if result.is_err() {
            self.discard()
        }
        result
    }
    pub(crate) fn discard(&self) {
        #[cfg(feature = "stock-test-probes")]
        self.inner
            .host
            .transaction_probe(TransactionProbePoint::Discard)
            .unwrap_or_else(|error| std::panic::panic_any(error));
        let state = self.inner.state.lock();
        for entry in &state.documents {
            if let Some(future) = entry.acquisition.lock().as_ref()
                && let Some(Ok(cell)) = future.peek()
            {
                cell.change.lock().abort();
            }
        }
        for plan in &state.plans {
            if let Some(change) = &plan.change {
                change.prepared.abort();
            }
        }
    }
    async fn assemble(&self) -> Result<Vec<StorageWrite>> {
        let documents = self.inner.state.lock().documents.clone();
        let mut plans = Vec::new();
        // Preparing all drafts revokes them before any validation callback runs.
        for entry in documents {
            let target = entry.target.lock().clone();
            let Some(target) = target else { continue };
            let mut plan = DocumentPlan {
                record: match &target {
                    Target::Loaded(l) => l.lock().record.create.clone(),
                    Target::Created { record, .. } | Target::ForkCopy { record, .. } => {
                        record.clone()
                    }
                    Target::RetireOnly(r) => r.create.clone(),
                },
                committed: None,
                retire: entry.retire.load(Ordering::Acquire),
                content: None,
                change: None,
                conversation_id: None,
            };
            match target {
                Target::ForkCopy { record, source } => {
                    plan.content = Some(StorageWrite::DocumentCopy { record, source })
                }
                Target::RetireOnly(record) => plan.committed = Some(record),
                target => {
                    let (tracker, version, loaded) = match target {
                        Target::Loaded(l) => {
                            let loaded = l.lock();
                            (
                                loaded.tracker.clone(),
                                loaded.value_version,
                                Some(l.clone()),
                            )
                        }
                        Target::Created {
                            tracker, version, ..
                        } => (tracker, version, None),
                        _ => unreachable!(),
                    };
                    let acquisition = entry
                        .acquisition
                        .lock()
                        .clone()
                        .expect("tracked document has acquisition");
                    let cell = acquisition.await?;
                    let prepared = cell.change.lock().prepare()?;
                    if let Some(loaded) = &loaded {
                        let loaded = loaded.lock();
                        plan.committed = Some(loaded.record.clone());
                        if loaded.stored_version < version {
                            plan.content = Some(StorageWrite::DocumentChange {
                                id: plan.record.id,
                                content: DocumentContent::Base {
                                    version,
                                    value: JsonObject::try_from_value(
                                        prepared.value().as_ref().clone(),
                                    )
                                    .map_err(Error::message)?,
                                },
                            })
                        } else if !prepared.ops().is_empty() {
                            plan.content = Some(StorageWrite::DocumentChange {
                                id: plan.record.id,
                                content: DocumentContent::Delta {
                                    version,
                                    ops: prepared.shared_ops(),
                                },
                            })
                        }
                    } else {
                        plan.content = Some(StorageWrite::DocumentCreate {
                            record: plan.record.clone(),
                            content: DocumentBase {
                                version,
                                value: JsonObject::try_from_value(
                                    prepared.value().as_ref().clone(),
                                )
                                .map_err(Error::message)?,
                            },
                        })
                    }
                    plan.change = Some(PlannedChange {
                        tracker,
                        prepared,
                        version,
                        loaded,
                        definition: entry
                            .definition
                            .lock()
                            .clone()
                            .expect("acquired definition"),
                    });
                }
            }
            plans.push(plan);
        }
        // Keep prepared leases reachable even when a later validation fails.
        self.inner.state.lock().plans = plans;
        self.reject_fork_source_writes()?;
        let candidates = self.inner.state.lock().tasks.clone();
        for conversation in self.staged_conversations() {
            if let Some(owner) = conversation.owner {
                self.validate_owner(owner.task_id, "Conversation owner task")
                    .await?;
            }
        }
        for candidate in candidates.values() {
            if candidate.created {
                if let Some(owner) = candidate.record.info.owner {
                    self.validate_owner(owner, "Task owner").await?;
                }
            } else {
                let id = candidate.record.id();
                let committed = self
                    .committed_task(id)
                    .await?
                    .ok_or_else(|| Error::message(format!("Task {id} does not exist")))?;
                if committed.state.status() == TaskStatus::Terminal {
                    return Err(Error::message(format!("Task {id} is already terminal")));
                }
                if committed.info.conversation_id != candidate.record.info.conversation_id {
                    return Err(Error::message(format!(
                        "Task {id} cannot change conversations"
                    )));
                }
            }
        }
        for candidate in candidates
            .values()
            .filter(|c| c.record.state.status() == TaskStatus::Terminal)
        {
            let id = candidate.record.id();
            {
                let mut state = self.inner.state.lock();
                for plan in &mut state.plans {
                    if plan.record.semantics.scope() == (DocumentScope::Task { task_id: id }) {
                        plan.retire = true;
                    }
                }
            }
            if candidate.created {
                continue;
            }
            let query = DocumentQuery {
                scope: DocumentScope::Task { task_id: id },
                at: DocumentPoint::Current,
                kind: None,
            };
            let mut cursor = None;
            loop {
                let page = self
                    .storage()
                    .scan_documents(&query, 256, cursor.as_ref(), &self.context())
                    .await?;
                let mut state = self.inner.state.lock();
                for record in page.items {
                    if let Some(plan) = state.plans.iter_mut().find(|p| p.record.id == record.id())
                    {
                        plan.retire = true;
                    } else {
                        state.plans.push(DocumentPlan {
                            record: record.create.clone(),
                            committed: Some(record),
                            retire: true,
                            content: None,
                            change: None,
                            conversation_id: None,
                        });
                    }
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        let scopes = self
            .inner
            .state
            .lock()
            .plans
            .iter()
            .map(|p| p.record.semantics.scope())
            .collect::<Vec<_>>();
        for (index, scope) in scopes.into_iter().enumerate() {
            let conversation = match scope {
                DocumentScope::Session => None,
                DocumentScope::Conversation { conversation_id } => Some(conversation_id),
                DocumentScope::Task { task_id } => self
                    .current_task(task_id)
                    .await?
                    .map(|t| t.info.conversation_id),
            };
            self.inner.state.lock().plans[index].conversation_id = conversation;
        }
        self.resolve_submissions().await?;
        let mut state = self.inner.state.lock();
        // Checkpoint policy is user code and runs only after semantic validation.
        for plan in &mut state.plans {
            let Some(StorageWrite::DocumentChange {
                content: DocumentContent::Delta { .. },
                ..
            }) = &plan.content
            else {
                continue;
            };
            let change = plan.change.as_ref().expect("delta has change");
            let loaded = change
                .loaded
                .as_ref()
                .expect("delta has loaded document")
                .lock();
            let value = JsonObject::try_from_value(change.prepared.value().as_ref().clone())
                .map_err(Error::message)?;
            if change.definition.checkpoint_when_shared(
                &value,
                &change.prepared.shared_ops(),
                CheckpointInfo {
                    deltas_since_base: loaded.deltas_since_base,
                },
            )? {
                plan.content = Some(StorageWrite::DocumentChange {
                    id: plan.record.id,
                    content: DocumentContent::Base {
                        version: change.version,
                        value,
                    },
                });
            }
        }
        let mut writes = state.writes.clone();
        writes.extend(
            state
                .submissions
                .values()
                .cloned()
                .map(|value| StorageWrite::Submission { value }),
        );
        writes.extend(state.tasks.values().map(|c| StorageWrite::Task {
            value: Box::new(c.record.clone()),
        }));
        for plan in &state.plans {
            if let Some(content) = &plan.content {
                writes.push(content.clone());
            }
            if plan.retire {
                writes.push(StorageWrite::DocumentRetire { id: plan.record.id });
            }
        }
        Ok(writes)
    }
    fn reject_fork_source_writes(&self) -> Result<()> {
        let state = self.inner.state.lock();
        for plan in &state.plans {
            if !plan.publishes() {
                continue;
            }
            if state.fork_documents.contains(&plan.record.id) {
                return Err(Error::message(format!(
                    "Cannot change fork source document {} in the fork transaction",
                    plan.record.id
                )));
            }
            if let DocumentSemantics::Conversation {
                conversation_id,
                history,
            } = &plan.record.semantics
                && state.fork_conversations.contains(conversation_id)
                && matches!(
                    history,
                    ConversationHistory::Latest {
                        fork: LatestFork::Current
                    } | ConversationHistory::Rewindable {
                        fork: ForkPolicy::Current
                    }
                )
            {
                return Err(Error::message(format!(
                    "Cannot fork conversation {conversation_id} while changing its current-policy documents"
                )));
            }
        }
        Ok(())
    }
    async fn validate_owner(&self, id: TaskId, what: &str) -> Result<()> {
        let owner = self
            .current_task(id)
            .await?
            .ok_or_else(|| Error::message(format!("{what} {id} does not exist")))?;
        if matches!(
            owner.state.status(),
            TaskStatus::Completing | TaskStatus::Terminal
        ) {
            let status = if owner.state.status() == TaskStatus::Terminal {
                "terminal"
            } else {
                "completing"
            };
            return Err(Error::message(format!("{what} {id} is {status}")));
        }
        if owner.info.abort_requested {
            return Err(Error::message(format!("{what} {id} is abort-marked")));
        }
        Ok(())
    }
    async fn resolve_submissions(&self) -> Result<()> {
        let changes = std::mem::take(&mut self.inner.state.lock().submission_changes);
        for (id, change) in changes {
            let staged = self.inner.state.lock().submissions.get(&id).cloned();
            let mut record = match staged {
                Some(s) => s,
                None => self
                    .storage()
                    .submission(id, &self.context())
                    .await?
                    .ok_or_else(|| Error::message(format!("Submission {id} does not exist")))?,
            };
            use SubmissionState::*;
            if !matches!(
                record.state.status(),
                SubmissionStatus::Done | SubmissionStatus::Unanswered
            ) {
                record.state = match change {
                    SubmissionChange::Place(entry) => match record.state {
                        InputQueued => InputPlaced { entry },
                        WriteQueued => WriteDone { entry },
                        _ => return Err(Error::message(format!("Submission {id} is not queued"))),
                    },
                    SubmissionChange::Settle(SubmissionSettlement::Done { answer }) => {
                        match record.state {
                            InputPlaced { entry } => InputDone { entry, answer },
                            _ => {
                                return Err(Error::message(format!(
                                    "Submission {id} is not a placed input"
                                )));
                            }
                        }
                    }
                    SubmissionChange::Settle(SubmissionSettlement::Unanswered {
                        reason,
                        detail,
                    }) => match record.state {
                        InputQueued => InputUnanswered {
                            entry: None,
                            reason,
                            detail,
                        },
                        InputPlaced { entry } => InputUnanswered {
                            entry: Some(entry),
                            reason,
                            detail,
                        },
                        WriteQueued => WriteUnanswered { reason, detail },
                        _ => unreachable!(),
                    },
                };
            }
            self.inner.state.lock().submissions.insert(id, record);
        }
        Ok(())
    }
    pub(crate) fn adopt(&self, seq: Seq, writes: &[StorageWrite]) -> Result<CommitPublication> {
        #[cfg(feature = "stock-test-probes")]
        self.inner
            .host
            .transaction_probe(TransactionProbePoint::Adopt(seq))?;
        let mut changes = Vec::new();
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => {
                    changes.push(CommitChange::Conversation(value.clone()))
                }
                StorageWrite::Entry { value } => changes.push(CommitChange::Entry(value.clone())),
                StorageWrite::Task { value } => changes.push(CommitChange::Task(value.clone())),
                StorageWrite::Submission { value } => {
                    changes.push(CommitChange::Submission(value.clone()))
                }
                _ => {}
            }
        }
        let plans = std::mem::take(&mut self.inner.state.lock().plans);
        for plan in plans {
            let publishes = plan.publishes();
            let record = DocumentRecord {
                create: plan.record.clone(),
                created_at: plan.committed.as_ref().map_or(seq, |r| r.created_at),
                retired_at: if plan.retire {
                    Some(seq)
                } else {
                    plan.committed.as_ref().and_then(|r| r.retired_at)
                },
            };
            let address = address_id(&record.create.address());
            if let Some(change) = &plan.change {
                if if change.loaded.is_some() {
                    change.prepared.ops().is_empty()
                } else {
                    plan.retire
                } {
                    change.prepared.abort();
                } else {
                    change.tracker.lock().adopt(&change.prepared)?;
                }
                if let Some(loaded) = &change.loaded {
                    let mut loaded = loaded.lock();
                    if !change.prepared.ops().is_empty() {
                        loaded.typed.clear();
                    }
                    if let Some(StorageWrite::DocumentChange { content, .. }) = &plan.content {
                        loaded.stored_version = change.version;
                        loaded.deltas_since_base = match content {
                            DocumentContent::Base { .. } => 0,
                            DocumentContent::Delta { .. } => loaded.deltas_since_base + 1,
                        };
                    }
                } else if !plan.retire {
                    self.inner.host.install(
                        address.clone(),
                        Arc::new(Mutex::new(LoadedDocument {
                            record: record.clone(),
                            stored_version: change.version,
                            value_version: change.version,
                            deltas_since_base: 0,
                            tracker: change.tracker.clone(),
                            typed: HashMap::new(),
                        })),
                    );
                }
            }
            if plan.retire {
                self.inner.host.evict(&address, record.id());
            }
            if !publishes {
                continue;
            }
            let document = if plan.retire {
                DocumentCommitChange::Document {
                    record,
                    conversation_id: plan.conversation_id,
                    version: None,
                    value: Arc::new(JsonValue::Null),
                    ops: Arc::new(Vec::new()),
                }
            } else if let Some(StorageWrite::DocumentCopy { source, .. }) = plan.content {
                DocumentCommitChange::Copy {
                    record,
                    conversation_id: plan.conversation_id.expect("copy is conversation scoped"),
                    source,
                }
            } else {
                let change = plan.change.expect("published change has revision");
                let ops = if change.loaded.is_none() {
                    Arc::new(Vec::new())
                } else {
                    change.prepared.shared_ops()
                };
                DocumentCommitChange::Document {
                    record,
                    conversation_id: plan.conversation_id,
                    version: Some(change.version),
                    value: change.prepared.value().clone(),
                    ops,
                }
            };
            changes.push(CommitChange::Document(document));
        }
        Ok(CommitPublication { seq, changes })
    }
}
