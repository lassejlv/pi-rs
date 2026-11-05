use super::*;
use pi_durable::storage::{Adopt, StoredEntry};

/// The original stock test owns this backend, including any gates or failures.
/// Rust owns its Session and scheduler, and sends every backend operation here.
pub struct RemoteStorage {
    pub hub: Arc<Hub>,
    pub handle: u64,
}
impl RemoteStorage {
    fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        args: JsonValue,
        context: &Context,
    ) -> StorageFuture<T> {
        let call = self.hub.call_context(
            "storage",
            object([
                ("handle", encode(&self.handle)),
                ("method", encode(&method)),
                ("args", args),
            ]),
            context,
        );
        Box::pin(async move { decode(&call.await?.unwrap_or(JsonValue::Null)) })
    }
}
macro_rules! read {
    ($name:ident,$wire:literal,($($arg:ident:$ty:ty),*),$out:ty) => {
        fn $name(&self,$($arg:$ty,)* context:&Context)->StorageFuture<$out> { self.call($wire, encode(&($($arg,)*)), context) }
    };
}
impl Storage for RemoteStorage {
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        let id = self.hub.next.fetch_add(1, Ordering::Relaxed);
        self.hub.adoptions.lock().insert(id, adopt);
        let mut args = encode(&(writes, id));
        args.as_array_mut()
            .unwrap()
            .push(self.hub.native_storage_identities(writes));
        let operation = self.call("commit", args, context);
        let hub = self.hub.clone();
        Box::pin(async move {
            let result = operation.await;
            hub.adoptions.lock().remove(&id);
            result
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.call("mintId", encode(&Vec::<u8>::new()), &Context::default())
    }
    read!(conversation,"conversation",(id:ConversationId),Option<ConversationRecord>);
    read!(scan_conversations,"scanConversations",(query:&ConversationQuery,limit:usize,cursor:Option<&Cursor>),Page<ConversationRecord>);
    read!(entry,"entry",(id:EntryId),Option<StoredEntry>);
    read!(entry_in,"entry",(conversation:ConversationId,id:EntryId),Option<StoredEntry>);
    read!(find_latest_head_marker,"findLatestHeadMarker",(conversation:ConversationId,at:Option<EntryId>),Option<EntryRecord>);
    read!(scan_entries,"scanEntries",(query:&EntryQuery,limit:usize,cursor:Option<&Cursor>),Page<EntryRecord>);
    read!(task,"task",(id:TaskId),Option<TaskRecord>);
    read!(scan_tasks,"scanTasks",(query:&TaskQuery,limit:usize,cursor:Option<&Cursor>),Page<TaskRecord>);
    read!(submission,"submission",(id:SubmissionId),Option<SubmissionRecord>);
    read!(scan_submissions,"scanSubmissions",(query:&SubmissionQuery,limit:usize,cursor:Option<&Cursor>),Page<SubmissionRecord>);
    read!(submission_by_request,"submissionByRequest",(conversation:ConversationId,request:&JsonString),Option<SubmissionRecord>);
    read!(find_document,"findDocument",(address:&DocumentAddress,at:DocumentPoint),Option<DocumentRecord>);
    read!(document,"document",(id:DocumentId,at:DocumentPoint),Option<StoredDocument>);
    read!(scan_documents,"scanDocuments",(query:&DocumentQuery,limit:usize,cursor:Option<&Cursor>),Page<DocumentRecord>);
    fn close(&self, context: &Context) -> StorageFuture<()> {
        self.call("close", encode(&Vec::<u8>::new()), context)
    }
}
