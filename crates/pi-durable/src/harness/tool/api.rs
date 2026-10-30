use super::*;
use crate::{
    storage::{StorageFuture, line::eager, ready},
    *,
};
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use parking_lot::Mutex;
use pi_chord::{
    context::{AwaitError, await_with_context},
    output::{OutputBuffer, OutputLimits, OutputSkip, PROGRESS_BYTES_PER_SECOND, Retain},
};
use std::{future::Future, ops::Deref, sync::Arc};

type Admission = Shared<BoxFuture<'static, Result<TypedTaskId<ToolTaskResult>>>>;
#[derive(Default)]
struct Admissions {
    sequence: u64,
    generation: u64,
    memo: Option<Shared<BoxFuture<'static, Result<bool>>>>,
    pending: Vec<Admission>,
}
pub(crate) struct Reported {
    pub output: OutputBuffer,
    pub details: Option<Arc<JsonValue>>,
    pub diagnostics: Vec<ToolDiagnostic>,
}
#[derive(Default)]
struct Lifetime {
    ended: bool,
    active: usize,
}
pub(crate) struct ToolScope {
    pub limits: OutputLimits,
    life: Mutex<Lifetime>,
    changed: tokio::sync::Notify,
    pub reported: Arc<Mutex<Reported>>,
    pub progress: pi_chord::output::ProgressWithError<Error>,
    admissions: Mutex<Admissions>,
}
struct Lease(Arc<ToolScope>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.life.lock().active -= 1;
        self.0.changed.notify_waiters();
    }
}
impl ToolScope {
    pub fn new(
        runtime: &ToolRuntime,
        limits: OutputLimits,
        enabled: bool,
        context: &Context,
    ) -> Result<Arc<Self>> {
        let reported = Arc::new(Mutex::new(Reported {
            output: OutputBuffer::new(limits),
            details: None,
            diagnostics: Vec::new(),
        }));
        let progress =
            super::progress::publish(runtime.clone(), reported.clone(), enabled, context.clone())?;
        Ok(Arc::new(Self {
            limits,
            life: Mutex::new(Lifetime::default()),
            changed: tokio::sync::Notify::new(),
            reported,
            progress,
            admissions: Mutex::new(Admissions::default()),
        }))
    }
    fn acquire(self: &Arc<Self>, call: &JsonString) -> Result<Lease> {
        let mut life = self.life.lock();
        if life.ended {
            return Err(Error::host(
                "Error",
                result::join(
                    ["Tool call ".into(), call.clone(), " has settled".into()],
                    "",
                ),
            ));
        }
        life.active += 1;
        Ok(Lease(self.clone()))
    }
    pub async fn end(&self) {
        loop {
            let notified = self.changed.notified();
            {
                let mut life = self.life.lock();
                life.ended = true;
                if life.active == 0 {
                    break;
                }
            }
            notified.await;
        }
    }
    pub async fn drain_admissions(&self) {
        let pending = std::mem::take(&mut self.admissions.lock().pending);
        futures::future::join_all(pending).await;
    }
}
#[derive(Clone, Debug)]
pub struct OutputWindow {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub min_interval_ms: f64,
    pub bytes_per_second: usize,
}
#[derive(Clone, Debug)]
pub struct RetainedOutput {
    pub text: JsonString,
    pub truncated: bool,
}
#[derive(Clone, Default)]
pub struct NestedCallOptions {
    pub key: Option<JsonString>,
    pub progress: Option<bool>,
}
/// Omission and explicit false have different replay inheritance semantics.
pub struct ToolTaskOptions {
    pub ownership: TaskOwnership,
    pub trace: Option<TaskTrace>,
    pub links: Option<Vec<JsonObject>>,
    pub background: bool,
    pub abandon_on_restart: Option<bool>,
    pub submission_ids: Option<Vec<SubmissionId>>,
}
impl ToolTaskOptions {
    pub fn new(ownership: TaskOwnership) -> Self {
        Self {
            ownership,
            trace: None,
            links: None,
            background: false,
            abandon_on_restart: None,
            submission_ids: None,
        }
    }
}
#[derive(Clone)]
pub struct ToolExecutionApi {
    runtime: ToolRuntime,
    pub call_id: JsonString,
    pub env: Option<Arc<dyn crate::env::ExecutionEnv>>,
    pub output_window: Option<OutputWindow>,
    pub(crate) scope: Arc<ToolScope>,
    resumes: bool,
}
// Read/memo/watch/conversation operations keep the invocation's own guard, which
// outlives execute while afterTool and the terminal commit are still running.
impl Deref for ToolExecutionApi {
    type Target = ToolRuntime;
    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}
