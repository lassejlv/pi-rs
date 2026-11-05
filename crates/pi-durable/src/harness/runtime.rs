//! Invocation-bound task operations. The scheduler supplies one driver per
//! reservation; typed task code cannot bypass its ordered commit gate.
use super::state::NextTaskState;
use crate::{
    storage::{StorageFuture, line::eager, ready},
    tasks::{decode, encode},
    *,
};
use parking_lot::Mutex;
use pi_chord::{Watch, context::AbortSignal};
use std::{future::Future, marker::PhantomData, sync::Arc};

pub(crate) type Change =
    Box<dyn FnOnce(Transaction, TaskRecord) -> StorageFuture<Option<TaskState>> + Send>;
pub(crate) trait TaskDriver: Send + Sync {
    fn check(&self) -> Result<()>;
    fn task_id(&self) -> TaskId;
    fn task_name(&self) -> JsonString;
    fn conversation_id(&self) -> ConversationId;
    fn signal(&self) -> AbortSignal;
    fn mode(&self) -> super::state::InvocationMode;
    fn session(&self) -> Session;
    fn registry(&self) -> super::reader::RegistryView;
    fn agent(&self, context: &Context) -> StorageFuture<super::agent::AgentView>;
    fn settings(&self) -> Result<super::settings::Settings>;
    fn models(&self) -> Result<pi_ai::models::Models>;
    fn model_collection(&self) -> Result<Arc<dyn super::models::ModelCollection>>;
    fn env(&self, context: &Context) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>>;
    fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<super::conversation::ConversationHandle>>;
    fn current(&self) -> Option<Arc<TaskRecord>>;
    fn commit(&self, change: Change, context: &Context) -> StorageFuture<()>;
    fn wait(&self, id: TaskId, context: &Context) -> StorageFuture<TaskRecord>;
    fn abort_owned(&self, id: TaskId, context: &Context) -> StorageFuture<()>;
    fn owned(&self, context: &Context) -> StorageFuture<Vec<TaskRecord>>;
    fn observe(&self, watch: Watch) -> Result<()>;
    fn context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        context: &Context,
    ) -> StorageFuture<super::context::ContextView>;
    fn now(&self) -> f64;
    fn set_timer(&self, callback: Box<dyn FnOnce() + Send>, delay_ms: f64) -> DisarmTimer;
    fn report(&self, error: Error);
    fn report_host(&self, error: Error, callback: &'static str);
    fn report_extension(&self, error: Error, extension: Option<JsonString>);
}

