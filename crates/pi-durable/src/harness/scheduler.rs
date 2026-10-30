//! Durable task scheduling. Reservations, state changes and decisions share the
//! Session's mutation line. Handlers and joins always run outside it.
pub(super) mod ordered;
mod telemetry;
#[cfg(test)]
#[path = "scheduler/timer_tests.rs"]
mod timer_tests;
use super::{
    agent::{AgentRequest, AgentResolver, AgentView, resolve_agent_in},
    ownership::*,
    reader::*,
    runtime::{Change, TaskDriver},
    settings::{Settings, SettingsReader, read_settings, resolve_settings},
    state::*,
    util::*,
};
use crate::{
    storage::{StorageFuture, line::eager, ready},
    timers::Timers,
    *,
};
use futures::FutureExt;
use indexmap::{IndexMap, IndexSet};
use ordered::{OrderedMap, OrderedSet};
use parking_lot::Mutex;
use pi_chord::{
    AbortController, Watch,
    context::{AbortError, AbortReason as SignalReason, with_abort_signal, without_abort_signal},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    MissingTask,
    TaskTooOld,
    MigrationFailed,
    RestartLimit,
}
impl BlockedReason {
    fn text(self) -> &'static str {
        match self {
            Self::MissingTask => "missing_task",
            Self::TaskTooOld => "task_too_old",
            Self::MigrationFailed => "migration_failed",
            Self::RestartLimit => "restart_limit",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportTask {
    pub id: TaskId,
    pub kind: JsonString,
    pub conversation_id: ConversationId,
}
impl From<&TaskRecord> for ReportTask {
    fn from(record: &TaskRecord) -> Self {
        Self {
            id: record.id(),
            kind: record.info.kind.clone(),
            conversation_id: record.info.conversation_id,
        }
    }
}
#[derive(Clone)]
struct ReportSubject {
    task: ReportTask,
    extension: Option<JsonString>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct TaskReport {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<ReportTask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension: Option<JsonString>,
    #[serde(rename = "conversationId", skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationId>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Wake {
    pub active: bool,
    pub at: Option<f64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AbortResult {
    Marked,
    Terminal,
}
type SettleOutcome =
    Arc<dyn Fn(Transaction, TaskRecord, TaskOutcome) -> StorageFuture<()> + Send + Sync>;
type WithdrawInputs = Arc<dyn Fn(Transaction, ConversationId) -> StorageFuture<()> + Send + Sync>;
pub type EnvironmentResolver = Arc<
    dyn Fn(ConversationId, Context) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub struct SchedulerOptions {
    pub telemetry: pi_telemetry::ContextRef,
    pub settings: SettingsReader,
    pub models: super::models::ModelsReader,
    pub model_collection: Option<super::models::ModelCollectionReader>,
    pub env: EnvironmentResolver,
    pub conversation: Option<super::conversation::ConversationResolver>,
    pub agent: AgentResolver,
    pub on_report: Arc<dyn Fn(Error, TaskReport, Option<Context>) + Send + Sync>,
    pub on_wake: Option<Arc<dyn Fn(Wake) -> Result<()> + Send + Sync>>,
    pub settle_outcome: SettleOutcome,
    pub withdraw_inputs: WithdrawInputs,
    /// Awaited before each step of an invocation is queued on the line: a phase's first step,
    /// its next step after the handler returns, and an abort handler's start. A host across a
    /// process boundary uses it to let the continuation of the caller it just answered queue its
    /// own work first, as upstream's microtask order does. Reservation is never gated.
    pub step_gate: Option<StepGate>,
}
/// See [`SchedulerOptions::step_gate`].
pub type StepGate = Arc<dyn Fn(TaskId) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;
impl Default for SchedulerOptions {
    fn default() -> Self {
        let models = pi_ai::models::Models::default();
        Self {
            telemetry: pi_telemetry::noop_telemetry_context(),
            models: Arc::new(move || Ok(models.clone())),
            model_collection: None,
            env: Arc::new(|_, _| ready(Ok(None))),
            conversation: None,
            settings: Arc::new(|| Ok(resolve_settings(Default::default()))),
            agent: Arc::new(resolve_agent_in),
            on_report: Arc::new(|_, _, _| {}),
            on_wake: None,
            settle_outcome: Arc::new(|_, _, _| ready(Ok(()))),
            withdraw_inputs: Arc::new(|_, _| ready(Ok(()))),
            step_gate: None,
        }
    }
}
#[derive(Default)]
struct State {
    snapshot_reported: bool,
    enabled: bool,
    closing: bool,
    dirty: bool,
    draining: bool,
    reconciling: bool,
    reconcile_dirty: bool,
    reconciling_work: bool,
    finalize_pending: bool,
    cascade: bool,
    sweep_pending: bool,
    runnable: OrderedSet<TaskId>,
    held: IndexSet<TaskId>,
    sleeping: IndexSet<TaskId>,
    parked: IndexSet<TaskId>,
    abandoned: IndexSet<TaskId>,
    fail_fast: IndexSet<TaskId>,
    waiting: HashMap<TaskId, IndexSet<TaskId>>,
    invocations: OrderedMap<TaskId, Arc<Invocation>>,
    resumed: HashSet<TaskId>,
    failed_migrations: HashMap<TaskId, FailedMigration>,
    blocked_listeners: IndexMap<u64, Arc<dyn Fn() -> Result<()> + Send + Sync>>,
    next_blocked_listener: u64,
    timer: Option<WakeTimer>,
    next_wake_timer: u64,
    last_wake: Option<Wake>,
    wake_pending: bool,
    contexts: HashMap<ConversationId, KeptContext>,
    expiry: Option<(f64, DisarmTimer)>,
    expiry_generation: u64,
}
struct WakeTimer {
    at: f64,
    generation: u64,
    disarm: Option<DisarmTimer>,
    _keepalive: tokio::sync::oneshot::Sender<()>,
}
struct KeptContext {
    range: Arc<super::context::ContextRange>,
    idle_since: Option<f64>,
}
struct Invocation {
    task: TaskId,
    name: JsonString,
    extension: Option<JsonString>,
    conversation: ConversationId,
    mode: InvocationMode,
    controller: AbortController,
    context: Mutex<Context>,
    record: Mutex<TaskRecord>,
    fault: Mutex<Option<TaskOutcome>>,
    registry: Mutex<RegistryView>,
    agent: Mutex<Option<futures::future::Shared<StorageFuture<AgentView>>>>,
    ended: AtomicBool,
    done: tokio::sync::watch::Sender<bool>,
    watches: Mutex<IndexMap<u64, Watch>>,
    next_watch: AtomicU64,
    timers: Mutex<IndexMap<u64, DisarmTimer>>,
    next_timer: AtomicU64,
}
struct FailedMigration {
    task: AnyTask,
    error: Error,
}

/// The graph retains only fields used to derive blocked reasons, never payloads.
#[derive(Clone, Debug)]
pub struct BlockedSubject {
    pub id: TaskId,
    pub kind: JsonString,
    pub version: u64,
    pub abort_requested: bool,
    pub abort_reason: Option<AbortReason>,
    pub restarts: Option<u64>,
    pub status: TaskStatus,
}
impl From<&TaskRecord> for BlockedSubject {
    fn from(record: &TaskRecord) -> Self {
        Self {
            id: record.id(),
            kind: record.info.kind.clone(),
            version: record.info.version,
            abort_requested: record.info.abort_requested,
            abort_reason: record.info.abort_reason,
            restarts: record.info.restarts,
            status: record.state.status(),
        }
    }
}
impl Invocation {
    fn report_task(&self) -> ReportTask {
        ReportTask {
            id: self.task,
            kind: self.name.clone(),
            conversation_id: self.conversation,
        }
    }
    fn subject(&self) -> ReportSubject {
        ReportSubject {
            task: self.report_task(),
            extension: self.extension.clone(),
        }
    }
    fn ended_error(&self) -> Error {
        Error::message(format!("Task {} invocation has ended", self.task))
    }
    async fn finished(&self) {
        let mut done = self.done.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                return;
            }
        }
    }
}
struct Inner {
    session: Session,
    storage: Arc<dyn Storage>,
    registry: Arc<dyn RegistryReader>,
    options: SchedulerOptions,
    context: Context,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    read_only: bool,
    timers: Timers,
    tree: TaskTree,
    state: Mutex<State>,
    subscriptions: Mutex<Option<(Subscription, Subscription, Option<ReaderSubscription>)>>,
    task_waiters: Waiters<TaskId, TaskRecord>,
    idle_waiters: Waiters<Option<ConversationId>, ()>,
    graph: std::sync::OnceLock<Result<super::task_graph::TaskGraphView>>,
    active_metric: pi_telemetry::SchemaTelemetryCounter,
    duration_metric: pi_telemetry::SchemaTelemetryHistogram,
}
#[derive(Clone)]
pub struct TaskScheduler {
    inner: Arc<Inner>,
}
#[derive(Clone)]
pub(crate) struct WeakTaskScheduler(std::sync::Weak<Inner>);
impl WeakTaskScheduler {
    pub(crate) fn upgrade(&self) -> Option<TaskScheduler> {
        self.0.upgrade().map(|inner| TaskScheduler { inner })
    }
}
struct Reservation {
    invocation: Arc<Invocation>,
    task: AnyTask,
}

impl TaskScheduler {
    /// Open the task service without enabling dispatch. Recovery still repairs
    /// interrupted reservations and durable cancellation/finalization intents.
    pub async fn open(
        storage: Arc<dyn Storage>,
        registry: impl RegistryReader,
        options: SchedulerOptions,
        mut session_options: SessionOptions,
        context: &Context,
    ) -> Result<Self> {
        let registry: Arc<dyn RegistryReader> = Arc::new(registry);
        let owner = Arc::new(Mutex::new(Weak::<Inner>::new()));
        let close_owner = owner.clone();
        let previous_close = session_options.before_close.take();
        session_options.before_close = Some(Arc::new(move |session| {
            let inner = close_owner.lock().upgrade();
            let previous = previous_close.clone();
            Box::pin(async move {
                if let Some(inner) = inner {
                    TaskScheduler { inner }.join().await;
                }
                if let Some(previous) = previous {
                    previous(session).await?;
                }
                Ok(())
            })
        }));
        let now = session_options.now.clone();
        let read_only = session_options.read_only;
        let report = session_options.report.clone();
        let timers = Timers::new(session_options.set_timer.clone(), report);
        let session = Session::new(storage, session_options);
        let metrics = pi_telemetry::create_typed_metrics(
            options
                .telemetry
                .metrics()
                .unwrap_or_else(pi_telemetry::noop_telemetry_metrics),
            &[&super::telemetry::DURABLE_TELEMETRY_SCHEMA],
        );
        let scheduler = Self {
            inner: Arc::new(Inner {
                storage: session.guarded_storage(),
                session,
                registry: registry.clone(),
                options,
                context: without_abort_signal(context),
                now,
                read_only,
                timers,
                tree: TaskTree::default(),
                state: Mutex::new(State::default()),
                subscriptions: Mutex::new(None),
                task_waiters: Waiters::default(),
                idle_waiters: Waiters::default(),
                graph: std::sync::OnceLock::new(),
                active_metric: metrics.up_down_counter("pi.durable.tasks.active"),
                duration_metric: metrics.histogram("pi.durable.phase.duration"),
            }),
        };
        *owner.lock() = Arc::downgrade(&scheduler.inner);
        let weak = Arc::downgrade(&scheduler.inner);
        let publication = scheduler
            .inner
            .session
            .subscribe_internal(move |publication, _| {
                if let Some(inner) = weak.upgrade() {
                    TaskScheduler { inner }.observe(publication);
                }
                Ok(())
            })?;
        let weak = Arc::downgrade(&scheduler.inner);
        let close = scheduler.inner.session.subscribe_close(move || {
            if let Some(inner) = weak.upgrade() {
                TaskScheduler { inner }.seal();
            }
        })?;
        let weak = Arc::downgrade(&scheduler.inner);
        let subscription = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.subscribe(Arc::new(move || {
                if let Some(inner) = weak.upgrade() {
                    let scheduler = TaskScheduler { inner };
                    scheduler.kick();
                    scheduler.notify_blocked();
                }
            }))
        }))
        .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)));
        let registry = match subscription {
            Ok(subscription) => subscription,
            Err(error) => {
                drop(publication);
                drop(close);
                let _ = scheduler.inner.session.close(context).await;
                return Err(error);
            }
        };
        *scheduler.inner.subscriptions.lock() = Some((publication, close, Some(registry)));
        let parked = Arc::new(Mutex::new(Vec::<TaskRecord>::new()));
        let recovered_parked = parked.clone();
        let limit = scheduler.restart_limit();
        let initialize = scheduler.clone();
        let result = scheduler
            .inner
            .session
            .commit(
                move |tx| async move {
                    let mut records = Vec::new();
                    for status in [
                        TaskStatus::Pending,
                        TaskStatus::Running,
                        TaskStatus::Waiting,
                        TaskStatus::Sleeping,
                        TaskStatus::Completing,
                    ] {
                        let storage = initialize.inner.storage.clone();
                        let context = initialize.inner.context.clone();
                        records.extend(
                            scan_all(move |cursor| {
                                storage.scan_tasks(
                                    &TaskQuery {
                                        status: Some(status),
                                        ..Default::default()
                                    },
                                    256,
                                    cursor.as_ref(),
                                    &context,
                                )
                            })
                            .await?,
                        );
                    }
                    for record in records {
                        initialize.track(record.clone());
                        initialize.inner.state.lock().resumed.insert(record.id());
                        if initialize.inner.read_only {
                            continue;
                        }
                        if record.info.abandon_on_restart.is_some() {
                            initialize.inner.state.lock().abandoned.insert(record.id());
                        }
                        if record.state.status() == TaskStatus::Running {
                            let checkpoint = record.state.checkpoint().unwrap().clone();
                            let mut recovered = with_state(record, TaskState::pending(checkpoint));
                            recovered.info.restarts =
                                Some(recovered.info.restarts.unwrap_or(0) + 1);
                            if !recovered.info.abort_requested
                                && recovered.info.restarts.unwrap_or(0) as f64 >= limit
                            {
                                recovered_parked.lock().push(recovered.clone());
                            }
                            tx.set_task(recovered)?;
                        }
                    }
                    Ok(())
                },
                context,
            )
            .await;
        if let Err(error) = result {
            let _ = scheduler.inner.session.close(&Context::default()).await;
            return Err(error);
        }
        for record in std::mem::take(&mut *parked.lock()) {
            let message = format!(
                "Task {} ({}) was cut off {} times without progress; it is parked until retryTask()",
                record.id(),
                record.info.kind,
                record.info.restarts.unwrap_or(0)
            );
            scheduler.report(
                Error::message(message),
                "task_parked",
                Some(scheduler.about_task(&record)),
                None,
            );
        }
        if !scheduler.inner.read_only {
            scheduler.inner.state.lock().cascade = true;
            for record in scheduler.inner.tree.live_records() {
                if matches!(
                    record.state,
                    TaskState::Waiting {
                        policy: JoinPolicy::FailFast,
                        ..
                    }
                ) {
                    scheduler.inner.state.lock().fail_fast.insert(record.id());
                }
            }
        }
        scheduler.schedule_reconcile();
        scheduler.schedule_wake();
        Ok(scheduler)
    }
    pub fn session(&self) -> Session {
        self.inner.session.clone()
    }
    pub(crate) fn downgrade(&self) -> WeakTaskScheduler {
        WeakTaskScheduler(Arc::downgrade(&self.inner))
    }
    pub(crate) fn session_ref(&self) -> &Session {
        &self.inner.session
    }
    pub fn index_sizes(&self) -> IndexSizes {
        self.inner.tree.sizes()
    }
    fn now(&self) -> f64 {
        (self.inner.now)()
    }
    fn about_task(&self, record: &TaskRecord) -> ReportSubject {
        let extension = match read_registry(self.inner.registry.as_ref()) {
            Ok(snapshot) => snapshot
                .task(&record.info.kind)
                .and_then(|installed| installed.extension.clone()),
            Err(error) => {
                self.report_host(error, "registry");
                None
            }
        };
        ReportSubject {
            task: record.into(),
            extension,
        }
    }
    fn report(
        &self,
        error: Error,
        kind: &'static str,
        subject: Option<ReportSubject>,
        reason: Option<&'static str>,
    ) {
        let context = subject
            .as_ref()
            .and_then(|subject| self.report_context(subject.task.id));
        self.report_in(error, kind, subject, reason, context);
    }
    fn report_context(&self, task: TaskId) -> Option<Context> {
        let invocation = self.inner.state.lock().invocations.get(&task).cloned();
        invocation.map(|invocation| invocation.context.lock().clone())
    }
    fn report_in(
        &self,
        error: Error,
        kind: &'static str,
        subject: Option<ReportSubject>,
        reason: Option<&'static str>,
        context: Option<Context>,
    ) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.inner.options.on_report)(
                error,
                TaskReport {
                    kind,
                    task: subject.as_ref().map(|subject| subject.task.clone()),
                    reason,
                    callback: None,
                    extension: subject.and_then(|subject| subject.extension),
                    conversation: None,
                },
                context,
            )
        }));
    }
    fn report_host(&self, error: Error, callback: &'static str) {
        self.report_host_for(error, callback, None);
    }
    fn report_host_for(&self, error: Error, callback: &'static str, task: Option<ReportTask>) {
        let context = task.as_ref().and_then(|task| self.report_context(task.id));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.inner.options.on_report)(
                error,
                TaskReport {
                    kind: "host_error",
                    task,
                    reason: None,
                    callback: Some(callback),
                    extension: None,
                    conversation: None,
                },
                context,
            )
        }));
    }
    fn report_extension(
        &self,
        error: Error,
        extension: impl Into<Option<JsonString>>,
        conversation: ConversationId,
        task: Option<ReportTask>,
    ) {
        let extension = extension.into();
        let context = task.as_ref().and_then(|task| self.report_context(task.id));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.inner.options.on_report)(
                error,
                TaskReport {
                    kind: "extension_error",
                    conversation: task.is_none().then_some(conversation),
                    task,
                    reason: None,
                    callback: None,
                    extension,
                },
                context,
            )
        }));
    }
    fn settings(&self) -> Result<Settings> {
        read_settings(&self.inner.options.settings)
    }
    fn fail(&self, error: Error) {
        self.inner.session.fail_if_open(error);
    }
    fn restart_limit(&self) -> f64 {
        self.settings()
            .map(|settings| settings.restart_limit)
            .unwrap_or_else(|error| {
                self.report_host(error, "settings");
                super::settings::DEFAULT_RESTART_LIMIT
            })
    }
    fn parked(&self, record: &TaskRecord) -> bool {
        !record.info.abort_requested
            && record
                .info
                .restarts
                .is_some_and(|n| n as f64 >= self.restart_limit())
    }
    fn not_due(&self, record: &TaskRecord) -> bool {
        matches!(record.state, TaskState::Sleeping { until, .. } if !record.info.abort_requested && until > self.now())
    }
    fn waiting_on(&self, record: &TaskRecord, first: bool) -> Vec<TaskId> {
        if record.info.abort_requested {
            return self.inner.tree.owned_live(record.id(), None, first, None);
        }
        match &record.state {
            TaskState::Waiting { on, .. } => on
                .iter()
                .filter(|id| self.inner.tree.live(**id).is_some())
                .copied()
                .collect(),
            _ => Vec::new(),
        }
    }
    fn track(&self, record: TaskRecord) {
        let id = record.id();
        let previous = self.inner.tree.live(id);
        let above = self.inner.tree.track(record.clone());
        let mut state = self.inner.state.lock();
        if let Some(invocation) = state.invocations.get(&id) {
            *invocation.record.lock() = record.clone();
        }
        let terminal = record.state.status() == TaskStatus::Terminal;
        let delta = if state.closing {
            0.0
        } else if terminal && previous.is_some() {
            -1.0
        } else if !terminal && previous.is_none() {
            1.0
        } else {
            0.0
        };
        if terminal {
            state.resumed.remove(&id);
        }
        drop(state);
        if delta != 0.0 {
            self.inner.active_metric.add(
                delta,
                pi_telemetry::attributes! {"task" => record.info.kind.clone()},
            );
        }
        let mut state = self.inner.state.lock();
        if let Some(TaskState::Waiting { on, .. }) = previous.as_ref().map(|r| &r.state) {
            for target in on {
                if let Some(waiters) = state.waiting.get_mut(target) {
                    waiters.shift_remove(&id);
                    if waiters.is_empty() {
                        state.waiting.remove(target);
                    }
                }
            }
        }
        if record.state.status() == TaskStatus::Terminal {
            state.runnable.shift_remove(&id);
            state.held.shift_remove(&id);
            state.sleeping.shift_remove(&id);
            state.parked.shift_remove(&id);
            state.failed_migrations.remove(&id);
            state.fail_fast.shift_remove(&id);
            if let Some(above) = above {
                if above.state.status() == TaskStatus::Completing {
                    state.held.insert(above.id());
                } else if above.info.abort_requested {
                    state.runnable.insert(above.id());
                }
            }
            let waiters = state.waiting.get(&id).cloned().unwrap_or_default();
            state.runnable.extend(waiters);
            return;
        }
        if record.state.status() == TaskStatus::Completing {
            state.runnable.shift_remove(&id);
            state.held.insert(id);
            state.finalize_pending = true;
        } else {
            state.runnable.insert(id);
        }
        if let TaskState::Waiting { on, .. } = &record.state {
            for target in on {
                state.waiting.entry(*target).or_default().insert(id);
            }
        }
        if record.state.status() == TaskStatus::Sleeping {
            state.sleeping.insert(id);
        } else {
            state.sleeping.shift_remove(&id);
        }
    }
    fn observe(&self, publication: &CommitPublication) {
        for change in &publication.changes {
            if let CommitChange::Conversation(record) = change {
                self.inner
                    .tree
                    .set_edge(record.id, record.owner.as_ref().map(|owner| owner.task_id));
            }
        }
        let mut changed = false;
        let mut reconcile = false;
        let mut cascade = false;
        for change in &publication.changes {
            let CommitChange::Task(record) = change else {
                continue;
            };
            changed = true;
            let previous = self.inner.tree.live(record.id());
            self.track((**record).clone());
            if record.state.status() == TaskStatus::Terminal {
                reconcile = true;
                {
                    let mut state = self.inner.state.lock();
                    state.finalize_pending |= !state.held.is_empty();
                }
                self.inner
                    .task_waiters
                    .resolve(&record.id(), (**record).clone());
            }
            let invocation = self
                .inner
                .state
                .lock()
                .invocations
                .get(&record.id())
                .cloned();
            if record.info.abort_requested
                && !previous.as_ref().is_some_and(|r| r.info.abort_requested)
            {
                cascade = true;
                if let Some(invocation) = invocation.filter(|i| i.mode == InvocationMode::Run) {
                    invocation.controller.abort();
                }
            }
            if failed_outcome(record) && !previous.as_ref().is_some_and(|r| failed_outcome(r)) {
                let waiters = self
                    .inner
                    .state
                    .lock()
                    .waiting
                    .get(&record.id())
                    .cloned()
                    .unwrap_or_default();
                for waiter in waiters {
                    if self.inner.tree.live(waiter).is_some_and(|r| {
                        matches!(
                            r.state,
                            TaskState::Waiting {
                                policy: JoinPolicy::FailFast,
                                ..
                            }
                        )
                    }) {
                        self.inner.state.lock().fail_fast.insert(waiter);
                        reconcile = true;
                    }
                }
            }
            if matches!(
                record.state,
                TaskState::Waiting {
                    policy: JoinPolicy::FailFast,
                    ..
                }
            ) && previous
                .as_ref()
                .is_none_or(|r| r.state.status() != TaskStatus::Waiting)
            {
                self.inner.state.lock().fail_fast.insert(record.id());
                reconcile = true;
            }
            if record.state.status() != TaskStatus::Terminal {
                if previous
                    .as_ref()
                    .is_some_and(|r| r.info.abort_reason.is_some())
                    && record.info.abort_reason.is_none()
                {
                    cascade = true;
                }
                if record.state.status() == TaskStatus::Completing
                    && previous
                        .as_ref()
                        .is_none_or(|r| r.state.status() != TaskStatus::Completing)
                {
                    reconcile = true;
                    cascade |= cancellation_intent(record);
                }
                if previous.is_none() {
                    if !self.inner.tree.known(Node::Task(record.id())) {
                        reconcile = true;
                    } else if !record.info.background
                        && !record.info.abort_requested
                        && self
                            .inner
                            .tree
                            .cancelling_owner(TaskNode::of(record).parent())
                            .is_some()
                    {
                        cascade = true;
                    }
                }
            }
        }
        if self.inner.tree.has_cancellation_intents() {
            for change in &publication.changes {
                if let CommitChange::Submission(record) = change {
                    let node = Node::Conversation(record.info.conversation_id);
                    if matches!(record.state, SubmissionState::InputQueued)
                        && (!self.inner.tree.known(node)
                            || self.inner.tree.cancelling_owner(node).is_some())
                    {
                        cascade = true;
                    }
                }
            }
        }
        let pending = {
            let mut state = self.inner.state.lock();
            state.cascade |= cascade;
            state.cascade
        };
        if reconcile || pending {
            self.schedule_reconcile();
        }
        self.inner.tree.sweep();
        if changed {
            self.settle_idle();
            self.kick();
        }
    }
    pub fn resume(&self) -> Result<()> {
        if self.inner.state.lock().closing {
            return Err(closed_error(&self.inner.session));
        }
        if self.inner.read_only {
            return Err(Error::read_only("running tasks"));
        }
        self.inner.state.lock().enabled = true;
        self.kick();
        Ok(())
    }
    pub fn pause(&self) {
        self.inner.state.lock().enabled = false;
        self.schedule_wake();
    }
    pub fn retry(&self, id: TaskId, context: &Context) -> StorageFuture<()> {
        let scheduler = self.clone();
        let operation = self.inner.session.commit(
            move |tx| async move {
                let mut record = tx
                    .task(id)
                    .await?
                    .ok_or_else(|| Error::message(format!("Task {id} does not exist")))?;
                if !matches!(
                    record.state.status(),
                    TaskStatus::Terminal | TaskStatus::Completing
                ) && scheduler.parked(&record)
                {
                    record.info.restarts = None;
                    tx.set_task(record)?;
                }
                Ok(())
            },
            context,
        );
        let scheduler = self.clone();
        eager(async move {
            operation.await?;
            scheduler.resume()
        })
    }
    pub fn wait_for_task(&self, id: TaskId, context: &Context) -> StorageFuture<TaskRecord> {
        let scheduler = self.clone();
        let session = self.inner.session.clone();
        let context = context.clone();
        let found = self.inner.session.read_on_line(move || async move {
            if scheduler.inner.state.lock().closing {
                return Err(closed_error(&scheduler.inner.session));
            }
            if scheduler.inner.tree.live(id).is_some() {
                return Ok(scheduler.inner.task_waiters.add(id, &context));
            }
            let record = scheduler
                .inner
                .storage
                .task(id, &context)
                .await?
                .ok_or_else(|| Error::message(format!("Task {id} does not exist")))?;
            Ok(ready(Ok(record)))
        });
        eager(async move {
            let record = found.await?.await?;
            session.publication_settled().await;
            Ok(record)
        })
    }
    pub fn wait_for_idle(
        &self,
        conversation: Option<ConversationId>,
        context: &Context,
    ) -> StorageFuture<()> {
        if self.inner.state.lock().closing {
            return ready(Err(closed_error(&self.inner.session)));
        }
        if self.inner.tree.idle(conversation) {
            return ready(Ok(()));
        }
        self.schedule_reconcile();
        self.inner.idle_waiters.add(conversation, context)
    }
    fn settle_idle(&self) {
        for scope in self.inner.idle_waiters.keys() {
            if self.inner.tree.idle(scope) {
                self.inner.idle_waiters.resolve(&scope, ());
            }
        }
        let now = self.now();
        let retention = self.context_retention();
        let mut state = self.inner.state.lock();
        state.contexts.retain(|id, kept| {
            if kept
                .idle_since
                .is_some_and(|since| now - since >= retention)
            {
                return false;
            }
            if !self.inner.tree.idle(Some(*id)) {
                kept.idle_since = None;
            } else if retention > 0.0 {
                kept.idle_since.get_or_insert(now);
            } else {
                return false;
            }
            true
        });
        drop(state);
        self.schedule_expiry();
    }
    fn context_retention(&self) -> f64 {
        self.settings()
            .map(|settings| settings.context_retention_ms)
            .unwrap_or_else(|error| {
                self.report_host(error, "settings");
                0.0
            })
    }
    fn schedule_expiry(&self) {
        let retention = self.context_retention();
        let (at, previous, generation) = {
            let mut state = self.inner.state.lock();
            let at = state
                .contexts
                .values()
                .filter_map(|kept| kept.idle_since.map(|since| since + retention))
                .reduce(f64::min);
            if state.expiry.as_ref().map(|(at, _)| *at) == at {
                return;
            }
            let previous = state.expiry.take();
            state.expiry_generation += 1;
            (
                if state.closing { None } else { at },
                previous,
                state.expiry_generation,
            )
        };
        if let Some((_, disarm)) = previous {
            disarm();
        }
        let Some(at) = at else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let disarm = self.inner.timers.background(
            Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    let scheduler = TaskScheduler { inner };
                    {
                        let mut state = scheduler.inner.state.lock();
                        if state.expiry_generation != generation || state.closing {
                            return;
                        }
                        state.expiry = None;
                    }
                    scheduler.settle_idle();
                }
            }),
            at - self.now(),
        );
        let mut state = self.inner.state.lock();
        if state.expiry_generation == generation && !state.closing {
            state.expiry = disarm.map(|disarm| (at, disarm));
        } else if let Some(disarm) = disarm {
            drop(state);
            disarm();
        }
    }
    /// Context retention is diagnostic only; entries remain authoritative in Storage.
    pub fn retained_contexts(&self) -> usize {
        self.inner.state.lock().contexts.len()
    }
    fn kick(&self) {
        self.schedule_wake();
        let spawn = {
            let mut state = self.inner.state.lock();
            state.dirty = true;
            if state.draining || !state.enabled || state.closing {
                false
            } else {
                state.draining = true;
                true
            }
        };
        if spawn {
            let scheduler = self.clone();
            self.inner.session.spawn(async move {
                let _ = scheduler.drain().await;
            });
        }
    }
    fn drain(&self) -> StorageFuture<()> {
        let scheduler = self.clone();
        Box::pin(async move {
            loop {
                let again = {
                    let mut state = scheduler.inner.state.lock();
                    if state.dirty && state.enabled && !state.closing {
                        state.dirty = false;
                        true
                    } else {
                        false
                    }
                };
                if !again {
                    break;
                }
                match scheduler.reserve().await {
                    Ok(reservations) => {
                        for reservation in reservations {
                            scheduler.start(reservation);
                        }
                    }
                    Err(error) => {
                        scheduler.fail(error);
                        break;
                    }
                }
            }
            let dirty = {
                let mut state = scheduler.inner.state.lock();
                state.draining = false;
                state.dirty
            };
            if dirty {
                scheduler.kick();
            }
            Ok(())
        })
    }
    fn fit(
        &self,
        record: &TaskRecord,
        task: Option<&AnyTask>,
    ) -> std::result::Result<bool, BlockedReason> {
        self.fit_subject(&BlockedSubject::from(record), task)
    }
    fn fit_subject(
        &self,
        record: &BlockedSubject,
        task: Option<&AnyTask>,
    ) -> std::result::Result<bool, BlockedReason> {
        let Some(task) = task else {
            return Err(BlockedReason::MissingTask);
        };
        if task.version() == record.version {
            return Ok(false);
        }
        if task.version() < record.version {
            return Err(BlockedReason::TaskTooOld);
        }
        if self
            .inner
            .state
            .lock()
            .failed_migrations
            .get(&record.id)
            .is_some_and(|failed| failed.task.same_definition(task))
        {
            return Err(BlockedReason::MigrationFailed);
        }
        Ok(true)
    }
    fn resolve(
        &self,
        record: &TaskRecord,
        snapshot: &RegistryView,
    ) -> std::result::Result<(AnyTask, TaskRecord), BlockedReason> {
        let installed = snapshot.task(&record.info.kind);
        let migrates = self.fit(record, installed.map(|installed| &installed.task))?;
        let task = installed.unwrap().task.clone();
        if !migrates {
            return Ok((task, record.clone()));
        }
        let migrated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let migrate = task.0.migrate.as_ref().ok_or_else(|| {
                Error::message(format!(
                    "Task {} version {} has no migration from {}",
                    record.info.kind,
                    task.version(),
                    record.info.version
                ))
            })?;
            migrate(
                record.info.input.clone(),
                record.state.checkpoint().unwrap().clone(),
                record.info.version,
            )
        }))
        .unwrap_or_else(|error| Err(crate::session::panic_error(error)));
        match migrated {
            Ok((input, checkpoint)) => {
                let mut record = record.clone();
                record.info.input = input;
                record.info.version = task.version();
                match &mut record.state {
                    TaskState::Pending {
                        checkpoint: current,
                        ..
                    }
                    | TaskState::Running {
                        checkpoint: current,
                        ..
                    }
                    | TaskState::Waiting {
                        checkpoint: current,
                        ..
                    }
                    | TaskState::Sleeping {
                        checkpoint: current,
                        ..
                    } => *current = checkpoint,
                    _ => unreachable!(),
                }
                Ok((task, record))
            }
            Err(error) => {
                self.inner.state.lock().failed_migrations.insert(
                    record.id(),
                    FailedMigration {
                        task,
                        error: error.clone(),
                    },
                );
                self.defer_blocked_notification();
                self.schedule_wake();
                self.report(
                    error,
                    "migration_failed",
                    Some(ReportSubject {
                        task: record.into(),
                        extension: installed.and_then(|installed| installed.extension.clone()),
                    }),
                    None,
                );
                Err(BlockedReason::MigrationFailed)
            }
        }
    }
    pub fn blocked_reason(&self, record: &TaskRecord) -> Option<BlockedReason> {
        self.blocked_subject_reason(&BlockedSubject::from(record))
    }
    pub fn blocked_subject_reason(&self, record: &BlockedSubject) -> Option<BlockedReason> {
        if matches!(
            record.status,
            TaskStatus::Running | TaskStatus::Completing | TaskStatus::Terminal
        ) || (record.abort_requested && record.abort_reason.is_none())
        {
            return None;
        }
        if !record.abort_requested
            && record
                .restarts
                .is_some_and(|n| n as f64 >= self.restart_limit())
        {
            return Some(BlockedReason::RestartLimit);
        }
        let snapshot = match read_registry(self.inner.registry.as_ref()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let reported = {
                    let mut state = self.inner.state.lock();
                    let reported = state.snapshot_reported;
                    state.snapshot_reported = true;
                    reported
                };
                if !reported {
                    self.report_host(error, "registry");
                }
                return None;
            }
        };
        let installed = snapshot.task(&record.kind);
        match self.fit_subject(record, installed.map(|i| &i.task)) {
            Err(reason) => Some(reason),
            Ok(true) if installed.unwrap().task.0.migrate.is_none() => {
                Some(BlockedReason::MigrationFailed)
            }
            _ => None,
        }
    }
    pub fn subscribe_blocked(
        &self,
        listener: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> ReaderSubscription {
        let id = {
            let mut state = self.inner.state.lock();
            state.next_blocked_listener += 1;
            let id = state.next_blocked_listener;
            state.blocked_listeners.insert(id, listener);
            id
        };
        let weak = Arc::downgrade(&self.inner);
        ReaderSubscription::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.state.lock().blocked_listeners.shift_remove(&id);
            }
            Ok(())
        })
    }
    fn notify_blocked(&self) {
        let listeners = self
            .inner
            .state
            .lock()
            .blocked_listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener()))
                .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)));
            if let Err(error) = result {
                self.report(error, "listener_error", None, None);
            }
        }
    }
    fn defer_blocked_notification(&self) {
        let scheduler = self.clone();
        pi_chord::rt::spawn(async move {
            tokio::task::yield_now().await;
            scheduler.notify_blocked();
        });
    }
    pub fn inspect(&self, context: &Context) -> StorageFuture<super::inspect::HarnessInspection> {
        use super::inspect::*;
        let scheduler = self.clone();
        let context = context.clone();
        self.inner.session.read_on_line(move || async move {
            let snapshot = read_registry(scheduler.inner.registry.as_ref())?;
            load_scopes(
                &scheduler.inner.tree,
                scheduler.inner.storage.as_ref(),
                &context,
            )
            .await?;
            let tasks = scheduler
                .inner
                .tree
                .live_records()
                .into_iter()
                .map(|record| TaskInspection {
                    state: scheduler.inspect_task(&record, &snapshot),
                    record,
                })
                .collect();
            let scheduling = {
                let state = scheduler.inner.state.lock();
                if state.closing {
                    Scheduling::Closing
                } else if state.enabled {
                    Scheduling::Running
                } else {
                    Scheduling::Paused
                }
            };
            let mut submissions = Vec::new();
            for status in [SubmissionStatus::Queued, SubmissionStatus::Placed] {
                let mut cursor = None;
                loop {
                    let page = scheduler
                        .inner
                        .storage
                        .scan_submissions(
                            &SubmissionQuery {
                                status: Some(status),
                                ..Default::default()
                            },
                            256,
                            cursor.as_ref(),
                            &context,
                        )
                        .await?;
                    submissions.extend(page.items);
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            submissions.sort_by_key(SubmissionRecord::id);
            Ok(HarnessInspection {
                scheduling,
                tasks,
                submissions,
            })
        })
    }
    fn inspect_task(
        &self,
        record: &TaskRecord,
        snapshot: &RegistryView,
    ) -> super::inspect::TaskInspectionState {
        use super::inspect::TaskInspectionState as State;
        if self
            .inner
            .state
            .lock()
            .invocations
            .contains_key(&record.id())
        {
            return State::Running;
        }
        if record.state.status() == TaskStatus::Completing {
            return State::Completing;
        }
        let on = self.waiting_on(record, false);
        if !on.is_empty() {
            return State::Waiting { on };
        }
        if self.not_due(record)
            && let TaskState::Sleeping { until, .. } = record.state
        {
            return State::Sleeping { until };
        }
        if self.parked(record) {
            return State::Blocked {
                reason: BlockedReason::RestartLimit,
                error: None,
            };
        }
        let task = snapshot
            .task(&record.info.kind)
            .map(|installed| &installed.task);
        match self.fit(record, task) {
            Err(reason) => State::Blocked {
                reason,
                error: if reason == BlockedReason::MigrationFailed {
                    self.inner
                        .state
                        .lock()
                        .failed_migrations
                        .get(&record.id())
                        .map(|failed| failed.error.clone())
                } else {
                    None
                },
            },
            Ok(true) if task.unwrap().0.migrate.is_none() => State::Blocked {
                reason: BlockedReason::MigrationFailed,
                error: Some(Error::message(format!(
                    "Task {} version {} has no migration from {}",
                    record.info.kind,
                    task.unwrap().version(),
                    record.info.version
                ))),
            },
            Ok(migrates) => State::Ready { migrates },
        }
    }
    pub fn watch_task_graph(
        &self,
        context: &Context,
    ) -> StorageFuture<super::task_graph::TaskGraphWatch> {
        use super::task_graph::*;
        let graph = self.inner.graph.get_or_init(|| {
            let weak_blocked = Arc::downgrade(&self.inner);
            let weak_subscribe = Arc::downgrade(&self.inner);
            TaskGraphView::new(
                self.inner.session.clone(),
                self.inner.storage.clone(),
                TaskGraphBlocking {
                    blocked: Arc::new(move |subject| {
                        weak_blocked.upgrade().and_then(|inner| {
                            TaskScheduler { inner }.blocked_subject_reason(subject)
                        })
                    }),
                    subscribe: Arc::new(move |listener| {
                        weak_subscribe
                            .upgrade()
                            .map(|inner| TaskScheduler { inner }.subscribe_blocked(listener))
                            .ok_or_else(|| Error::message("Scheduler is closed"))
                    }),
                },
            )
        });
        match graph {
            Ok(graph) => graph.watch(context),
            Err(error) => ready(Err(error.clone())),
        }
    }
    fn reserve(&self) -> StorageFuture<Vec<Reservation>> {
        let scheduler = self.clone();
        let reservations = Arc::new(Mutex::new(Vec::<Reservation>::new()));
        let capture = reservations.clone();
        let faulted = Arc::new(Mutex::new(Vec::<(Arc<TaskRecord>, String)>::new()));
        let capture_faulted = faulted.clone();
        let operation = self.inner.session.commit(move |tx| async move {
            let stopped = { let state = scheduler.inner.state.lock(); !state.enabled || state.closing };
            if stopped { return Ok(()); }
            let abandoned = scheduler.inner.state.lock().abandoned.iter().copied().collect::<Vec<_>>();
            if !abandoned.is_empty() {
                for id in &abandoned { if let Some(record) = scheduler.inner.tree.live(*id).filter(|r| !r.info.abort_requested) { tx.set_task(with_abort_mark((*record).clone(), Some(AbortReason::Restart)))?; } }
                // Retired only after this commit resolves; no handler runs in this pass.
                return Ok(());
            }
            load_scopes(&scheduler.inner.tree, scheduler.inner.storage.as_ref(), &scheduler.inner.context).await?;
            let now = scheduler.now();
            let sleeping = scheduler.inner.state.lock().sleeping.iter().copied().collect::<Vec<_>>();
            for id in sleeping { if scheduler.inner.tree.live(id).is_some_and(|r| !matches!(r.state, TaskState::Sleeping { until, .. } if !r.info.abort_requested && until > now)) { scheduler.inner.state.lock().runnable.insert(id); } }
            let parked = scheduler.inner.state.lock().parked.iter().copied().collect::<Vec<_>>();
            let mut unparked = false;
            for id in parked { if !scheduler.inner.tree.live(id).is_some_and(|r| scheduler.parked(&r)) { let mut state = scheduler.inner.state.lock(); state.parked.shift_remove(&id); state.runnable.insert(id); unparked = true; } }
            if unparked { scheduler.defer_blocked_notification(); }
            let candidates = scheduler.inner.state.lock().runnable.iter().copied().collect::<Vec<_>>();
            let mut snapshot = None;
            for id in candidates {
                let record = scheduler.inner.tree.live(id);
                if record.as_ref().is_none_or(|r| r.state.status() == TaskStatus::Completing) || scheduler.inner.state.lock().invocations.contains_key(&id) { scheduler.inner.state.lock().runnable.shift_remove(&id); continue; }
                let record = record.unwrap();
                if !scheduler.waiting_on(&record, true).is_empty() || scheduler.not_due(&record) { scheduler.inner.state.lock().runnable.shift_remove(&id); continue; }
                if scheduler.parked(&record) { let newly = { let mut state = scheduler.inner.state.lock(); state.runnable.shift_remove(&id); state.parked.insert(id) }; if newly { scheduler.defer_blocked_notification(); } continue; }
                let mode = if record.info.abort_requested { InvocationMode::Abort } else { InvocationMode::Run };
                if mode == InvocationMode::Run && !record.info.background && scheduler.inner.tree.cancelling_owner(TaskNode::of(&record).parent()).is_some() { scheduler.inner.state.lock().cascade = true; scheduler.schedule_reconcile(); continue; }
                if mode == InvocationMode::Abort && record.info.restarts.unwrap_or(0) as f64 >= scheduler.restart_limit() {
                    let message = format!("Abort handler of task {id} ({}) was cut off {} times", record.info.kind, record.info.restarts.unwrap_or(0));
                    scheduler.terminate(&tx, &record, TaskOutcome::Faulted { error: TaskOutcomeError { message: message.clone().into(), detail: Some(crate::wire::encode(&serde_json::json!({"reason":"restart_limit"}))) } }).await?;
                    capture_faulted.lock().push((record.clone(), message));
                    continue;
                }
                if snapshot.is_none() { snapshot = Some(read_registry(scheduler.inner.registry.as_ref())?); }
                let snapshot = snapshot.as_ref().unwrap();
                let (task, resolved) = match scheduler.resolve(&record, snapshot) {
                    Ok(ready) => ready,
                    Err(reason) => { if mode == InvocationMode::Abort && record.info.abort_reason.is_none() { scheduler.terminate(&tx, &record, TaskOutcome::Orphaned { reason: reason.text().into() }).await?; } continue; }
                };
                let checkpoint = resolved.state.checkpoint().unwrap().clone();
                if resolved != *record || record.state.status() != TaskStatus::Running { tx.set_task(with_state(resolved, TaskState::Running { checkpoint, memos: None }))?; }
                let controller = AbortController::new();
                let (done, _) = tokio::sync::watch::channel(false);
                let invocation = Arc::new(Invocation { task: id, name: task.name().clone(), extension: snapshot.task(task.name()).and_then(|installed| installed.extension.clone()), conversation: record.info.conversation_id, mode, context: Mutex::new(with_abort_signal(controller.signal(), &scheduler.inner.context)), record: Mutex::new((*record).clone()), fault: Mutex::new(None), controller, registry: Mutex::new(snapshot.clone()), agent: Mutex::new(None), ended: AtomicBool::new(false), done, watches: Mutex::new(IndexMap::new()), next_watch: AtomicU64::new(1), timers: Mutex::new(IndexMap::new()), next_timer: AtomicU64::new(1) });
                { let mut state = scheduler.inner.state.lock(); state.runnable.shift_remove(&id); state.invocations.insert(id, invocation.clone()); }
                capture.lock().push(Reservation { invocation, task });
            }
            Ok(())
        }, &self.inner.context);
        let scheduler = self.clone();
        Box::pin(async move {
            if let Err(error) = operation.await {
                for reservation in reservations.lock().drain(..) {
                    scheduler.end(&reservation.invocation);
                    reservation.invocation.done.send_replace(true);
                }
                return Err(error);
            }
            for (record, message) in std::mem::take(&mut *faulted.lock()) {
                scheduler.report(
                    Error::message(message),
                    "task_faulted",
                    Some(scheduler.about_task(&record)),
                    Some("restart_limit"),
                );
            }
            let mut state = scheduler.inner.state.lock();
            if !state.abandoned.is_empty() {
                state.abandoned.clear();
                state.dirty = true;
            }
            drop(state);
            Ok(std::mem::take(&mut *reservations.lock()))
        })
    }
    fn start(&self, reservation: Reservation) {
        let scheduler = self.clone();
        self.inner.session.spawn(async move {
            let invocation = reservation.invocation.clone();
            let result = std::panic::AssertUnwindSafe(scheduler.run(reservation))
                .catch_unwind()
                .await;
            if let Err(panic) = result {
                scheduler.report(
                    crate::session::panic_error(panic),
                    "task_error",
                    Some(invocation.subject()),
                    None,
                );
            }
            scheduler.end(&invocation);
            invocation.done.send_replace(true);
            scheduler.kick();
        });
    }
    async fn run(&self, reservation: Reservation) {
        let invocation = reservation.invocation;
        let definition = reservation.task;
        let mut reported: Option<Option<AnyTask>> = None;
        let driver: Arc<dyn TaskDriver> = Arc::new(Driver {
            scheduler: self.clone(),
            invocation: invocation.clone(),
        });
        self.step_gate(invocation.task).await;
        let mut next = self
            .step(&invocation, None, &definition, &mut reported)
            .await;
        while let Some(current) = next {
            if self.inner.state.lock().closing {
                return;
            }
            let phase = telemetry::Phase::begin(self, invocation.clone(), &current);
            *invocation.agent.lock() = None;
            let checkpoint = current.state.checkpoint().unwrap().clone();
            let handler = if invocation.mode == InvocationMode::Abort {
                Some(definition.0.abort.clone())
            } else {
                crate::wire::get(&checkpoint, "phase")
                    .and_then(JsonValue::as_string)
                    .and_then(|phase| definition.0.phases.get(phase))
                    .cloned()
            };
            let context = invocation.context.lock().clone();
            let result = std::panic::AssertUnwindSafe(async {
                let handler = handler.ok_or_else(|| {
                    Error::message(format!(
                        "Task {} has no handler for checkpoint phase",
                        current.info.kind
                    ))
                })?;
                handler(current, driver.clone(), context).await
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)));
            self.step_gate(invocation.task).await;
            next = self
                .step(
                    &invocation,
                    Some((checkpoint, result)),
                    &definition,
                    &mut reported,
                )
                .await;
            drop(phase);
        }
    }
    fn step_gate(&self, task: TaskId) -> futures::future::BoxFuture<'static, ()> {
        match &self.inner.options.step_gate {
            Some(gate) => gate(task),
            None => Box::pin(async {}),
        }
    }
    async fn step(
        &self,
        invocation: &Arc<Invocation>,
        previous: Option<(JsonValue, Result<()>)>,
        definition: &AnyTask,
        reported: &mut Option<Option<AnyTask>>,
    ) -> Option<TaskRecord> {
        let scheduler = self.clone();
        let invocation = invocation.clone();
        let call = invocation.clone();
        let definition = definition.clone();
        let old_reported = reported.clone();
        let capture = Arc::new(Mutex::new(old_reported));
        let update = capture.clone();
        let fault = Arc::new(Mutex::new(None));
        let capture_fault = fault.clone();
        let context = without_abort_signal(&call.context.lock().clone());
        let result = self
            .inner
            .session
            .commit(
                move |tx| async move {
                    let current = scheduler
                        .inner
                        .tree
                        .live(invocation.task)
                        .filter(|r| r.state.status() == TaskStatus::Running);
                    let Some(current) = current.filter(|_| !scheduler.inner.state.lock().closing)
                    else {
                        scheduler.end(&invocation);
                        return Ok(None);
                    };
                    let cancelled = invocation.mode == InvocationMode::Run
                        && (current.info.abort_requested
                            || (!current.info.background
                                && scheduler
                                    .inner
                                    .tree
                                    .cancelling_owner(TaskNode::of(&current).parent())
                                    .is_some()));
                    if cancelled {
                        scheduler.end(&invocation);
                        scheduler.schedule_reconcile();
                        return Ok(None);
                    }
                    let failure = if let Some((checkpoint, result)) = previous {
                        if invocation.mode == InvocationMode::Abort {
                            Some(result.err().unwrap_or_else(|| {
                                Error::message(format!(
                                    "Abort handler of task {} returned without a terminal outcome",
                                    invocation.task
                                ))
                            }))
                        } else if let Err(error) = result {
                            Some(error)
                        } else if current.state.checkpoint() == Some(&checkpoint) {
                            let phase = crate::wire::get(&checkpoint, "phase")
                                .map(|value| value.to_utf8().unwrap_or_else(|| value.to_string()))
                                .unwrap_or_else(|| "undefined".into());
                            Some(Error::message(format!(
                                "Task {} phase {phase} returned without durable progress",
                                current.info.kind
                            )))
                        } else {
                            let snapshot = read_registry(scheduler.inner.registry.as_ref())?;
                            *invocation.registry.lock() = snapshot.clone();
                            let next = snapshot
                                .task(&current.info.kind)
                                .map(|installed| installed.task.clone());
                            if !next
                                .as_ref()
                                .is_some_and(|next| next.same_definition(&definition))
                            {
                                if next.as_ref().is_some_and(|next| next.can_reserve(&current)) {
                                    tx.set_task(with_state(
                                        (*current).clone(),
                                        TaskState::pending(
                                            current.state.checkpoint().unwrap().clone(),
                                        ),
                                    ))?;
                                    scheduler.end(&invocation);
                                    return Ok(None);
                                }
                                let different =
                                    update.lock().as_ref().is_none_or(|reported| {
                                        match (reported, &next) {
                                            (None, None) => false,
                                            (Some(a), Some(b)) => !a.same_definition(b),
                                            _ => true,
                                        }
                                    });
                                if different {
                                    *update.lock() = Some(next.clone());
                                    scheduler.report(
                                        Error::message(format!(
                                            "Task {} keeps running under its old {} definition",
                                            current.id(),
                                            current.info.kind
                                        )),
                                        "task_outdated",
                                        Some(invocation.subject()),
                                        Some(if next.is_some() {
                                            "incompatible_task"
                                        } else {
                                            "missing_task"
                                        }),
                                    );
                                }
                            }
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(error) = failure {
                        *invocation.fault.lock() = Some(TaskOutcome::Faulted {
                            error: TaskOutcomeError {
                                message: error.message_text(),
                                detail: None,
                            },
                        });
                        scheduler.end(&invocation);
                        scheduler
                            .terminate(
                                &tx,
                                &current,
                                TaskOutcome::Faulted {
                                    error: TaskOutcomeError {
                                        message: error.message_text(),
                                        detail: None,
                                    },
                                },
                            )
                            .await?;
                        *capture_fault.lock() = Some(error);
                        Ok(None)
                    } else {
                        Ok(Some((*current).clone()))
                    }
                },
                &context,
            )
            .await;
        *reported = capture.lock().take();
        match result {
            Ok(current) => {
                if let Some(error) = fault.lock().take() {
                    let context = call.context.lock().clone();
                    self.report_in(
                        error,
                        "task_faulted",
                        Some(call.subject()),
                        Some("threw"),
                        Some(context),
                    );
                }
                current
            }
            Err(error) => {
                self.end(&call);
                self.fail(error);
                None
            }
        }
    }
    fn end(&self, invocation: &Invocation) {
        if invocation.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut state = self.inner.state.lock();
            if state
                .invocations
                .get(&invocation.task)
                .is_some_and(|current| std::ptr::eq(current.as_ref(), invocation))
            {
                state.invocations.shift_remove(&invocation.task);
                if self.inner.tree.live(invocation.task).is_some() {
                    state.runnable.insert(invocation.task);
                }
            }
        }
        for watch in std::mem::take(&mut *invocation.watches.lock()).into_values() {
            watch.stop();
        }
        for disarm in std::mem::take(&mut *invocation.timers.lock()).into_values() {
            disarm();
        }
        invocation
            .controller
            .abort_with_reason(SignalReason::Error(AbortError::new(
                invocation.ended_error().to_string(),
            )));
    }
    async fn terminate(
        &self,
        tx: &Transaction,
        record: &TaskRecord,
        outcome: TaskOutcome,
    ) -> Result<()> {
        load_scopes(
            &self.inner.tree,
            self.inner.storage.as_ref(),
            &self.inner.context,
        )
        .await?;
        let overlay = TaskOverlay::from_transaction(tx);
        if !self
            .inner
            .tree
            .owned_live(record.id(), Some(&overlay), true, None)
            .is_empty()
        {
            tx.set_task(with_state(
                record.clone(),
                TaskState::Completing { outcome },
            ))?;
        } else {
            tx.set_task(with_state(
                record.clone(),
                TaskState::Terminal {
                    outcome: outcome.clone(),
                },
            ))?;
            (self.inner.options.settle_outcome)(tx.clone(), record.clone(), outcome).await?;
        }
        Ok(())
    }
    async fn commit_state(
        &self,
        tx: &Transaction,
        invocation: &Invocation,
        mut current: TaskRecord,
        next: TaskState,
    ) -> Result<()> {
        if let TaskState::Waiting { on, policy, .. } = &next {
            validate_wait(
                &self.inner.tree,
                self.inner.storage.as_ref(),
                &self.inner.context,
                invocation.mode,
                current.id(),
                on,
                *policy,
                &TaskOverlay::from_transaction(tx),
            )
            .await?;
        }
        if let TaskState::Sleeping { until, .. } = &next {
            validate_sleep(invocation.mode, current.id(), *until)?;
        }
        if let TaskState::Terminal { outcome } = &next {
            let overlay = TaskOverlay::from_transaction(tx);
            load_scopes(
                &self.inner.tree,
                self.inner.storage.as_ref(),
                &self.inner.context,
            )
            .await?;
            for record in overlay.tasks.values() {
                self.inner
                    .tree
                    .load_chain(
                        TaskNode::of(record).parent(),
                        Some(&overlay),
                        self.inner.storage.as_ref(),
                        &self.inner.context,
                    )
                    .await?;
            }
            if !self
                .inner
                .tree
                .owned_live(current.id(), Some(&overlay), true, None)
                .is_empty()
            {
                current.info.restarts = None;
                return tx.set_task(with_state(
                    current,
                    TaskState::Completing {
                        outcome: outcome.clone(),
                    },
                ));
            }
        }
        if crate::tasks::encode(&current.state)? != crate::tasks::encode(&next)? {
            current.info.restarts = None;
        }
        tx.set_task(with_state(current, next))
    }
    fn schedule_reconcile(&self) {
        let spawn = {
            let mut state = self.inner.state.lock();
            if state.closing || self.inner.read_only {
                false
            } else {
                state.reconcile_dirty = true;
                if state.reconciling {
                    false
                } else {
                    state.reconciling = true;
                    true
                }
            }
        };
        if spawn {
            let scheduler = self.clone();
            self.inner.session.spawn(async move {
                let _ = scheduler.reconcile().await;
            });
        }
    }
    fn reconcile(&self) -> StorageFuture<()> {
        let scheduler = self.clone();
        Box::pin(async move {
            loop {
                let again = {
                    let mut state = scheduler.inner.state.lock();
                    if state.reconcile_dirty && !state.closing {
                        state.reconcile_dirty = false;
                        state.reconciling_work =
                            state.cascade || state.finalize_pending || !state.fail_fast.is_empty();
                        state.finalize_pending = false;
                        true
                    } else {
                        state.reconciling = false;
                        false
                    }
                };
                if !again {
                    break;
                }
                let reconcile = scheduler.clone();
                let operation = scheduler.inner.session.commit(
                    move |tx| async move {
                        if reconcile.inner.state.lock().closing {
                            return Ok(());
                        }
                        reconcile.reconcile_on_line(&tx).await
                    },
                    &scheduler.inner.context,
                );
                if let Err(error) = operation.await {
                    scheduler.fail(error);
                }
                scheduler.inner.state.lock().reconciling_work = false;
                scheduler.schedule_wake();
                scheduler.settle_idle();
            }
            Ok(())
        })
    }
    async fn reconcile_on_line(&self, tx: &Transaction) -> Result<()> {
        load_scopes(
            &self.inner.tree,
            self.inner.storage.as_ref(),
            &self.inner.context,
        )
        .await?;
        let cascade = {
            let mut state = self.inner.state.lock();
            std::mem::take(&mut state.cascade)
        };
        let queued = if cascade {
            self.queued_conversations().await?
        } else {
            Vec::new()
        };
        let intents = self.inner.tree.cancellation_intents();
        let intent_ids = intents
            .iter()
            .map(|record| record.id())
            .collect::<HashSet<_>>();
        let mut marks = IndexMap::<TaskId, (TaskRecord, Option<AbortReason>)>::new();
        let mut mark = |record: &TaskRecord, reason: Option<AbortReason>| {
            if marks
                .get(&record.id())
                .is_some_and(|(_, reason)| reason.is_none())
                || (record.info.abort_requested
                    && (record.info.abort_reason.is_none() || reason.is_some()))
            {
                return;
            }
            marks.insert(record.id(), (record.clone(), reason));
        };
        for owner in intents {
            let restart =
                owner.info.abort_reason == Some(AbortReason::Restart) && !failed_outcome(&owner);
            for reached in self
                .inner
                .tree
                .walk_down(Node::Task(owner.id()), None, false, |id| {
                    !intent_ids.contains(&id)
                })
            {
                if let Some(record) = reached.live.filter(|_| !reached.node.background) {
                    mark(&record, restart.then_some(AbortReason::Restart));
                }
            }
        }
        let checks = { std::mem::take(&mut self.inner.state.lock().fail_fast) };
        for id in checks {
            let Some(record) = self.inner.tree.live(id) else {
                continue;
            };
            let TaskState::Waiting { on, .. } = &record.state else {
                continue;
            };
            let mut failed = false;
            for id in on {
                let record = match self.inner.tree.live(*id) {
                    Some(record) => Some((*record).clone()),
                    None => self.inner.storage.task(*id, &self.inner.context).await?,
                };
                if record.as_ref().is_some_and(failed_outcome) {
                    failed = true;
                    break;
                }
            }
            if failed {
                for id in on {
                    if let Some(record) = self.inner.tree.live(*id).filter(|r| !failed_outcome(r)) {
                        mark(&record, None);
                    }
                }
            }
        }
        for (_, (record, reason)) in marks {
            tx.set_task(with_abort_mark(record, reason))?;
        }
        for id in queued {
            if self
                .inner
                .tree
                .cancelling_owner(Node::Conversation(id))
                .is_some()
            {
                (self.inner.options.withdraw_inputs)(tx.clone(), id).await?;
            }
        }
        let mut round = std::mem::take(&mut self.inner.state.lock().held)
            .into_iter()
            .collect::<Vec<_>>();
        let mut done = HashSet::new();
        let mut overlay = TaskOverlay::from_transaction(tx);
        while !round.is_empty() {
            let mut next = IndexSet::new();
            for id in round {
                let Some(record) = overlay
                    .tasks
                    .get(&id)
                    .cloned()
                    .or_else(|| self.inner.tree.live(id))
                else {
                    continue;
                };
                let TaskState::Completing { outcome } = &record.state else {
                    continue;
                };
                if done.contains(&id)
                    || !self
                        .inner
                        .tree
                        .owned_live(id, Some(&overlay), true, Some(&done))
                        .is_empty()
                {
                    continue;
                }
                let terminal = with_state(
                    (*record).clone(),
                    TaskState::Terminal {
                        outcome: outcome.clone(),
                    },
                );
                tx.set_task(terminal.clone())?;
                self.inner.tree.stage(&mut overlay, terminal);
                done.insert(id);
                if matches!(
                    outcome,
                    TaskOutcome::Faulted { .. } | TaskOutcome::Orphaned { .. }
                ) {
                    (self.inner.options.settle_outcome)(
                        tx.clone(),
                        (*record).clone(),
                        outcome.clone(),
                    )
                    .await?;
                }
                if let Some(above) = self
                    .inner
                    .tree
                    .nearest_live_above(TaskNode::of(&record), Some(&overlay))
                    .filter(|r| r.state.status() == TaskStatus::Completing)
                {
                    next.insert(above.id());
                }
            }
            round = next.into_iter().collect();
        }
        self.schedule_sweep();
        Ok(())
    }
    async fn queued_conversations(&self) -> Result<Vec<ConversationId>> {
        let storage = self.inner.storage.clone();
        let context = self.inner.context.clone();
        let submissions = scan_all(move |cursor| {
            storage.scan_submissions(
                &SubmissionQuery {
                    status: Some(SubmissionStatus::Queued),
                    ..Default::default()
                },
                256,
                cursor.as_ref(),
                &context,
            )
        })
        .await?;
        let ids = submissions
            .into_iter()
            .filter(|s| matches!(s.state, SubmissionState::InputQueued))
            .map(|s| s.info.conversation_id)
            .collect::<IndexSet<_>>();
        for id in &ids {
            self.inner
                .tree
                .load_chain(
                    Node::Conversation(*id),
                    None,
                    self.inner.storage.as_ref(),
                    &self.inner.context,
                )
                .await?;
        }
        Ok(ids.into_iter().collect())
    }
    fn schedule_sweep(&self) {
        {
            let mut state = self.inner.state.lock();
            if state.sweep_pending || state.closing {
                return;
            }
            state.sweep_pending = true;
        }
        let scheduler = self.clone();
        drop(self.inner.session.read_on_line(move || async move {
            scheduler.inner.state.lock().sweep_pending = false;
            scheduler.inner.tree.sweep();
            Ok(())
        }));
    }
    pub fn abort(&self, id: TaskId, context: &Context) -> StorageFuture<AbortResult> {
        self.abort_with(id, context, false)
    }
    fn abort_with(
        &self,
        id: TaskId,
        context: &Context,
        keep_restart: bool,
    ) -> StorageFuture<AbortResult> {
        let scheduler = self.clone();
        let operation = self.inner.session.commit(
            move |tx| async move {
                let current = tx
                    .task(id)
                    .await?
                    .ok_or_else(|| Error::message(format!("Task {id} does not exist")))?;
                if current.state.status() == TaskStatus::Terminal {
                    return Ok((AbortResult::Terminal, None));
                }
                if keep_restart && current.info.abort_reason == Some(AbortReason::Restart) {
                    return Ok((AbortResult::Marked, None));
                }
                let invocation = scheduler.inner.state.lock().invocations.get(&id).cloned();
                if invocation.is_none() && current.state.status() != TaskStatus::Completing {
                    load_scopes(
                        &scheduler.inner.tree,
                        scheduler.inner.storage.as_ref(),
                        &scheduler.inner.context,
                    )
                    .await?;
                    if scheduler
                        .inner
                        .tree
                        .owned_live(id, None, true, None)
                        .is_empty()
                        && let Err(reason) = scheduler
                            .resolve(&current, &read_registry(scheduler.inner.registry.as_ref())?)
                    {
                        scheduler
                            .terminate(
                                &tx,
                                &current,
                                TaskOutcome::Orphaned {
                                    reason: reason.text().into(),
                                },
                            )
                            .await?;
                        return Ok((AbortResult::Marked, None));
                    }
                }
                if !current.info.abort_requested || current.info.abort_reason.is_some() {
                    tx.set_task(with_abort_mark(current, None))?;
                }
                Ok((
                    AbortResult::Marked,
                    invocation.filter(|i| i.mode == InvocationMode::Run),
                ))
            },
            context,
        );
        let scheduler = self.clone();
        let context = context.clone();
        crate::storage::line::eager_on(&self.inner.session, async move {
            let (result, invocation) = operation.await?;
            if let Some(invocation) = invocation {
                let session = scheduler.inner.session.clone();
                let wait = async move {
                    tokio::select! { _ = invocation.finished() => Ok(()), failure = session.failed() => failure }
                };
                // Both operations already run independently. Keep this join on
                // the Session queue so the caller precedes a fresh abort phase.
                crate::storage::line::observe(wait, &context)
                    .await
                    .map_err(|error| Error::message(error.to_string()))??;
            }
            Ok(result)
        })
    }
    pub fn abort_conversation(
        &self,
        conversation: ConversationId,
        background: bool,
        context: &Context,
    ) -> StorageFuture<()> {
        let scheduler = self.clone();
        let operation = self.inner.session.commit(
            move |tx| async move {
                load_scopes(
                    &scheduler.inner.tree,
                    scheduler.inner.storage.as_ref(),
                    &scheduler.inner.context,
                )
                .await?;
                let queued = scheduler.queued_conversations().await?;
                let mut reached = Vec::new();
                for task in scheduler.inner.tree.walk_down(
                    Node::Conversation(conversation),
                    None,
                    background,
                    |_| true,
                ) {
                    let Some(record) = task.live.filter(|_| background || !task.node.background)
                    else {
                        continue;
                    };
                    reached.push(record.id());
                    if !record.info.abort_requested || record.info.abort_reason.is_some() {
                        tx.set_task(with_abort_mark((*record).clone(), None))?;
                    }
                }
                for id in queued {
                    if scheduler.inner.tree.scope_contains(
                        Node::Conversation(id),
                        Scope::Conversation(conversation),
                        background,
                    ) == Some(true)
                    {
                        (scheduler.inner.options.withdraw_inputs)(tx.clone(), id).await?;
                    }
                }
                Ok(reached)
            },
            context,
        );
        let scheduler = self.clone();
        let context = context.clone();
        eager(async move {
            let reached = operation.await?;
            if background {
                for id in reached {
                    scheduler.wait_for_task(id, &context).await?;
                }
            }
            scheduler.wait_for_idle(Some(conversation), &context).await
        })
    }
    pub fn wake(&self) -> Wake {
        if self.inner.read_only {
            return Wake::default();
        }
        let mut wake = {
            let state = self.inner.state.lock();
            Wake {
                active: !state.invocations.is_empty()
                    || state.reconciling_work
                    || state.cascade
                    || state.finalize_pending
                    || !state.fail_fast.is_empty(),
                at: None,
            }
        };
        if wake.active {
            // Active work already determines the foreground bit. Only sleeping
            // records can contribute a deadline; cloning every live record for
            // every phase makes a deep concurrently running tree quadratic.
            let sleeping = self
                .inner
                .state
                .lock()
                .sleeping
                .iter()
                .copied()
                .collect::<Vec<_>>();
            for id in sleeping {
                if self.inner.state.lock().invocations.contains_key(&id) {
                    continue;
                }
                if let Some(record) = self.inner.tree.live(id)
                    && self.not_due(&record)
                    && let TaskState::Sleeping { until, .. } = record.state
                    && wake.at.is_none_or(|at| until < at)
                {
                    wake.at = Some(until);
                }
            }
            return wake;
        }
        let mut snapshot = None;
        let mut snapshot_failed = false;
        for record in self.inner.tree.live_records() {
            if record.state.status() == TaskStatus::Completing
                || self
                    .inner
                    .state
                    .lock()
                    .invocations
                    .contains_key(&record.id())
            {
                continue;
            }
            if !self.waiting_on(&record, true).is_empty() {
                continue;
            }
            if self.not_due(&record) {
                if let TaskState::Sleeping { until, .. } = record.state
                    && wake.at.is_none_or(|at| until < at)
                {
                    wake.at = Some(until);
                }
                continue;
            }
            if wake.active || self.parked(&record) {
                continue;
            }
            if !record.info.abort_requested || record.info.abort_reason.is_some() {
                if snapshot.is_none() && !snapshot_failed {
                    match read_registry(self.inner.registry.as_ref()) {
                        Ok(view) => snapshot = Some(view),
                        Err(error) => {
                            snapshot_failed = true;
                            self.report_host(error, "registry");
                        }
                    }
                }
                if snapshot.as_ref().is_some_and(|snapshot| {
                    self.fit(&record, snapshot.task(&record.info.kind).map(|i| &i.task))
                        .is_err()
                }) {
                    continue;
                }
            }
            wake.active = true;
        }
        wake
    }
    fn schedule_wake(&self) {
        {
            let mut state = self.inner.state.lock();
            if state.wake_pending || state.closing || self.inner.read_only {
                return;
            }
            state.wake_pending = true;
        }
        let scheduler = self.clone();
        self.inner.session.spawn(async move {
            {
                let mut state = scheduler.inner.state.lock();
                state.wake_pending = false;
                if state.closing {
                    return;
                }
            }
            let wake = scheduler.wake();
            if let Some(on_wake) = &scheduler.inner.options.on_wake {
                if scheduler.inner.state.lock().last_wake == Some(wake) {
                    return;
                }
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_wake(wake)))
                    .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
                {
                    Ok(()) => {
                        scheduler.inner.state.lock().last_wake = Some(wake);
                    }
                    Err(error) => scheduler.report_host(error, "onWake"),
                }
            } else {
                let at = scheduler
                    .inner
                    .state
                    .lock()
                    .enabled
                    .then_some(wake.at)
                    .flatten();
                scheduler.arm_timer(at);
            }
        });
    }
    fn arm_timer(&self, at: Option<f64>) {
        let (old, registration) = {
            let mut state = self.inner.state.lock();
            let at = at.filter(|_| !state.closing);
            if state.timer.as_ref().map(|timer| timer.at) == at {
                return;
            }
            let old = state.timer.take();
            let registration = at.map(|at| {
                let generation = state.next_wake_timer;
                state.next_wake_timer = state.next_wake_timer.wrapping_add(1);
                // A host or another runtime thread can fire before arming returns.
                let (keepalive, released) = tokio::sync::oneshot::channel();
                state.timer = Some(WakeTimer {
                    at,
                    generation,
                    disarm: None,
                    _keepalive: keepalive,
                });
                (at, generation, released)
            });
            (old, registration)
        };
        if let Some(disarm) = old.and_then(|timer| timer.disarm) {
            disarm();
        }
        let Some((at, generation, released)) = registration else {
            return;
        };
        // A foreground deadline is pending work. Retain the scheduler outside
        // its own timer state, so runtime shutdown can drop this owner too.
        let owner = self.clone();
        pi_chord::rt::spawn(async move {
            let _owner = owner;
            let _ = released.await;
        });
        let weak = Arc::downgrade(&self.inner);
        let disarm = self.inner.timers.foreground(
            Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    let scheduler = TaskScheduler { inner };
                    let current = {
                        let mut state = scheduler.inner.state.lock();
                        if state
                            .timer
                            .as_ref()
                            .is_some_and(|timer| timer.generation == generation)
                        {
                            state.timer.take();
                            true
                        } else {
                            false
                        }
                    };
                    if current {
                        scheduler.kick();
                    }
                }
            }),
            at - self.now(),
        );
        let unused = {
            let mut state = self.inner.state.lock();
            if let Some(timer) = state
                .timer
                .as_mut()
                .filter(|timer| timer.generation == generation)
            {
                timer.disarm = Some(disarm);
                None
            } else {
                Some(disarm)
            }
        };
        if let Some(disarm) = unused {
            disarm();
        }
    }
    fn seal(&self) {
        let expiry = {
            let mut state = self.inner.state.lock();
            if state.closing {
                return;
            }
            state.closing = true;
            state.resumed.clear();
            state.contexts.clear();
            state.expiry_generation += 1;
            state.expiry.take()
        };
        for record in self.inner.tree.live_records() {
            self.inner.active_metric.add(
                -1.0,
                pi_telemetry::attributes! {"task" => record.info.kind.clone()},
            );
        }
        if let Some((_, disarm)) = expiry {
            disarm();
        }
        let registry = self
            .inner
            .subscriptions
            .lock()
            .as_mut()
            .and_then(|(_, _, registry)| registry.take());
        if let Some(registry) = registry
            && let Err(error) = registry.close()
        {
            self.report_host(error, "registry");
        }
        let timer = {
            let mut state = self.inner.state.lock();
            state.timer.take()
        };
        if let Some(disarm) = timer.and_then(|timer| timer.disarm) {
            disarm();
        }
        let error = closed_error(&self.inner.session);
        self.inner.task_waiters.reject_all(error.clone());
        self.inner.idle_waiters.reject_all(error);
        // Invocation signals are deliberately left alone until every Session
        // close listener, including its document observers, has run.
    }
    async fn join(&self) {
        let stopped = self
            .inner
            .state
            .lock()
            .invocations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for invocation in &stopped {
            invocation.controller.abort();
        }
        for invocation in &stopped {
            invocation.finished().await;
        }
        if stopped.is_empty() {
            return;
        }
        let scheduler = self.clone();
        let _ = self
            .inner
            .session
            .write_at_close(move || {
                Ok(stopped
                    .into_iter()
                    .filter_map(|invocation| scheduler.inner.tree.live(invocation.task))
                    .filter_map(|record| {
                        if let TaskState::Running { checkpoint, .. } = &record.state {
                            Some(StorageWrite::Task {
                                value: Box::new(with_state(
                                    (*record).clone(),
                                    TaskState::pending(checkpoint.clone()),
                                )),
                            })
                        } else {
                            None
                        }
                    })
                    .collect())
            })
            .await;
    }
}
fn with_abort_mark(mut record: TaskRecord, reason: Option<AbortReason>) -> TaskRecord {
    record.info.abort_requested = true;
    record.info.abort_reason = reason;
    record.info.restarts = None;
    record
}