impl ToolExecutionApi {
    pub(crate) fn new(
        runtime: ToolRuntime,
        call_id: JsonString,
        env: Option<Arc<dyn crate::env::ExecutionEnv>>,
        scope: Arc<ToolScope>,
        limits: OutputLimits,
        resumes: bool,
    ) -> Result<Self> {
        let output_window = if limits.retain == Retain::Tail {
            Some(OutputWindow {
                max_bytes: limits.max_bytes,
                max_lines: limits.max_lines,
                min_interval_ms: runtime.settings()?.progress.output_interval_ms,
                bytes_per_second: PROGRESS_BYTES_PER_SECOND,
            })
        } else {
            None
        };
        Ok(Self {
            runtime,
            call_id,
            env,
            output_window,
            scope,
            resumes,
        })
    }
    pub fn output(&self, chunk: impl Into<JsonString>, skipped: Option<OutputSkip>) -> Result<()> {
        let _lease = self.scope.acquire(&self.call_id)?;
        let changed = self
            .scope
            .reported
            .lock()
            .output
            .push_text(chunk, skipped)
            .map_err(Error::message)?;
        if changed {
            self.scope.progress.mark();
        }
        Ok(())
    }
    pub fn output_bytes(&self, chunk: &[u8], skipped: Option<OutputSkip>) -> Result<()> {
        let _lease = self.scope.acquire(&self.call_id)?;
        let changed = self
            .scope
            .reported
            .lock()
            .output
            .push_bytes(chunk, skipped)
            .map_err(Error::message)?;
        if changed {
            self.scope.progress.mark();
        }
        Ok(())
    }
    pub fn retained_output(&self) -> Result<RetainedOutput> {
        let _lease = self.scope.acquire(&self.call_id)?;
        let output = self.scope.reported.lock().output.snapshot();
        Ok(RetainedOutput {
            text: output.text,
            truncated: output.dropped_bytes > 0,
        })
    }
    pub fn diagnostic(&self, diagnostic: ToolDiagnostic) -> Result<()> {
        let _lease = self.scope.acquire(&self.call_id)?;
        self.scope.reported.lock().diagnostics.push(diagnostic);
        self.scope.progress.mark();
        Ok(())
    }
    pub fn details(&self, value: JsonValue, context: &Context) -> StorageFuture<()> {
        let lease = match self.scope.acquire(&self.call_id) {
            Ok(lease) => lease,
            Err(error) => return ready(Err(error)),
        };
        if let Err(error) = context.check() {
            return ready(Err(error.into()));
        }
        self.scope.reported.lock().details = Some(Arc::new(value));
        let wait = self.scope.progress.mark_and_wait();
        drop(lease);
        let context = context.clone();
        eager(async move {
            await_with_context(
                async move {
                    wait.await
                        .unwrap_or_else(|_| Err(Error::message("Progress waiter dropped")))
                },
                &context,
            )
            .await
            .map_err(await_error)?
        })
    }
    pub fn commit<T: Send + 'static, F, Fut>(
        &self,
        change: F,
        context: &Context,
    ) -> StorageFuture<T>
    where
        F: FnOnce(Transaction) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let result = Arc::new(Mutex::new(None));
        let capture = result.clone();
        let operation = self.runtime.commit(
            move |tx, _| async move {
                *capture.lock() = Some(change(tx).await?);
                Ok(None)
            },
            context,
        );
        eager(async move {
            operation.await?;
            Ok(result.lock().take().expect("committed tool result"))
        })
    }
    pub fn create_task<I: TaskValue, S: TaskValue, R: TaskValue, H: Send + Sync + 'static>(
        &self,
        task: &Task<I, S, R, H>,
        input: I,
        options: ToolTaskOptions,
        context: &Context,
    ) -> StorageFuture<TypedTaskId<R>> {
        let task = task.clone();
        let native = self.task_options(options);
        self.commit(
            move |tx| async move { tx.create_task(&task, input, native).await },
            context,
        )
    }
    pub fn create_erased_task(
        &self,
        task: &AnyTask,
        input: JsonValue,
        options: ToolTaskOptions,
        context: &Context,
    ) -> StorageFuture<TaskId> {
        let task = task.clone();
        let native = self.task_options(options);
        self.commit(
            move |tx| async move { tx.create_erased_task(&task, input, native).await },
            context,
        )
    }
    fn task_options(&self, options: ToolTaskOptions) -> TaskOptions {
        let mut native = TaskOptions::new(options.ownership);
        native.trace = options.trace;
        native.links = options.links;
        native.background = options.background;
        native.submission_ids = options.submission_ids;
        native.abandon_on_restart = options
            .abandon_on_restart
            .unwrap_or(!self.resumes && matches!(native.ownership, TaskOwnership::Task(_)));
        native
    }
    pub fn execute_tool(
        &self,
        name: impl Into<JsonString>,
        arguments: JsonObject,
        context: &Context,
        options: NestedCallOptions,
    ) -> StorageFuture<NestedToolExecutionResult> {
        self.execute_tool_value(name, arguments.into_value(), context, options)
    }
    pub fn execute_tool_value(
        &self,
        name: impl Into<JsonString>,
        arguments: JsonValue,
        context: &Context,
        options: NestedCallOptions,
    ) -> StorageFuture<NestedToolExecutionResult> {
        let lease = match self.scope.acquire(&self.call_id) {
            Ok(lease) => lease,
            Err(error) => return ready(Err(error)),
        };
        if let Some(key) = &options.key
            && let Err(error) = check_key(key)
        {
            return ready(Err(error));
        }
        let name = name.into();
        let mut admissions = self.scope.admissions.lock();
        let key = options.key.unwrap_or_else(|| {
            admissions.sequence += 1;
            admissions.sequence.to_string().into()
        });
        if admissions.memo.is_none() {
            admissions.generation += 1;
            let generation = admissions.generation;
            let weak = Arc::downgrade(&self.scope);
            let commit = self.runtime.store_memo(MADE_NESTED_CALLS, true, context);
            admissions.memo = Some(
                self.runtime
                    .eager(async move {
                        let result = commit.await;
                        if result.is_err()
                            && let Some(scope) = weak.upgrade()
                        {
                            let mut calls = scope.admissions.lock();
                            if calls.generation == generation {
                                calls.memo = None;
                            }
                        }
                        result
                    })
                    .shared(),
            );
        }
        let memo = admissions.memo.clone().expect("memo installed");
        let runtime = self.runtime.clone();
        let parent_call = self.call_id.clone();
        let call_name = name.clone();
        let caller = context.clone();
        let call_key = key.clone();
        let resumes = self.resumes;
        let admission = self
            .runtime
            .eager(async move {
                memo.await?;
                super::task::start_nested_call(
                    &runtime,
                    parent_call,
                    call_name,
                    arguments,
                    super::task::AdmissionOptions {
                        key: call_key,
                        progress: options.progress,
                        abandon: !resumes,
                    },
                    &caller,
                )
                .await
            })
            .shared();
        admissions.pending.push(admission.clone());
        drop(admissions);
        drop(lease);
        let runtime = self.runtime.clone();
        let context = context.clone();
        eager(async move {
            let id = admission.await?;
            let settled = runtime.wait_for_task(id, &context).await?;
            let stored = runtime
                .snapshot_family(&NESTED_CALL_DOC, runtime.task_id().untyped(), key, &context)
                .await?;
            if let Some(result) = stored.and_then(|state| state.result.clone()) {
                return Ok(result);
            }
            let TaskState::Terminal { outcome } = settled.state else {
                return Err(Error::message("Nested call did not settle"));
            };
            Ok(fallback_result(id.untyped(), &name, &outcome))
        })
    }
}
fn await_error(error: AwaitError) -> Error {
    match error {
        AwaitError::Aborted(error) => error.into(),
        AwaitError::Task(error) => Error::message(error.to_string()),
    }
}
