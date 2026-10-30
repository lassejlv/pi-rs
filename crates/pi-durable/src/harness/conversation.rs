//! Invocation-bound handles over shared conversation services. Admission checks
//! and cancellation are separate: admitted writes retain their durable outcome.
use super::{
    scheduler::{TaskScheduler, WeakTaskScheduler},
    submissions::{Submission, SubmissionAbort, Submissions},
    util::closed_error,
};
use crate::{
    storage::{StorageFuture, line::eager, ready},
    *,
};
use pi_chord::{AbortSignal, context::with_abort_signal};
use std::sync::Arc;

#[derive(Clone)]
pub struct InvocationBinding {
    pub signal: AbortSignal,
    check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    owner: Option<TaskScheduler>,
}
impl InvocationBinding {
    pub fn new(
        signal: AbortSignal,
        check: impl Fn() -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            signal,
            check: Arc::new(check),
            owner: None,
        }
    }
    pub(crate) fn retaining(mut self, owner: TaskScheduler) -> Self {
        self.owner = Some(owner);
        self
    }
    pub fn check(&self) -> Result<()> {
        (self.check)()
    }
    pub fn context(&self, context: &Context) -> Context {
        with_abort_signal(self.signal.clone(), context)
    }
}
#[derive(Clone, Debug)]
pub struct InputSubmissionDraft {
    pub request_id: Option<JsonString>,
    pub content: UserInput,
    pub when_busy: Option<WhenBusy>,
    pub entry: Option<InputEntry>,
}
impl InputSubmissionDraft {
    pub fn new(content: UserInput) -> Self {
        Self {
            request_id: None,
            content,
            when_busy: None,
            entry: None,
        }
    }
}
impl From<InputSubmissionDraft> for SubmissionDraft {
    fn from(input: InputSubmissionDraft) -> Self {
        Self::Input {
            request_id: input.request_id,
            content: input.content,
            when_busy: input.when_busy,
            entry: input.entry,
        }
    }
}
pub type ConversationResolver = Arc<
    dyn Fn(ConversationId, InvocationBinding, Context) -> StorageFuture<Option<ConversationHandle>>
        + Send
        + Sync,
>;
/// Shared conversation operations. Scheduler-owned services use a weak handle
/// to avoid a cycle; standalone services and invocation bindings retain it.
pub struct ConversationServices {
    session: Session,
    submissions: Submissions,
    tasks: WeakTaskScheduler,
    _scheduler_owner: Option<TaskScheduler>,
}
impl ConversationServices {
    pub fn new(session: Session, submissions: Submissions, tasks: TaskScheduler) -> Arc<Self> {
        Arc::new(Self {
            session,
            submissions,
            tasks: tasks.downgrade(),
            _scheduler_owner: Some(tasks),
        })
    }
    pub(crate) fn for_scheduler(
        session: Session,
        submissions: Submissions,
        tasks: &TaskScheduler,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            submissions,
            tasks: tasks.downgrade(),
            _scheduler_owner: None,
        })
    }
    pub fn resolver(self: &Arc<Self>) -> ConversationResolver {
        let weak = Arc::downgrade(self);
        Arc::new(move |id, binding, context| match weak.upgrade() {
            Some(services) => services.acquire(id, binding, &context),
            None => ready(Err(Error::message("Conversation services are unavailable"))),
        })
    }
    pub fn acquire(
        self: &Arc<Self>,
        id: ConversationId,
        binding: InvocationBinding,
        context: &Context,
    ) -> StorageFuture<Option<ConversationHandle>> {
        let storage = self.session.guarded_storage();
        let context = context.clone();
        let read = self
            .session
            .read_on_line(move || async move { storage.conversation(id, &context).await });
        let services = self.clone();
        eager(async move {
            Ok(read.await?.map(|_| ConversationHandle {
                id,
                binding,
                services,
            }))
        })
    }
}
#[derive(Clone)]
pub struct ConversationHandle {
    pub id: ConversationId,
    binding: InvocationBinding,
    services: Arc<ConversationServices>,
}
impl ConversationHandle {
    pub fn submit(
        &self,
        draft: InputSubmissionDraft,
        context: &Context,
    ) -> StorageFuture<BoundSubmission> {
        if let Err(error) = self.binding.check() {
            return ready(Err(error));
        }
        let operation =
            self.services
                .submissions
                .submit(self.id, draft.into(), &self.binding.context(context));
        let binding = self.binding.clone();
        eager(async move {
            let submission = operation.await?;
            Ok(BoundSubmission {
                id: submission.id,
                submission,
                binding,
            })
        })
    }
    pub fn abort(&self, context: &Context, background: bool) -> StorageFuture<()> {
        if let Err(error) = self.binding.check() {
            return ready(Err(error));
        }
        let Some(tasks) = self.services.tasks.upgrade() else {
            return ready(Err(closed_error(&self.services.session)));
        };
        tasks.abort_conversation(self.id, background, &self.binding.context(context))
    }
    pub fn wait_for_idle(&self, context: &Context) -> StorageFuture<()> {
        if let Err(error) = self.binding.check() {
            return ready(Err(error));
        }
        let Some(tasks) = self.services.tasks.upgrade() else {
            return ready(Err(closed_error(&self.services.session)));
        };
        tasks.wait_for_idle(Some(self.id), &self.binding.context(context))
    }
}
#[derive(Clone)]
pub struct BoundSubmission {
    pub id: SubmissionId,
    submission: Submission,
    binding: InvocationBinding,
}
impl BoundSubmission {
    pub fn status(&self, context: &Context) -> StorageFuture<SubmissionRecord> {
        if let Err(error) = self.binding.check() {
            return ready(Err(error));
        }
        self.submission.status(&self.binding.context(context))
    }
    pub fn wait(&self, context: &Context) -> StorageFuture<SettledSubmissionRecord> {
        if let Err(error) = self.binding.check() {
            return ready(Err(error));
        }
        self.submission.wait(&self.binding.context(context))
    }
    pub fn abort(&self, context: &Context) -> StorageFuture<SubmissionAbort> {
        if let Err(error) = self.binding.check() {
            return ready(Err(error));
        }
        self.submission.abort(&self.binding.context(context))
    }
}