struct Driver {
    scheduler: TaskScheduler,
    invocation: Arc<Invocation>,
}
impl TaskDriver for Driver {
    fn check(&self) -> Result<()> {
        if self.invocation.ended.load(Ordering::Acquire) {
            Err(self.invocation.ended_error())
        } else {
            Ok(())
        }
    }
    fn task_id(&self) -> TaskId {
        self.invocation.task
    }
    fn task_name(&self) -> JsonString {
        self.invocation.name.clone()
    }
    fn conversation_id(&self) -> ConversationId {
        self.invocation.conversation
    }
    fn signal(&self) -> pi_chord::context::AbortSignal {
        self.invocation.controller.signal()
    }
    fn mode(&self) -> InvocationMode {
        self.invocation.mode
    }
    fn session(&self) -> Session {
        self.scheduler.inner.session.clone()
    }
    fn registry(&self) -> RegistryView {
        self.invocation.registry.lock().clone()
    }
    fn settings(&self) -> Result<Settings> {
        self.scheduler.settings()
    }
    fn models(&self) -> Result<pi_ai::models::Models> {
        super::models::read_models(&self.scheduler.inner.options.models)
    }
    fn model_collection(&self) -> Result<Arc<dyn super::models::ModelCollection>> {
        match &self.scheduler.inner.options.model_collection {
            Some(reader) => super::models::read_collection(reader),
            None => self.models().map(super::models::sdk_collection),
        }
    }
    fn env(&self, context: &Context) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>> {
        let resolve = self.scheduler.inner.options.env.clone();
        let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve(self.invocation.conversation, context.clone())
        }));
        match call {
            Err(panic) => ready(Err(crate::session::panic_error(panic))),
            Ok(operation) => eager(async move {
                std::panic::AssertUnwindSafe(operation)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
            }),
        }
    }
    fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<super::conversation::ConversationHandle>> {
        let Some(resolve) = &self.scheduler.inner.options.conversation else {
            return ready(Err(Error::message("Conversation services are unavailable")));
        };
        let invocation = self.invocation.clone();
        let binding = super::conversation::InvocationBinding::new(self.signal(), move || {
            if invocation.ended.load(Ordering::Acquire) {
                Err(invocation.ended_error())
            } else {
                Ok(())
            }
        })
        .retaining(self.scheduler.clone());
        let context = context.clone();
        let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve(id, binding, context)
        }));
        match call {
            Ok(operation) => operation,
            Err(panic) => ready(Err(crate::session::panic_error(panic))),
        }
    }
    fn report_extension(&self, error: Error, extension: Option<JsonString>) {
        self.scheduler.report_extension(
            error,
            extension,
            self.invocation.conversation,
            Some(self.invocation.report_task()),
        );
    }
    fn agent(&self, context: &Context) -> StorageFuture<AgentView> {
        if let Err(error) = self.check() {
            return ready(Err(error));
        }
        let shared = {
            let mut current = self.invocation.agent.lock();
            if let Some(shared) = current.as_ref() {
                shared.clone()
            } else {
                let scheduler = self.scheduler.clone();
                let conversation = self.invocation.conversation;
                let request = AgentRequest {
                    session: self.session(),
                    conversation,
                    snapshot: self.registry(),
                    settings: self.scheduler.inner.options.settings.clone(),
                    context: self.invocation.context.lock().clone(),
                    report: Arc::new(move |error, extension| {
                        scheduler.report_extension(error, extension, conversation, None)
                    }),
                };
                let resolve = self.scheduler.inner.options.agent.clone();
                let shared = eager(async move { resolve(request).await }).shared();
                *current = Some(shared.clone());
                shared
            }
        };
        let context = context.clone();
        eager(async move {
            match crate::storage::line::observe(shared, &context).await {
                Ok(result) => result,
                Err(pi_chord::context::AwaitError::Aborted(error)) => Err(error.into()),
                Err(pi_chord::context::AwaitError::Task(error)) => {
                    Err(Error::message(error.to_string()))
                }
            }
        })
    }

    fn current(&self) -> Option<Arc<TaskRecord>> {
        self.scheduler.inner.tree.live(self.invocation.task)
    }
    fn context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        context: &Context,
    ) -> StorageFuture<super::context::ContextView> {
        let previous = self
            .scheduler
            .inner
            .state
            .lock()
            .contexts
            .get(&conversation)
            .map(|kept| kept.range.clone());
        let read = super::context::read_context_from(
            &self.scheduler.inner.session,
            self.scheduler.inner.storage.clone(),
            conversation,
            context,
            at,
            previous,
        );
        let scheduler = self.scheduler.clone();
        let invocation = self.invocation.clone();
        eager(async move {
            let read = read.await?;
            if let Some(range) = read.range {
                let eligible = {
                    let state = scheduler.inner.state.lock();
                    !state.closing
                        && !invocation.ended.load(Ordering::Acquire)
                        && state
                            .contexts
                            .get(&conversation)
                            .is_none_or(|kept| kept.range.bounds.tail <= range.bounds.tail)
                };
                if !eligible {
                    return Ok(read.view);
                }
                let idle = scheduler.inner.tree.idle(Some(conversation));
                if idle {
                    let retention = scheduler.context_retention();
                    if retention.is_nan() || retention <= 0.0 {
                        return Ok(read.view);
                    }
                }
                let since = if idle {
                    let kept = scheduler
                        .inner
                        .state
                        .lock()
                        .contexts
                        .get(&conversation)
                        .and_then(|kept| kept.idle_since);
                    Some(kept.unwrap_or_else(|| scheduler.now()))
                } else {
                    None
                };
                let mut state = scheduler.inner.state.lock();
                let kept = state.contexts.get(&conversation);
                if !state.closing
                    && !invocation.ended.load(Ordering::Acquire)
                    && kept.is_none_or(|kept| kept.range.bounds.tail <= range.bounds.tail)
                {
                    let idle_since = if idle {
                        kept.and_then(|kept| kept.idle_since).or(since)
                    } else {
                        None
                    };
                    state
                        .contexts
                        .insert(conversation, KeptContext { range, idle_since });
                }
                drop(state);
                if idle {
                    scheduler.schedule_expiry();
                }
            }
            Ok(read.view)
        })
    }
    fn commit(&self, change: Change, context: &Context) -> StorageFuture<()> {
        if let Err(error) = self.check() {
            return ready(Err(error));
        }
        let scheduler = self.scheduler.clone();
        let invocation = self.invocation.clone();
        let phase = Arc::new(Mutex::new(None::<JsonString>));
        let capture = phase.clone();
        let invocation_context = invocation.context.lock().clone();
        let operation = self.session().commit_scoped(
            move |tx| async move {
                if invocation.ended.load(Ordering::Acquire) {
                    return Err(invocation.ended_error());
                }
                if scheduler.inner.state.lock().closing {
                    return Err(closed_error(&scheduler.inner.session));
                }
                let current = scheduler.inner.tree.live(invocation.task).ok_or_else(|| {
                    Error::message(format!("Task {} is terminal", invocation.task))
                })?;
                if current.state.status() != TaskStatus::Running {
                    let value = crate::tasks::encode(&current.state)?;
                    let status = crate::wire::tag(&value, "status").map_err(Error::message)?;
                    return Err(Error::message(format!(
                        "Task {} is {status}",
                        invocation.task
                    )));
                }
                if invocation.mode == InvocationMode::Run && current.info.abort_requested {
                    return Err(Error::message(format!(
                        "Task {} has a durable abort mark",
                        invocation.task
                    )));
                }
                let next = change(tx.clone(), (*current).clone()).await?;
                if let Some(next) = next {
                    if let TaskState::Running { checkpoint, .. } = &next {
                        let next_phase =
                            crate::wire::get(checkpoint, "phase").and_then(JsonValue::as_string);
                        let current_phase = current
                            .state
                            .checkpoint()
                            .and_then(|checkpoint| crate::wire::get(checkpoint, "phase"))
                            .and_then(JsonValue::as_string);
                        if next_phase != current_phase {
                            *capture.lock() = next_phase.cloned();
                        }
                    }
                    scheduler
                        .commit_state(&tx, &invocation, (*current).clone(), next)
                        .await?;
                }
                Ok(())
            },
            context,
            TransactionScope {
                conversation_id: Some(self.invocation.conversation),
                task_id: Some(self.invocation.task),
            },
        );
        let sweep = self.scheduler.clone();
        eager(async move {
            let result = operation.await;
            if sweep.inner.tree.has_drops() {
                sweep.schedule_sweep();
            }
            result?;
            if let Some(phase) = phase.lock().take() {
                invocation_context.telemetry().record_event(
                    "pi.durable.checkpoint",
                    pi_telemetry::attributes! {"phase" => phase},
                );
            }
            Ok(())
        })
    }
    fn wait(&self, id: TaskId, context: &Context) -> StorageFuture<TaskRecord> {
        self.scheduler
            .wait_for_task(id, &with_abort_signal(self.signal(), context))
    }
    fn abort_owned(&self, id: TaskId, context: &Context) -> StorageFuture<()> {
        let scheduler = self.scheduler.clone();
        let owner = self.invocation.task;
        let context = with_abort_signal(self.signal(), context);
        let lookup_context = context.clone();
        let storage = scheduler.inner.storage.clone();
        let lookup = self
            .session()
            .read_on_line(move || async move { storage.task(id, &lookup_context).await });
        eager(async move {
            let record = lookup
                .await?
                .filter(|r| r.info.owner == Some(owner))
                .ok_or_else(|| Error::message(format!("Task {id} is not owned by task {owner}")))?;
            if record.state.status() != TaskStatus::Terminal {
                scheduler.abort_with(id, &context, true).await?;
                scheduler.wait_for_task(id, &context).await?;
            }
            Ok(())
        })
    }
    fn owned(&self, context: &Context) -> StorageFuture<Vec<TaskRecord>> {
        let scheduler = self.scheduler.clone();
        let invocation = self.invocation.clone();
        let context = context.clone();
        self.session().read_on_line(move || async move {
            if invocation.ended.load(Ordering::Acquire) {
                return Err(invocation.ended_error());
            }
            context.check()?;
            Ok(scheduler
                .inner
                .tree
                .direct_owned(invocation.task)
                .into_iter()
                .map(|r| (*r).clone())
                .collect())
        })
    }
    fn observe(&self, watch: Watch) -> Result<()> {
        let mut watches = self.invocation.watches.lock();
        self.check()?;
        let id = self.invocation.next_watch.fetch_add(1, Ordering::Relaxed);
        watches.insert(id, watch.clone());
        drop(watches);
        let invocation = Arc::downgrade(&self.invocation);
        pi_chord::rt::spawn(async move {
            watch.closed().await;
            if let Some(invocation) = invocation.upgrade() {
                invocation.watches.lock().shift_remove(&id);
            }
        });
        Ok(())
    }
    fn now(&self) -> f64 {
        self.scheduler.now()
    }
    fn set_timer(&self, callback: Box<dyn FnOnce() + Send>, delay_ms: f64) -> DisarmTimer {
        if self.invocation.ended.load(Ordering::Acquire) {
            return Box::new(|| {});
        }
        let id = self.invocation.next_timer.fetch_add(1, Ordering::Relaxed);
        let invocation = self.invocation.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let did_fire = fired.clone();
        let disarm = self.scheduler.inner.timers.foreground(
            Box::new(move || {
                did_fire.store(true, Ordering::Release);
                invocation.timers.lock().shift_remove(&id);
                if !invocation.ended.load(Ordering::Acquire) {
                    callback();
                }
            }),
            delay_ms,
        );
        let mut timers = self.invocation.timers.lock();
        if fired.load(Ordering::Acquire) || self.invocation.ended.load(Ordering::Acquire) {
            drop(timers);
            disarm();
        } else {
            timers.insert(id, disarm);
        }
        let invocation = self.invocation.clone();
        Box::new(move || {
            let disarm = invocation.timers.lock().shift_remove(&id);
            if let Some(disarm) = disarm {
                disarm();
            }
        })
    }
    fn report(&self, error: Error) {
        self.scheduler
            .report(error, "task_error", Some(self.invocation.subject()), None);
    }
    fn report_host(&self, error: Error, callback: &'static str) {
        self.scheduler
            .report_host_for(error, callback, Some(self.invocation.report_task()));
    }
}
