use parking_lot::RwLock;
use pi_durable::{
    storage::{Adopt, StorageFuture, StoredEntry},
    *,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
pub struct ReopeningJsonl {
    current: Arc<RwLock<JsonlStorage>>,
    directory: PathBuf,
    options: JsonlStorageOptions,
    closed: AtomicBool,
}
impl ReopeningJsonl {
    pub fn new(storage: JsonlStorage, options: JsonlStorageOptions) -> Self {
        let directory = storage.directory().to_owned();
        Self {
            current: Arc::new(RwLock::new(storage)),
            directory,
            options,
            closed: AtomicBool::new(false),
        }
    }
}
macro_rules! forward {
    ($method:ident($($arg:ident:$ty:ty),*) -> $out:ty)=>{
        fn $method(&self,$($arg:$ty,)*ctx:&Context)->StorageFuture<$out>{self.current.read().$method($($arg,)*ctx)}
    };
}
impl Storage for ReopeningJsonl {
    fn commit(&self, writes: &[StorageWrite], adopt: Adopt, ctx: &Context) -> StorageFuture<Seq> {
        let storage = self.current.read().clone();
        let future = storage.commit(writes, adopt, ctx);
        if self.closed.load(Ordering::Acquire) {
            return future;
        }
        let current = self.current.clone();
        let directory = self.directory.clone();
        let options = self.options;
        Box::pin(async move {
            let result = future.await;
            storage.close(&Context::default()).await?;
            let reopened =
                JsonlStorage::open_native(directory, &Context::default(), options).await?;
            *current.write() = reopened;
            result
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.current.read().mint_id()
    }
    forward!(conversation(id:ConversationId)->Option<ConversationRecord>);
    forward!(scan_conversations(query:&ConversationQuery,limit:usize,cursor:Option<&Cursor>)->Page<ConversationRecord>);
    forward!(entry(id:EntryId)->Option<StoredEntry>);
    forward!(entry_in(conversation:ConversationId,id:EntryId)->Option<StoredEntry>);
    forward!(find_latest_head_marker(conversation:ConversationId,at:Option<EntryId>)->Option<EntryRecord>);
    forward!(scan_entries(query:&EntryQuery,limit:usize,cursor:Option<&Cursor>)->Page<EntryRecord>);
    forward!(task(id:TaskId)->Option<TaskRecord>);
    forward!(scan_tasks(query:&TaskQuery,limit:usize,cursor:Option<&Cursor>)->Page<TaskRecord>);
    forward!(submission(id:SubmissionId)->Option<SubmissionRecord>);
    forward!(scan_submissions(query:&SubmissionQuery,limit:usize,cursor:Option<&Cursor>)->Page<SubmissionRecord>);
    forward!(submission_by_request(conversation:ConversationId,request:&JsonString)->Option<SubmissionRecord>);
    forward!(find_document(address:&DocumentAddress,at:DocumentPoint)->Option<DocumentRecord>);
    forward!(document(id:DocumentId,at:DocumentPoint)->Option<StoredDocument>);
    forward!(scan_documents(query:&DocumentQuery,limit:usize,cursor:Option<&Cursor>)->Page<DocumentRecord>);
    fn close(&self, context: &Context) -> StorageFuture<()> {
        self.closed.store(true, Ordering::Release);
        self.current.read().close(context)
    }
}
