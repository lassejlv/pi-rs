use super::{
    Adopt, Storage, StorageFuture, StoredEntry, ready,
    scan::{self, ScanStart},
};
use crate::{Context, Error, JsonString, Result, types::*};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::delta::apply_immutable_batches;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::Bound,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Table {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}
impl Table {
    fn name(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Entry => "entry",
            Self::Task => "task",
            Self::Submission => "submission",
            Self::Document => "document",
        }
    }
}
struct Revision {
    seq: Seq,
    content: DocumentContent,
}
struct Document {
    record: DocumentRecord,
    revisions: Vec<Revision>,
}
#[derive(Default)]
struct Address {
    ids: BTreeSet<DocumentId>,
    current: Option<DocumentId>,
}
#[derive(Default)]
struct Action {
    create: Option<DocumentCreate>,
    content: Option<DocumentContent>,
    retire: bool,
}
#[derive(Default)]
struct State {
    record_types: HashMap<i64, Table>,
    conversations: BTreeMap<ConversationId, ConversationRecord>,
    conversations_by_owner_conversation: HashMap<ConversationId, BTreeSet<ConversationId>>,
    conversations_by_owner_task: HashMap<TaskId, BTreeSet<ConversationId>>,
    entries: BTreeMap<EntryId, StoredEntry>,
    entry_ids: HashMap<ConversationId, BTreeSet<EntryId>>,
    head_ids: HashMap<ConversationId, BTreeSet<EntryId>>,
    tasks: BTreeMap<TaskId, TaskRecord>,
    tasks_by_status: HashMap<TaskStatus, BTreeSet<TaskId>>,
    submissions: BTreeMap<SubmissionId, SubmissionRecord>,
    submissions_by_status: HashMap<SubmissionStatus, BTreeSet<SubmissionId>>,
    submissions_by_request: HashMap<(ConversationId, JsonString), SubmissionId>,
    documents: BTreeMap<DocumentId, Document>,
    addresses: HashMap<DocumentAddress, Address>,
    documents_by_scope: HashMap<DocumentScope, BTreeSet<DocumentId>>,
    next_id: i64,
    next_seq: i64,
    closed: bool,
}
impl State {
    fn open(&self) -> Result<()> {
        if self.closed {
            Err(Error::message("MemoryStorage is closed"))
        } else {
            Ok(())
        }
    }
    fn materialize(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        let Some(document) = self.documents.get(&id) else {
            return Ok(None);
        };
        if at != DocumentPoint::Current && document.record.create.semantics.current_only() {
            return Err(Error::request(format!(
                "Document {id} does not retain historical content"
            )));
        }
        if !document.record.alive_at(at) {
            return Ok(None);
        }
        let end = match at {
            DocumentPoint::Current => document.revisions.len(),
            DocumentPoint::At(seq) => document
                .revisions
                .partition_point(|revision| revision.seq <= seq),
        };
        let revisions = &document.revisions[..end];
        let base_index = revisions
            .iter()
            .rposition(|r| matches!(r.content, DocumentContent::Base { .. }))
            .ok_or_else(|| Error::message(format!("Document {id} is missing a required base")))?;
        let DocumentContent::Base { version, value } = &revisions[base_index].content else {
            unreachable!()
        };
        let batches: Vec<_> = revisions[base_index + 1..]
            .iter()
            .map(|revision| match &revision.content {
                DocumentContent::Delta {
                    version: stored,
                    ops,
                } if stored == version => Ok(ops.as_slice()),
                _ => Err(Error::message(format!(
                    "Document {id} crosses a stored version boundary without a base"
                ))),
            })
            .collect::<Result<_>>()?;
        let value = apply_immutable_batches(Some(value.value()), batches)?;
        Ok(Some(StoredDocument {
            record: document.record.clone(),
            version: *version,
            value: JsonObject::try_from_value(value).map_err(Error::message)?,
            deltas_since_base: revisions.len() - base_index - 1,
        }))
    }
    fn visible(
        &self,
        conversation: ConversationId,
        min: Option<EntryId>,
        max: Option<EntryId>,
        order: ScanOrder,
        limit: usize,
    ) -> Result<Vec<EntryRecord>> {
        if !self.conversations.contains_key(&conversation) {
            return Err(Error::request(format!(
                "Unknown conversation: {conversation}"
            )));
        }
        let lower = min.map_or(i64::MIN, EntryId::get);
        let mut upper = max.map_or(i64::MAX, EntryId::get);
        let mut current = conversation;
        let mut segments = Vec::new();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current) {
                return Err(Error::message("Conversation ancestry contains a cycle"));
            }
            segments.push((current, upper));
            let record = self
                .conversations
                .get(&current)
                .ok_or_else(|| Error::message(format!("Unknown conversation: {current}")))?;
            let Some(parent) = &record.parent else {
                break;
            };
            upper = upper.min(parent.at.get());
            if upper < lower {
                break;
            }
            current = parent.conversation_id;
        }
        if order == ScanOrder::Ascending {
            segments.reverse();
        }
        let mut result = Vec::new();
        for (conversation, upper) in segments {
            if lower > upper {
                continue;
            }
            let Some(ids) = self.entry_ids.get(&conversation) else {
                continue;
            };
            let matching = ids.range(EntryId::from_raw(lower)..=EntryId::from_raw(upper));
            for id in ordered(matching, order) {
                if result.len() >= limit {
                    return Ok(result);
                }
                result.push(self.entries[id].entry.clone());
            }
        }
        Ok(result)
    }
}

