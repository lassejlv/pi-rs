use crate::{Context, Result, types::*};
use std::{future::Future, pin::Pin};
pub mod indexeddb;
pub mod jsonl;
pub(crate) mod line;
pub mod memory;
pub mod scan;
pub mod sqlite;
pub use indexeddb::{IndexedDbStorage, MemoryIdbFactory};
pub use jsonl::{JsonlStorage, JsonlStorageOptions};
pub use memory::{MemoryStorage, PreparedMemoryCommit};
pub use sqlite::{SqliteStorage, SqliteStorageOptions};

pub type StorageFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;
pub type Adopt = Box<dyn FnOnce(Seq) + Send + 'static>;
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEntry {
    pub entry: EntryRecord,
    pub commit_seq: Seq,
}

/// Backend authority. A successful commit calls adoption exactly once after
/// durability, before another commit may begin; returned reads are detached.
pub trait Storage: Send + Sync {
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq>;
    fn mint_id(&self) -> StorageFuture<i64>;
    fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<ConversationRecord>>;
    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<ConversationRecord>>;
    fn entry(&self, id: EntryId, context: &Context) -> StorageFuture<Option<StoredEntry>>;
    fn entry_in(
        &self,
        conversation: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> StorageFuture<Option<StoredEntry>>;
    fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> StorageFuture<Option<EntryRecord>>;
    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<EntryRecord>>;
    fn task(&self, id: TaskId, context: &Context) -> StorageFuture<Option<TaskRecord>>;
    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<TaskRecord>>;
    fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>>;
    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<SubmissionRecord>>;
    fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &crate::JsonString,
        context: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>>;
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageFuture<Option<DocumentRecord>>;
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> StorageFuture<Option<StoredDocument>>;
    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<DocumentRecord>>;
    fn close(&self, context: &Context) -> StorageFuture<()>;
}
pub(crate) fn ready<T: Send + 'static>(result: Result<T>) -> StorageFuture<T> {
    Box::pin(async { result })
}
