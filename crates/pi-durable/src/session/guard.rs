use super::panic_error;
use crate::{
    storage::{Adopt, StorageFuture, ready},
    *,
};
use futures::FutureExt;
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};

#[derive(Default)]
struct State {
    failure: Option<Error>,
    closing: bool,
    active: usize,
}
type OnFailure = Arc<dyn Fn(Error) + Send + Sync>;
pub(crate) struct GuardedStorage {
    backend: Arc<dyn Storage>,
    state: Mutex<State>,
    changed: tokio::sync::watch::Sender<usize>,
    failure_changed: tokio::sync::watch::Sender<Option<Error>>,
    on_failure: Mutex<Option<OnFailure>>,
}
impl GuardedStorage {
    pub fn new(backend: Arc<dyn Storage>) -> Arc<Self> {
        let (changed, _) = tokio::sync::watch::channel(0);
        let (failure_changed, _) = tokio::sync::watch::channel(None);
        Arc::new(Self {
            backend,
            state: Mutex::new(State::default()),
            changed,
            failure_changed,
            on_failure: Mutex::new(None),
        })
    }
    pub fn on_failure(&self, callback: OnFailure) {
        *self.on_failure.lock() = Some(callback);
    }
    pub fn healthy(&self) -> Result<()> {
        self.state
            .lock()
            .failure
            .clone()
            .map_or(Ok(()), |e| Err(Error::failed(e)))
    }
    pub fn failure(&self) -> Option<Error> {
        self.state.lock().failure.clone()
    }
    pub fn failed(&self) -> StorageFuture<()> {
        let mut failed = self.failure_changed.subscribe();
        Box::pin(async move {
            loop {
                if let Some(error) = failed.borrow_and_update().clone() {
                    return Err(Error::failed(error));
                }
                if failed.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
    }
    pub fn fail(&self, error: Error) {
        if let Some(notify) = self.latch_failure(error) {
            notify();
        }
    }
    /// Latch under admission's lock, then notify after releasing it: failure
    /// callbacks can synchronously close the Session and acquire that lock.
    pub(super) fn latch_failure(&self, error: Error) -> Option<impl FnOnce() + '_> {
        {
            let mut state = self.state.lock();
            if state.failure.is_some() {
                return None;
            }
            state.failure = Some(error.clone());
        }
        Some(move || {
            self.failure_changed.send_replace(Some(error.clone()));
            let callback = self.on_failure.lock().clone();
            if let Some(callback) = callback {
                callback(error);
            }
        })
    }
    fn call<T, F, Fut>(self: &Arc<Self>, read: Option<Context>, operation: F) -> StorageFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn Storage>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        {
            let mut state = self.state.lock();
            if let Some(error) = &state.failure {
                return ready(Err(Error::failed(error.clone())));
            }
            if state.closing {
                return ready(Err(Error::message("Session is closed")));
            }
            state.active += 1;
        }
        let this = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        pi_chord::rt::spawn(async move {
            let result =
                std::panic::AssertUnwindSafe(async { operation(this.backend.clone()).await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|p| Err(panic_error(p)));
            let result = match result {
                Ok(value) => this.healthy().map(|()| value),
                Err(error) => {
                    if let Some(previous) = this.failure() {
                        Err(Error::failed(previous))
                    } else {
                        let exempt = read
                            .as_ref()
                            .is_some_and(|ctx| error.is_request() || ctx.check().is_err());
                        if !exempt {
                            this.fail(error.clone());
                        }
                        Err(error)
                    }
                }
            };
            let active = {
                let mut state = this.state.lock();
                state.active -= 1;
                state.active
            };
            this.changed.send_replace(active);
            let _ = send.send(result);
        });
        Box::pin(async move {
            receive
                .await
                .map_err(|_| Error::message("Storage operation stopped"))?
        })
    }
    pub fn close_backend(self: &Arc<Self>, context: Context) -> StorageFuture<()> {
        self.state.lock().closing = true;
        let this = self.clone();
        Box::pin(async move {
            let mut changed = this.changed.subscribe();
            loop {
                if this.state.lock().active == 0 {
                    break;
                }
                if changed.changed().await.is_err() {
                    break;
                }
            }
            let result = std::panic::AssertUnwindSafe(async { this.backend.close(&context).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|p| Err(panic_error(p)));
            if let Err(error) = &result {
                this.fail(error.clone());
            }
            result
        })
    }
}

/// Keep a cloneable authority handle so every call is eager and survives a
/// dropped caller future. Backend shutdown waits for the actual operations.
pub(crate) struct Guard(pub Arc<GuardedStorage>);
macro_rules! read_method {
    ($name:ident($($arg:ident:$ty:ty),*) -> $out:ty) => {
        fn $name(&self,$($arg:$ty,)* context:&Context)->StorageFuture<$out> {
            $(let $arg=$arg.to_owned();)*
            let context=context.clone();
            self.0.call(Some(context.clone()),move |backend| async move {backend.$name($($arg,)* &context).await})
        }
    };
}
impl Storage for Guard {
    fn commit(
        &self,
        writes: &[StorageWrite],
        adopt: Adopt,
        context: &Context,
    ) -> StorageFuture<Seq> {
        let writes = writes.to_vec();
        let context = context.clone();
        self.0.call(None, move |backend| async move {
            backend.commit(&writes, adopt, &context).await
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        self.0
            .call(None, |backend| async move { backend.mint_id().await })
    }
    read_method!(conversation(id:ConversationId)->Option<ConversationRecord>);
    read_method!(entry(id:EntryId)->Option<crate::storage::StoredEntry>);
    read_method!(entry_in(conversation:ConversationId,id:EntryId)->Option<crate::storage::StoredEntry>);
    read_method!(find_latest_head_marker(conversation:ConversationId,at:Option<EntryId>)->Option<EntryRecord>);
    read_method!(task(id:TaskId)->Option<TaskRecord>);
    read_method!(submission(id:SubmissionId)->Option<SubmissionRecord>);
    fn scan_conversations(
        &self,
        q: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        ctx: &Context,
    ) -> StorageFuture<Page<ConversationRecord>> {
        let (q, cursor, ctx) = (q.clone(), cursor.cloned(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.scan_conversations(&q, limit, cursor.as_ref(), &ctx).await
        })
    }
    fn scan_entries(
        &self,
        q: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        ctx: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        let (q, cursor, ctx) = (q.clone(), cursor.cloned(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.scan_entries(&q, limit, cursor.as_ref(), &ctx).await
        })
    }
    fn scan_tasks(
        &self,
        q: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        ctx: &Context,
    ) -> StorageFuture<Page<TaskRecord>> {
        let (q, cursor, ctx) = (q.clone(), cursor.cloned(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.scan_tasks(&q, limit, cursor.as_ref(), &ctx).await
        })
    }
    fn scan_submissions(
        &self,
        q: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        ctx: &Context,
    ) -> StorageFuture<Page<SubmissionRecord>> {
        let (q, cursor, ctx) = (q.clone(), cursor.cloned(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.scan_submissions(&q, limit, cursor.as_ref(), &ctx).await
        })
    }
    fn submission_by_request(
        &self,
        id: ConversationId,
        request: &JsonString,
        ctx: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        let (request, ctx) = (request.clone(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.submission_by_request(id, &request, &ctx).await
        })
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        ctx: &Context,
    ) -> StorageFuture<Option<DocumentRecord>> {
        let (address, ctx) = (address.clone(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.find_document(&address, at, &ctx).await
        })
    }
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        ctx: &Context,
    ) -> StorageFuture<Option<StoredDocument>> {
        let ctx = ctx.clone();
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.document(id, at, &ctx).await
        })
    }
    fn scan_documents(
        &self,
        q: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        ctx: &Context,
    ) -> StorageFuture<Page<DocumentRecord>> {
        let (q, cursor, ctx) = (q.clone(), cursor.cloned(), ctx.clone());
        self.0.call(Some(ctx.clone()), move |b| async move {
            b.scan_documents(&q, limit, cursor.as_ref(), &ctx).await
        })
    }
    fn close(&self, context: &Context) -> StorageFuture<()> {
        self.0.close_backend(context.clone())
    }
}
