use parking_lot::Mutex;
use pi_durable::{
    storage::{Adopt, StorageFuture, StoredEntry},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
pub struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl Gate {
    pub async fn entered(&self) {
        self.entered.notified().await;
    }
    pub fn release(&self) {
        self.release.notify_one();
    }
    async fn wait(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}
#[derive(Clone)]
pub enum Fault {
    Before(Error),
    After(Error),
    SkipAdopt,
}
#[derive(Default)]
pub struct ControlledStorage {
    pub memory: Arc<MemoryStorage>,
    pub writes: Mutex<Vec<Vec<StorageWrite>>>,
    pub fault: Mutex<Option<Fault>>,
    pub commit_gate: Mutex<Option<Arc<Gate>>>,
    pub after_adopt: Mutex<Option<Arc<Gate>>>,
    pub find_gate: Mutex<Option<Arc<Gate>>>,
    pub submission_gate: Mutex<Option<Arc<Gate>>>,
    pub read_error: Mutex<Option<Error>>,
    pub mint_error: Mutex<Option<Error>>,
    pub close_error: Mutex<Option<Error>>,
    pub closes: Arc<AtomicUsize>,
    pub mints: AtomicUsize,
    pub entry_rows: Arc<AtomicUsize>,
    pub range_gate: Mutex<Option<Arc<Gate>>>,
    /// Runs synchronously inside the next head-marker lookup, while its caller is polled.
    pub head_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
macro_rules! forward {
    ($method:ident($($arg:ident:$ty:ty),*) -> $out:ty) => {
        fn $method(&self,$($arg:$ty,)*ctx:&Context)->StorageFuture<$out>{self.memory.$method($($arg,)*ctx)}
    };
}
impl Storage for ControlledStorage {
    fn commit(&self, writes: &[StorageWrite], adopt: Adopt, ctx: &Context) -> StorageFuture<Seq> {
        self.writes.lock().push(writes.to_vec());
        let (memory, writes, ctx) = (self.memory.clone(), writes.to_vec(), ctx.clone());
        let fault = self.fault.lock().take();
        let gate = self.commit_gate.lock().take();
        let after = self.after_adopt.lock().take();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            if let Some(Fault::Before(e)) = &fault {
                return Err(e.clone());
            }
            let adopt = if matches!(fault, Some(Fault::SkipAdopt)) {
                Box::new(|_| {}) as Adopt
            } else {
                adopt
            };
            let seq = memory.commit(&writes, adopt, &ctx).await?;
            if let Some(gate) = after {
                gate.wait().await;
            }
            if let Some(Fault::After(e)) = fault {
                return Err(e);
            }
            Ok(seq)
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.mints.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.mint_error.lock().take() {
            return Box::pin(async { Err(e) });
        }
        self.memory.mint_id()
    }
    fn conversation(
        &self,
        id: ConversationId,
        ctx: &Context,
    ) -> StorageFuture<Option<ConversationRecord>> {
        if let Some(error) = self.read_error.lock().take() {
            return Box::pin(async { Err(error) });
        }
        self.memory.conversation(id, ctx)
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        ctx: &Context,
    ) -> StorageFuture<Option<DocumentRecord>> {
        let (memory, address, ctx, gate) = (
            self.memory.clone(),
            address.clone(),
            ctx.clone(),
            self.find_gate.lock().take(),
        );
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            memory.find_document(&address, at, &ctx).await
        })
    }
    forward!(scan_conversations(query:&ConversationQuery,limit:usize,cursor:Option<&Cursor>)->Page<ConversationRecord>);
    forward!(entry(id:EntryId)->Option<StoredEntry>);
    forward!(entry_in(conversation:ConversationId,id:EntryId)->Option<StoredEntry>);
    fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        ctx: &Context,
    ) -> StorageFuture<Option<EntryRecord>> {
        if let Some(hook) = self.head_hook.lock().take() {
            hook();
        }
        self.memory.find_latest_head_marker(conversation, at, ctx)
    }
    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        let memory = self.memory.clone();
        let query = query.clone();
        let cursor = cursor.cloned();
        let context = context.clone();
        let gate = if limit > 1 {
            self.range_gate.lock().take()
        } else {
            None
        };
        let rows = self.entry_rows.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            let page = memory
                .scan_entries(&query, limit, cursor.as_ref(), &context)
                .await?;
            rows.fetch_add(page.items.len(), Ordering::SeqCst);
            Ok(page)
        })
    }
    forward!(task(id:TaskId)->Option<TaskRecord>);
    forward!(scan_tasks(query:&TaskQuery,limit:usize,cursor:Option<&Cursor>)->Page<TaskRecord>);
    fn submission(
        &self,
        id: SubmissionId,
        ctx: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        let (memory, context, gate) = (
            self.memory.clone(),
            ctx.clone(),
            self.submission_gate.lock().take(),
        );
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            memory.submission(id, &context).await
        })
    }
    forward!(scan_submissions(query:&SubmissionQuery,limit:usize,cursor:Option<&Cursor>)->Page<SubmissionRecord>);
    forward!(submission_by_request(conversation:ConversationId,request:&JsonString)->Option<SubmissionRecord>);
    forward!(document(id:DocumentId,at:DocumentPoint)->Option<StoredDocument>);
    forward!(scan_documents(query:&DocumentQuery,limit:usize,cursor:Option<&Cursor>)->Page<DocumentRecord>);
    fn close(&self, ctx: &Context) -> StorageFuture<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.close_error.lock().take() {
            return Box::pin(async { Err(error) });
        }
        self.memory.close(ctx)
    }
}