pub struct TaskRuntime<I = JsonValue, S = JsonValue, R = JsonValue, H = ()> {
    driver: Arc<dyn TaskDriver>,
    marker: PhantomData<fn(I, S, R, H) -> (I, S, R, H)>,
}
impl<I, S, R, H> Clone for TaskRuntime<I, S, R, H> {
    fn clone(&self) -> Self {
        Self {
            driver: self.driver.clone(),
            marker: PhantomData,
        }
    }
}
impl<I, S, R, H> TaskRuntime<I, S, R, H> {
    pub(crate) fn new(driver: Arc<dyn TaskDriver>) -> Self {
        Self {
            driver,
            marker: PhantomData,
        }
    }
    /// Lossless JSON view for runtime bindings. It keeps the invocation driver,
    /// admission gates and lifetimes; typed hook selection remains on the typed view.
    pub fn erased(&self) -> TaskRuntime<JsonValue, JsonValue, JsonValue, ()> {
        TaskRuntime::new(self.driver.clone())
    }
    /// Host callbacks may enter from any thread, outside the phase's poll scope.
    pub(crate) fn eager<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T>> + Send + 'static,
    ) -> StorageFuture<T> {
        crate::storage::line::eager_on(&self.driver.session(), operation)
    }
    pub fn task_id(&self) -> TypedTaskId<R> {
        TypedTaskId::from_untyped(self.driver.task_id())
    }
    pub fn same_invocation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.driver, &other.driver)
    }
    pub fn conversation_id(&self) -> ConversationId {
        self.driver.conversation_id()
    }
    pub fn signal(&self) -> AbortSignal {
        self.driver.signal()
    }
    /// The phase snapshot remains available through retained runtime handles.
    /// It changes only at a durable phase boundary, never on a registry install.
    pub fn registry(&self) -> super::reader::RegistryView {
        self.driver.registry()
    }
    pub fn agent(&self, context: &Context) -> StorageFuture<super::agent::AgentView> {
        self.read(|| self.driver.agent(context))
    }
    pub fn settings(&self) -> Result<super::settings::Settings> {
        self.driver.settings()
    }
    /// Read the current host collection, including through a retained runtime.
    pub fn models(&self) -> Result<pi_ai::models::Models> {
        self.driver.models()
    }
    pub fn model_collection(&self) -> Result<Arc<dyn super::models::ModelCollection>> {
        self.driver.model_collection()
    }
    pub fn env(
        &self,
        context: &Context,
    ) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>> {
        self.read(|| self.driver.env(context))
    }
    pub fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<super::conversation::ConversationHandle>> {
        self.read(|| self.driver.conversation(id, context))
    }
    pub fn hooks(&self) -> super::hooks::HookRunner<H>
    where
        H: Send + Sync + 'static,
    {
        super::hooks::HookRunner::new(self.driver.clone())
    }
    pub fn now(&self) -> Result<f64> {
        self.driver.check()?;
        Ok(self.driver.now())
    }
    pub fn set_timer(
        &self,
        callback: impl FnOnce() + Send + 'static,
        delay_ms: f64,
    ) -> DisarmTimer {
        self.driver.set_timer(Box::new(callback), delay_ms)
    }
    pub fn report(&self, error: Error) -> Result<()> {
        self.driver.check()?;
        self.driver.report(error);
        Ok(())
    }
    pub fn report_host(&self, error: Error, callback: &'static str) -> Result<()> {
        self.driver.check()?;
        self.driver.report_host(error, callback);
        Ok(())
    }
    pub fn report_extension(&self, error: Error, extension: Option<JsonString>) -> Result<()> {
        self.driver.check()?;
        self.driver.report_extension(error, extension);
        Ok(())
    }
    fn read<T: Send + 'static>(&self, read: impl FnOnce() -> StorageFuture<T>) -> StorageFuture<T> {
        match self.driver.check() {
            Ok(()) => read(),
            Err(error) => ready(Err(error)),
        }
    }
    pub fn memo<T: TaskValue>(
        &self,
        name: impl Into<JsonString>,
        _context: &Context,
    ) -> StorageFuture<Option<T>> {
        let name = name.into();
        self.read(|| {
            let result = self
                .driver
                .current()
                .and_then(|record| {
                    record
                        .state
                        .memos()
                        .and_then(|memos| memos.get(&name))
                        .cloned()
                })
                .map(|value| decode(&value))
                .transpose();
            ready(result)
        })
    }
    pub fn store_memo<T: TaskValue>(
        &self,
        name: impl Into<JsonString>,
        candidate: T,
        context: &Context,
    ) -> StorageFuture<T> {
        let name = name.into();
        let candidate = match encode(&candidate) {
            Ok(value) => value,
            Err(error) => return ready(Err(error)),
        };
        let winner = Arc::new(Mutex::new(None));
        let capture = winner.clone();
        let operation = self.driver.commit(
            Box::new(move |tx, mut current| {
                Box::pin(async move {
                    let existing = current
                        .state
                        .memos()
                        .and_then(|memos| memos.get(&name))
                        .cloned();
                    let value = if let Some(existing) = existing {
                        existing
                    } else {
                        let memos = match &mut current.state {
                            TaskState::Running { memos, .. } => {
                                memos.get_or_insert_with(JsonObject::default)
                            }
                            _ => {
                                return Err(Error::message(format!(
                                    "Task {} is not running",
                                    current.id()
                                )));
                            }
                        };
                        memos.insert(name, candidate.clone());
                        tx.set_task(current)?;
                        candidate
                    };
                    *capture.lock() = Some(value);
                    Ok(None)
                })
            }),
            context,
        );
        Box::pin(async move {
            operation.await?;
            decode(
                &winner
                    .lock()
                    .take()
                    .expect("successful memo commit captures its winner"),
            )
        })
    }
    pub fn get_task<T: TaskValue>(
        &self,
        id: TypedTaskId<T>,
        context: &Context,
    ) -> StorageFuture<Option<TaskRecord<JsonValue, JsonValue, T>>> {
        self.read(|| {
            let storage = self.driver.session().guarded_storage();
            let context = context.clone();
            self.driver.session().read_on_line(move || async move {
                storage
                    .task(id.untyped(), &context)
                    .await?
                    .map(|record| decode(&encode(&record)?))
                    .transpose()
            })
        })
    }
    pub fn wait_for_task<T: TaskValue>(
        &self,
        id: TypedTaskId<T>,
        context: &Context,
    ) -> StorageFuture<TaskRecord<JsonValue, JsonValue, T>> {
        self.read(|| {
            let wait = self.driver.wait(id.untyped(), context);
            Box::pin(async move { decode(&encode(&wait.await?)?) })
        })
    }
    pub fn abort_owned<T>(&self, id: TypedTaskId<T>, context: &Context) -> StorageFuture<()> {
        self.read(|| self.driver.abort_owned(id.untyped(), context))
    }
    pub fn owned_tasks(&self, context: &Context) -> StorageFuture<Vec<TaskRecord>> {
        self.read(|| self.driver.owned(context))
    }
    pub fn outcomes<T: TaskValue>(
        &self,
        ids: &[TypedTaskId<T>],
        context: &Context,
    ) -> StorageFuture<Vec<TaskOutcome<T>>> {
        let ids = ids.iter().map(|id| id.untyped()).collect::<Vec<_>>();
        self.read(|| {
            let storage = self.driver.session().guarded_storage();
            let context = context.clone();
            self.driver.session().read_on_line(move || async move {
                let mut outcomes = Vec::new();
                for id in ids {
                    let Some(TaskRecord {
                        state: TaskState::Terminal { outcome },
                        ..
                    }) = storage.task(id, &context).await?
                    else {
                        return Err(Error::message(format!("Task {id} is not terminal")));
                    };
                    outcomes.push(decode(&encode(&outcome)?)?);
                }
                Ok(outcomes)
            })
        })
    }
    pub fn entry(&self, id: EntryId, context: &Context) -> StorageFuture<Option<EntryRecord>> {
        self.read(|| {
            let storage = self.driver.session().guarded_storage();
            let conversation = self.conversation_id();
            let context = context.clone();
            self.driver.session().read_on_line(move || async move {
                Ok(storage
                    .entry_in(conversation, id, &context)
                    .await?
                    .map(|entry| entry.entry))
            })
        })
    }
    pub fn context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        context: &Context,
    ) -> StorageFuture<super::context::ContextView> {
        self.read(|| self.driver.context(conversation, at, context))
    }
    pub fn snapshot_as_of<T: DocumentState, D: RewindableScope>(
        &self,
        token: &DocToken<T, D>,
        owner: D::Owner,
        at: EntryId,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>> {
        self.read(|| {
            self.driver
                .session()
                .snapshot_as_of(token, owner, at, context)
        })
    }
    pub fn snapshot_family_as_of<T: DocumentState, Input, D: RewindableScope>(
        &self,
        token: &DocFamilyToken<T, Input, D>,
        owner: D::Owner,
        key: impl Into<JsonString>,
        at: EntryId,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>> {
        self.read(|| {
            self.driver
                .session()
                .snapshot_family_as_of(token, owner, key, at, context)
        })
    }
    pub fn snapshot<T: DocumentState, D: DocScope>(
        &self,
        token: &DocToken<T, D>,
        scope: D::Owner,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>> {
        self.read(|| self.driver.session().snapshot(token, scope, context))
    }
    pub fn snapshot_raw(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
        context: &Context,
    ) -> StorageFuture<Option<Arc<JsonValue>>> {
        self.read(|| {
            self.driver
                .session()
                .snapshot_raw(definition, address, context)
        })
    }
    pub fn snapshot_as_of_raw(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
        at: EntryId,
        context: &Context,
    ) -> StorageFuture<Option<Arc<JsonValue>>> {
        self.read(|| {
            self.driver
                .session()
                .snapshot_as_of_raw(definition, address, at, context)
        })
    }
    pub fn watch_doc_raw(
        &self,
        definition: Arc<AnyDocDefinition>,
        address: DocumentAddress,
        context: &Context,
    ) -> StorageFuture<Option<Watch>> {
        self.read(|| {
            let acquisition = self
                .driver
                .session()
                .watch_raw(definition, address, context);
            let driver = self.driver.clone();
            eager(async move {
                let watch = acquisition.await?;
                if let Some(watch) = &watch
                    && let Err(error) = driver.observe(watch.clone())
                {
                    watch.stop();
                    return Err(error);
                }
                Ok(watch)
            })
        })
    }
    pub fn snapshot_family<T: DocumentState, Input, D: DocScope>(
        &self,
        token: &DocFamilyToken<T, Input, D>,
        scope: D::Owner,
        key: impl Into<JsonString>,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>> {
        self.read(|| {
            self.driver
                .session()
                .snapshot_family(token, scope, key, context)
        })
    }
    pub fn watch_doc<T: DocumentState, D: DocScope>(
        &self,
        token: &DocToken<T, D>,
        scope: D::Owner,
        context: &Context,
    ) -> StorageFuture<Option<DocumentWatch<T>>> {
        self.read(|| self.bind_observation(self.driver.session().watch_doc(token, scope, context)))
    }
    pub fn watch_doc_family<T: DocumentState, Input, D: DocScope>(
        &self,
        token: &DocFamilyToken<T, Input, D>,
        owner: D::Owner,
        key: impl Into<JsonString>,
        context: &Context,
    ) -> StorageFuture<Option<DocumentWatch<T>>> {
        self.read(|| {
            self.bind_observation(
                self.driver
                    .session()
                    .watch_doc_family(token, owner, key, context),
            )
        })
    }
    fn bind_observation<T: DocumentState>(
        &self,
        acquisition: StorageFuture<Option<DocumentWatch<T>>>,
    ) -> StorageFuture<Option<DocumentWatch<T>>> {
        let driver = self.driver.clone();
        eager(async move {
            let watch = acquisition.await?;
            if let Some(watch) = &watch
                && let Err(error) = driver.observe(watch.raw().clone())
            {
                watch.stop();
                return Err(error);
            }
            Ok(watch)
        })
    }
}
impl<I: TaskValue, S: TaskValue, R: TaskValue, H: Send + Sync + 'static> TaskRuntime<I, S, R, H> {
    pub fn commit<F, Fut>(&self, change: F, context: &Context) -> StorageFuture<()>
    where
        F: FnOnce(Transaction, RunningTask<I, S, R>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<NextTaskState<S, R>>>> + Send + 'static,
    {
        let mode = self.driver.mode();
        let task_id = self.driver.task_id();
        self.driver.commit(
            Box::new(move |tx, record| {
                Box::pin(async move {
                    let current = RunningTask::try_from_record(decode(&encode(&record)?)?)?;
                    let next = change(tx, current).await?;
                    // Validate before JSON conversion: serde encodes a native
                    // nonfinite f64 as null, which would lose the original time
                    // and its reference error message.
                    if let Some(NextTaskState::Sleeping { until, .. }) = &next {
                        super::state::validate_sleep(mode, task_id, *until)?;
                    }
                    next.map(|next| decode(&encode(&next.into_state())?))
                        .transpose()
                })
            }),
            context,
        )
    }
    pub fn sleep_until(&self, until: f64, checkpoint: S, context: &Context) -> StorageFuture<()> {
        self.commit(
            move |_, _| async move { Ok(Some(NextTaskState::Sleeping { until, checkpoint })) },
            context,
        )
    }
}