/// Detached reference backend. Prepared commits keep validation and copying ahead
/// of persistence, and apply under one mutex so readers see an atomic prefix.
#[derive(Clone)]
pub struct MemoryStorage {
    state: Arc<Mutex<State>>,
    commit_line: Arc<Mutex<()>>,
}
impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}
impl MemoryStorage {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                next_id: 2,
                next_seq: 1,
                ..State::default()
            })),
            commit_line: Arc::default(),
        }
    }
    /// Whether `close()` ran.
    pub fn is_closed(&self) -> bool {
        self.state.lock().closed
    }
    /// Reopen after `close()` (or close again), keeping every committed record, as a test
    /// backend reopens the same store for a new Session.
    pub fn set_closed(&self, closed: bool) {
        self.state.lock().closed = closed;
    }
    pub fn prepare_commit(
        &self,
        writes: &[StorageWrite],
        seq: Option<Seq>,
    ) -> Result<PreparedMemoryCommit> {
        let state = self.state.lock();
        state.open()?;
        let seq = seq.unwrap_or(Seq::from_raw(state.next_seq));
        if seq.get() > MAX_SAFE_INTEGER || seq.get() < state.next_seq {
            return Err(Error::message(format!(
                "Commit sequence {seq} does not strictly increase"
            )));
        }
        let writes = resolve_copies(&state, writes.to_vec())?;
        check_global_ids(&state, &writes)?;
        let actions = document_actions(&writes)?;
        check_document_actions(&state, &actions)?;
        Ok(PreparedMemoryCommit {
            state: self.state.clone(),
            seq,
            writes: Arc::new(writes),
            actions,
            applied: Mutex::new(false),
        })
    }
    fn read<T>(&self, read: impl FnOnce(&State) -> Result<T>) -> Result<T> {
        let state = self.state.lock();
        state.open()?;
        read(&state)
    }
}
pub struct PreparedMemoryCommit {
    state: Arc<Mutex<State>>,
    seq: Seq,
    writes: Arc<Vec<StorageWrite>>,
    actions: IndexMap<DocumentId, Action>,
    applied: Mutex<bool>,
}
impl PreparedMemoryCommit {
    pub fn seq(&self) -> Seq {
        self.seq
    }
    pub fn writes(&self) -> &[StorageWrite] {
        &self.writes
    }
    pub fn apply(&self) -> Result<Seq> {
        let mut applied = self.applied.lock();
        if *applied {
            return Ok(self.seq);
        }
        let mut state = self.state.lock();
        if self.seq.get() < state.next_seq {
            return Err(Error::message("Prepared memory commit is stale"));
        }
        apply_tables(&mut state, &self.writes, self.seq);
        apply_documents(&mut state, &self.actions, self.seq);
        state.next_seq = self.seq.get() + 1;
        *applied = true;
        Ok(self.seq)
    }
}
fn resolve_copies(state: &State, writes: Vec<StorageWrite>) -> Result<Vec<StorageWrite>> {
    let changed: HashSet<_> = writes
        .iter()
        .filter_map(|write| match write {
            StorageWrite::DocumentCreate { record, .. }
            | StorageWrite::DocumentCopy { record, .. } => Some(record.id),
            StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => {
                Some(*id)
            }
            _ => None,
        })
        .collect();
    writes.into_iter().map(|write| {
        let StorageWrite::DocumentCopy {record,source}=write else {return Ok(write);};let id=record.id;
        let resolved=(|| {
            if changed.contains(&source.id) {return Err(Error::message(format!("Fork source document {} is changed in the copy batch",source.id)));}
            let stored=state.materialize(source.id,source.at)?.ok_or_else(||Error::message(format!("Fork source document {} cannot be read",source.id)))?;
            let source_create=&stored.record.create;
            let matches=matches!((&source_create.semantics,&record.semantics),(DocumentSemantics::Conversation {history:a,..},DocumentSemantics::Conversation {history:b,..}) if a==b) && source_create.kind==record.kind && source_create.key==record.key;
            if !matches {return Err(Error::message(format!("Fork source document {} does not match the copied record",source.id)));}
            Ok(StorageWrite::DocumentCreate {record,content:DocumentBase {version:stored.version,value:stored.value}})
        })();resolved.map_err(|cause|Error::document_copy(id,cause))
    }).collect()
}
fn claim(write: &StorageWrite) -> Option<(i64, Table)> {
    Some(match write {
        StorageWrite::Conversation { value } => (value.id.get(), Table::Conversation),
        StorageWrite::Entry { value } => (value.id.get(), Table::Entry),
        StorageWrite::Task { value } => (value.id().get(), Table::Task),
        StorageWrite::Submission { value } => (value.id().get(), Table::Submission),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            (record.id.get(), Table::Document)
        }
        _ => return None,
    })
}
fn check_global_ids(state: &State, writes: &[StorageWrite]) -> Result<()> {
    let mut claimed = HashMap::new();
    for write in writes {
        let Some((id, table)) = claim(write) else {
            continue;
        };
        let existing = state.record_types.get(&id);
        let earlier = claimed.get(&id);
        if matches!(table, Table::Conversation | Table::Entry | Table::Document) {
            if let Some(existing) = existing {
                return Err(Error::message(format!(
                    "ID {id} already belongs to {}",
                    existing.name()
                )));
            }
            if earlier.is_some() {
                return Err(Error::message(format!("ID {id} is written more than once")));
            }
        } else {
            if let Some(existing) = existing.filter(|existing| **existing != table) {
                return Err(Error::message(format!(
                    "ID {id} already belongs to {}",
                    existing.name()
                )));
            }
            if earlier.is_some_and(|earlier| *earlier != table) {
                return Err(Error::message(format!(
                    "ID {id} is written as two record types"
                )));
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}
fn document_actions(writes: &[StorageWrite]) -> Result<IndexMap<DocumentId, Action>> {
    let mut actions: IndexMap<DocumentId, Action> = IndexMap::new();
    for write in writes {
        let (id, record, content, retire) = match write {
            StorageWrite::DocumentCreate { record, content } => (
                record.id,
                Some(record.clone()),
                Some(DocumentContent::Base {
                    version: content.version,
                    value: content.value.clone(),
                }),
                false,
            ),
            StorageWrite::DocumentChange { id, content } => {
                (*id, None, Some(content.clone()), false)
            }
            StorageWrite::DocumentRetire { id } => (*id, None, None, true),
            _ => continue,
        };
        let action = actions.entry(id).or_default();
        if content.is_some()
            && (action.content.is_some() || record.is_some() && action.create.is_some())
        {
            return Err(Error::message(format!(
                "Document {id} has more than one content command"
            )));
        }
        if retire && action.retire {
            return Err(Error::message(format!(
                "Document {id} is retired more than once"
            )));
        }
        if let Some(record) = record {
            action.create = Some(record);
        }
        if let Some(content) = content {
            action.content = Some(content);
        }
        action.retire |= retire;
    }
    Ok(actions)
}
fn check_document_actions(state: &State, actions: &IndexMap<DocumentId, Action>) -> Result<()> {
    let mut live_counts = HashMap::new();
    for (id, action) in actions {
        let existing = state.documents.get(id);
        if action.create.is_none() && existing.is_none() {
            return Err(Error::message(format!("Unknown document: {id}")));
        }
        if action.create.is_some() && existing.is_some() {
            return Err(Error::message(format!("Document {id} already exists")));
        }
        if existing.is_some_and(|stored| stored.record.retired_at.is_some()) {
            return Err(Error::message(format!("Document {id} is retired")));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let previous = existing
                .and_then(|stored| stored.revisions.last())
                .ok_or_else(|| Error::message(format!("Document {id} delta has no base")))?;
            if previous.content.version() != *version {
                return Err(Error::message(format!(
                    "Document {id} version transition requires a base"
                )));
            }
        }
        let address = action
            .create
            .as_ref()
            .unwrap_or_else(|| &existing.unwrap().record.create)
            .address();
        let current = state
            .addresses
            .get(&address)
            .and_then(|address| address.current);
        let live = live_counts
            .entry(address)
            .or_insert(i32::from(current.is_some()));
        if action.retire && current == Some(*id) {
            *live -= 1;
        }
        if action.create.is_some() && !action.retire {
            *live += 1;
        }
    }
    if live_counts.values().any(|live| *live > 1) {
        return Err(Error::message(
            "Document address already has a current incarnation",
        ));
    }
    Ok(())
}
fn apply_tables(state: &mut State, writes: &[StorageWrite], seq: Seq) {
    for write in writes {
        match write {
            StorageWrite::Conversation { value } => {
                state.conversations.insert(value.id, value.clone());
                if let Some(owner) = &value.owner {
                    state
                        .conversations_by_owner_conversation
                        .entry(owner.conversation_id)
                        .or_default()
                        .insert(value.id);
                    state
                        .conversations_by_owner_task
                        .entry(owner.task_id)
                        .or_default()
                        .insert(value.id);
                }
            }
            StorageWrite::Entry { value } => {
                state.entries.insert(
                    value.id,
                    StoredEntry {
                        entry: value.clone(),
                        commit_seq: seq,
                    },
                );
                state
                    .entry_ids
                    .entry(value.conversation_id)
                    .or_default()
                    .insert(value.id);
                if value.head.is_some() {
                    state
                        .head_ids
                        .entry(value.conversation_id)
                        .or_default()
                        .insert(value.id);
                }
            }
            StorageWrite::Task { value } => {
                if let Some(previous) = state.tasks.get(&value.id()) {
                    state
                        .tasks_by_status
                        .entry(previous.state.status())
                        .or_default()
                        .remove(&value.id());
                }
                state
                    .tasks_by_status
                    .entry(value.state.status())
                    .or_default()
                    .insert(value.id());
                state.tasks.insert(value.id(), *value.clone());
            }
            StorageWrite::Submission { value } => {
                if let Some(previous) = state.submissions.get(&value.id()) {
                    state
                        .submissions_by_status
                        .entry(previous.state.status())
                        .or_default()
                        .remove(&value.id());
                    if let Some(request) = &previous.info.request_id {
                        let key = (previous.info.conversation_id, request.clone());
                        if state.submissions_by_request.get(&key) == Some(&value.id()) {
                            state.submissions_by_request.remove(&key);
                        }
                    }
                }
                if let Some(request) = &value.info.request_id {
                    state
                        .submissions_by_request
                        .insert((value.info.conversation_id, request.clone()), value.id());
                }
                state
                    .submissions_by_status
                    .entry(value.state.status())
                    .or_default()
                    .insert(value.id());
                state.submissions.insert(value.id(), value.clone());
            }
            _ => continue,
        }
        let (id, table) = claim(write).unwrap();
        state.record_types.insert(id, table);
        state.next_id = state.next_id.max(id + 1);
    }
}
fn apply_documents(state: &mut State, actions: &IndexMap<DocumentId, Action>, seq: Seq) {
    for (id, action) in actions {
        if let Some(create) = &action.create {
            let record = DocumentRecord {
                create: create.clone(),
                created_at: seq,
                retired_at: action.retire.then_some(seq),
            };
            state.documents.insert(
                *id,
                Document {
                    record,
                    revisions: vec![Revision {
                        seq,
                        content: action.content.as_ref().unwrap().clone(),
                    }],
                },
            );
            state.record_types.insert(id.get(), Table::Document);
            state
                .addresses
                .entry(create.address())
                .or_default()
                .ids
                .insert(*id);
            state
                .documents_by_scope
                .entry(create.semantics.scope())
                .or_default()
                .insert(*id);
            state.next_id = state.next_id.max(id.get() + 1);
        } else if let Some(content) = &action.content {
            let stored = state.documents.get_mut(id).unwrap();
            if matches!(content, DocumentContent::Base { .. })
                && stored.record.create.semantics.current_only()
            {
                stored.revisions.clear();
            }
            stored.revisions.push(Revision {
                seq,
                content: content.clone(),
            });
        }
        let stored = state.documents.get_mut(id).unwrap();
        if action.retire {
            if action.create.is_none() {
                stored.record.retired_at = Some(seq);
            }
            if stored.record.create.semantics.current_only() {
                stored.revisions.clear();
            }
        }
        if action.create.is_some() || action.retire {
            let address = state
                .addresses
                .get_mut(&stored.record.create.address())
                .unwrap();
            if action.retire && address.current == Some(*id) {
                address.current = None;
            }
            if action.create.is_some() && !action.retire {
                address.current = Some(*id);
            }
        }
    }
}
fn after(id: i64, start: &ScanStart) -> bool {
    start.after.is_none_or(|bound| match start.order {
        ScanOrder::Ascending => id > bound,
        ScanOrder::Descending => id < bound,
    })
}
fn bounds<K>(start: &ScanStart, convert: impl FnOnce(i64) -> K) -> (Bound<K>, Bound<K>) {
    match (start.after, start.order) {
        (Some(after), ScanOrder::Ascending) => (Bound::Excluded(convert(after)), Bound::Unbounded),
        (Some(after), ScanOrder::Descending) => (Bound::Unbounded, Bound::Excluded(convert(after))),
        (None, _) => (Bound::Unbounded, Bound::Unbounded),
    }
}
fn map_range<'a, K: Ord, V>(
    map: &'a BTreeMap<K, V>,
    start: &ScanStart,
    convert: impl FnOnce(i64) -> K,
) -> std::collections::btree_map::Range<'a, K, V> {
    map.range(bounds(start, convert))
}
fn set_range<'a, K: Ord>(
    set: &'a BTreeSet<K>,
    start: &ScanStart,
    convert: impl FnOnce(i64) -> K,
) -> std::collections::btree_set::Range<'a, K> {
    set.range(bounds(start, convert))
}
fn page<T>(mut values: Vec<T>, limit: usize, order: ScanOrder, id: impl Fn(&T) -> i64) -> Page<T> {
    let more = values.len() > limit;
    values.truncate(limit);
    let next = if more {
        values.last().map(|last| scan::next_cursor(id(last), order))
    } else {
        None
    };
    Page {
        items: values,
        next,
    }
}
fn ordered<'a, T: 'a>(
    values: impl DoubleEndedIterator<Item = T> + 'a,
    order: ScanOrder,
) -> Box<dyn Iterator<Item = T> + 'a> {
    match order {
        ScanOrder::Ascending => Box::new(values),
        ScanOrder::Descending => Box::new(values.rev()),
    }
}

impl Storage for MemoryStorage {
    fn commit(&self, writes: &[StorageWrite], adopt: Adopt, _: &Context) -> StorageFuture<Seq> {
        let result = (|| {
            let _line = self.commit_line.lock();
            let seq = self.prepare_commit(writes, None)?.apply()?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| adopt(seq)))
                .map_err(|_| Error::message("Storage adoption callback panicked"))?;
            Ok(seq)
        })();
        ready(result)
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        let mut state = self.state.lock();
        let result = state.open().and_then(|_| {
            if state.next_id > MAX_SAFE_INTEGER {
                Err(Error::message("ID space is exhausted"))
            } else {
                let id = state.next_id;
                state.next_id += 1;
                Ok(id)
            }
        });
        ready(result)
    }
    fn conversation(
        &self,
        id: ConversationId,
        _: &Context,
    ) -> StorageFuture<Option<ConversationRecord>> {
        ready(self.read(|s| Ok(s.conversations.get(&id).cloned())))
    }
    fn scan_conversations(
        &self,
        q: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<ConversationRecord>> {
        ready(self.read(|s| {
            let start = scan::scan_start(q.order, cursor, ScanOrder::Ascending)?;
            let candidates: Box<dyn DoubleEndedIterator<Item = &ConversationRecord>> =
                if let Some(owner) = q.owner_task_id {
                    Box::new(
                        s.conversations_by_owner_task
                            .get(&owner)
                            .into_iter()
                            .flat_map(|ids| set_range(ids, &start, ConversationId::from_raw))
                            .filter_map(|id| s.conversations.get(id)),
                    )
                } else if let Some(owner) = q.owner_conversation_id {
                    Box::new(
                        s.conversations_by_owner_conversation
                            .get(&owner)
                            .into_iter()
                            .flat_map(|ids| set_range(ids, &start, ConversationId::from_raw))
                            .filter_map(|id| s.conversations.get(id)),
                    )
                } else {
                    Box::new(
                        map_range(&s.conversations, &start, ConversationId::from_raw)
                            .map(|(_, value)| value),
                    )
                };
            let values = ordered(candidates, start.order)
                .filter(|v| {
                    after(v.id.get(), &start)
                        && q.owner_conversation_id.is_none_or(|id| {
                            v.owner.as_ref().is_some_and(|o| o.conversation_id == id)
                        })
                })
                .take(limit.saturating_add(1))
                .cloned()
                .collect();
            Ok(page(values, limit, start.order, |v| v.id.get()))
        }))
    }
    fn entry(&self, id: EntryId, _: &Context) -> StorageFuture<Option<StoredEntry>> {
        ready(self.read(|s| Ok(s.entries.get(&id).cloned())))
    }
    fn entry_in(
        &self,
        conversation: ConversationId,
        id: EntryId,
        _: &Context,
    ) -> StorageFuture<Option<StoredEntry>> {
        ready(self.read(|s| {
            Ok(
                s.visible(conversation, Some(id), Some(id), ScanOrder::Descending, 1)?
                    .first()
                    .and_then(|entry| s.entries.get(&entry.id))
                    .cloned(),
            )
        }))
    }
    fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        _: &Context,
    ) -> StorageFuture<Option<EntryRecord>> {
        ready(self.read(|s| {
            if !s.conversations.contains_key(&conversation) {
                return Err(Error::request(format!(
                    "Unknown conversation: {conversation}"
                )));
            }
            let mut current = conversation;
            let mut upper = at.unwrap_or(EntryId::from_raw(i64::MAX));
            let mut seen = HashSet::new();
            loop {
                if !seen.insert(current) {
                    return Err(Error::message("Conversation ancestry contains a cycle"));
                }
                if let Some(id) = s
                    .head_ids
                    .get(&current)
                    .and_then(|ids| ids.range(..=upper).next_back())
                {
                    return Ok(Some(s.entries[id].entry.clone()));
                }
                let record = s
                    .conversations
                    .get(&current)
                    .ok_or_else(|| Error::message(format!("Unknown conversation: {current}")))?;
                let Some(parent) = &record.parent else {
                    return Ok(None);
                };
                upper = upper.min(parent.at);
                current = parent.conversation_id;
            }
        }))
    }
    fn scan_entries(
        &self,
        q: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        ready(self.read(|s| {
            let start = scan::scan_start(q.order, cursor, ScanOrder::Descending)?;
            let (mut min, mut max) = (q.min_entry_id, q.max_entry_id);
            if let Some(after) = start.after {
                match start.order {
                    ScanOrder::Descending => {
                        let bound = EntryId::from_raw(after.saturating_sub(1));
                        max = Some(max.map_or(bound, |max| max.min(bound)));
                    }
                    ScanOrder::Ascending => {
                        let bound = EntryId::from_raw(after.saturating_add(1));
                        min = Some(min.map_or(bound, |min| min.max(bound)));
                    }
                }
            }
            let values = s
                .visible(
                    q.conversation_id,
                    min,
                    max,
                    start.order,
                    limit.saturating_add(1),
                )?
                .into_iter()
                .take(limit.saturating_add(1))
                .collect();
            Ok(page(values, limit, start.order, |v| v.id.get()))
        }))
    }
    fn task(&self, id: TaskId, _: &Context) -> StorageFuture<Option<TaskRecord>> {
        ready(self.read(|s| Ok(s.tasks.get(&id).cloned())))
    }
    fn scan_tasks(
        &self,
        q: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<TaskRecord>> {
        ready(self.read(|s| {
            let start = scan::scan_start(q.order, cursor, ScanOrder::Ascending)?;
            let candidates: Box<dyn DoubleEndedIterator<Item = &TaskRecord>> =
                if let Some(status) = q.status {
                    Box::new(
                        s.tasks_by_status
                            .get(&status)
                            .into_iter()
                            .flat_map(|ids| set_range(ids, &start, TaskId::from_raw))
                            .filter_map(|id| s.tasks.get(id)),
                    )
                } else {
                    Box::new(map_range(&s.tasks, &start, TaskId::from_raw).map(|(_, value)| value))
                };
            let values = ordered(candidates, start.order)
                .filter(|v| {
                    after(v.id().get(), &start)
                        && q.conversation_id
                            .is_none_or(|id| v.info.conversation_id == id)
                        && q.kind.as_ref().is_none_or(|kind| v.info.kind == *kind)
                        && q.abort_requested
                            .is_none_or(|abort| v.info.abort_requested == abort)
                        && q.background.is_none_or(|bg| v.info.background == bg)
                })
                .take(limit.saturating_add(1))
                .cloned()
                .collect();
            Ok(page(values, limit, start.order, |v| v.id().get()))
        }))
    }
    fn submission(&self, id: SubmissionId, _: &Context) -> StorageFuture<Option<SubmissionRecord>> {
        ready(self.read(|s| Ok(s.submissions.get(&id).cloned())))
    }
    fn scan_submissions(
        &self,
        q: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<SubmissionRecord>> {
        ready(self.read(|s| {
            let start = scan::scan_start(q.order, cursor, ScanOrder::Ascending)?;
            let candidates: Box<dyn DoubleEndedIterator<Item = &SubmissionRecord>> =
                if let Some(status) = q.status {
                    Box::new(
                        s.submissions_by_status
                            .get(&status)
                            .into_iter()
                            .flat_map(|ids| set_range(ids, &start, SubmissionId::from_raw))
                            .filter_map(|id| s.submissions.get(id)),
                    )
                } else {
                    Box::new(
                        map_range(&s.submissions, &start, SubmissionId::from_raw)
                            .map(|(_, value)| value),
                    )
                };
            let values = ordered(candidates, start.order)
                .filter(|v| {
                    after(v.id().get(), &start)
                        && q.conversation_id
                            .is_none_or(|id| v.info.conversation_id == id)
                })
                .take(limit.saturating_add(1))
                .cloned()
                .collect();
            Ok(page(values, limit, start.order, |v| v.id().get()))
        }))
    }
    fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &JsonString,
        _: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        ready(self.read(|s| {
            Ok(s.submissions_by_request
                .get(&(conversation, request.clone()))
                .and_then(|id| s.submissions.get(id))
                .cloned())
        }))
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _: &Context,
    ) -> StorageFuture<Option<DocumentRecord>> {
        ready(self.read(|s| {
            let Some(index) = s.addresses.get(address) else {
                return Ok(None);
            };
            Ok(match at {
                DocumentPoint::Current => index
                    .current
                    .and_then(|id| s.documents.get(&id))
                    .map(|d| d.record.clone()),
                DocumentPoint::At(_) => index
                    .ids
                    .iter()
                    .filter_map(|id| s.documents.get(id))
                    .find(|d| d.record.alive_at(at))
                    .map(|d| d.record.clone()),
            })
        }))
    }
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _: &Context,
    ) -> StorageFuture<Option<StoredDocument>> {
        ready(self.read(|s| s.materialize(id, at)))
    }
    fn scan_documents(
        &self,
        q: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<DocumentRecord>> {
        ready(self.read(|s| {
            let after = scan::cursor_id(cursor)?;
            let start = ScanStart {
                order: ScanOrder::Ascending,
                after,
            };
            let ids = s.documents_by_scope.get(&q.scope);
            let values = ids
                .into_iter()
                .flat_map(|ids| set_range(ids, &start, DocumentId::from_raw))
                .filter_map(|id| s.documents.get(id))
                .filter(|d| {
                    q.kind
                        .as_ref()
                        .is_none_or(|kind| d.record.create.kind == *kind)
                        && d.record.alive_at(q.at)
                })
                .take(limit.saturating_add(1))
                .map(|d| d.record.clone())
                .collect();
            Ok(page(values, limit, ScanOrder::Ascending, |v| v.id().get()))
        }))
    }
    fn close(&self, _: &Context) -> StorageFuture<()> {
        self.state.lock().closed = true;
        ready(Ok(()))
    }
}
