//! Public orchestration over one native Session. Hosts supply capabilities;
//! creation, admission, recovery and observation stay in the shared services.
use super::{
    agent::{self, AGENT_DOC, AgentView},
    compaction::{CompactionResult, compact_conversation},
    conversation::{ConversationResolver, ConversationServices},
    events::{self, AgentEventStream},
    generation::{GenerationPromptInput, generation_task},
    inspect::HarnessInspection,
    models::{ModelCollectionReader, ModelsReader, read_models},
    prompt::PromptSection,
    reader::{RegistryReader, RegistryView, read_registry},
    registry::{RegisteredTool, Registry},
    scheduler::{AbortResult, SchedulerOptions, TaskReport, TaskScheduler, Wake},
    settings::{HarnessSettings, SettingsReader, resolve_settings},
    settled::SettledTask,
    submissions::{Submission, Submissions, SubmissionsOptions},
    types::{QueueModes, SubmissionDraft},
    usage::{USAGE_DOC, UsageState, add_usage_state},
    view::ConversationViews,
};
use crate::{
    storage::{StorageFuture, line::eager_on, ready},
    wire, *,
};
use pi_ai::{
    models::Models,
    utils::identity::{ClientIdentity, merge_client_identity, pi_attribution},
};
use pi_telemetry::{AttributeValue, ContextRef, SpanAttributes};
use std::{
    future::Future,
    ops::Deref,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

pub use super::view::ConversationViewWatch as ConversationWatch;

pub type HarnessSettingsReader = Arc<dyn Fn() -> Result<HarnessSettings> + Send + Sync>;
pub type HostClock = Arc<dyn Fn() -> Result<f64> + Send + Sync>;
pub type HarnessReport = Arc<dyn Fn(Error, TaskReport) + Send + Sync>;
pub type ConversationInit =
    Arc<dyn Fn(Transaction, ConversationId) -> StorageFuture<()> + Send + Sync>;
pub type ConversationCreated =
    Arc<dyn Fn(Transaction, ConversationRecord) -> StorageFuture<()> + Send + Sync>;
pub type EnvironmentFactory = Arc<
    dyn Fn(EnvTarget, Context) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct EnvTarget {
    pub conversation_id: ConversationId,
    pub cwd: Option<JsonString>,
    pub read: Session,
}

#[derive(Clone)]
pub struct HarnessOptions {
    pub registry: Arc<dyn RegistryReader>,
    pub models: ModelsReader,
    /// An identity-bound collection supplied by an embedding host. Without it,
    /// the native SDK collection uses `models` with this Harness's identity.
    pub model_collection: Option<ModelCollectionReader>,
    pub identity: Option<ClientIdentity>,
    pub settings: HarnessSettingsReader,
    pub env: Option<EnvironmentFactory>,
    pub conversation_created: Option<ConversationCreated>,
    pub now: Option<HostClock>,
    pub set_timer: Option<SetTimer>,
    pub on_wake: Option<Arc<dyn Fn(Wake) -> Result<()> + Send + Sync>>,
    /// See [`SchedulerOptions::step_gate`](super::scheduler::SchedulerOptions::step_gate).
    pub step_gate: Option<super::scheduler::StepGate>,
    pub on_report: Option<HarnessReport>,
    pub telemetry: ContextRef,
    pub read_only: bool,
    /// The Session default, also used by source-conformance hosts that run
    /// with immediate document eviction. An explicit setting overrides it. NaN, the default,
    /// means the process-wide default (`set_default_document_retention`) as it is when the
    /// Harness opens; an explicit value of 0 or more fixes it.
    pub default_document_retention_ms: f64,
    pub document_retention_reader: Option<DocumentRetentionReader>,
}
impl HarnessOptions {
    pub fn new(registry: impl RegistryReader, models: Models) -> Self {
        Self {
            registry: Arc::new(registry),
            models: Arc::new(move || Ok(models.clone())),
            model_collection: None,
            identity: None,
            settings: Arc::new(|| Ok(HarnessSettings::default())),
            env: None,
            conversation_created: None,
            now: None,
            set_timer: None,
            on_wake: None,
            step_gate: None,
            on_report: None,
            telemetry: pi_telemetry::noop_telemetry_context(),
            read_only: false,
            default_document_retention_ms: f64::NAN,
            document_retention_reader: None,
        }
    }
    pub fn with_settings(mut self, settings: HarnessSettings) -> Self {
        self.settings = Arc::new(move || Ok(settings.clone()));
        self
    }
}

#[derive(Clone, Default)]
pub struct ConversationOptions {
    pub agent: Option<JsonObject>,
    pub init: Option<ConversationInit>,
}
#[derive(Clone)]
pub struct ConversationCreateOptions {
    pub ownership: ConversationOwnership,
    pub agent: Option<JsonObject>,
    pub init: Option<ConversationInit>,
}
impl ConversationCreateOptions {
    pub fn new(ownership: ConversationOwnership) -> Self {
        Self {
            ownership,
            agent: None,
            init: None,
        }
    }
    fn options(&self) -> ConversationOptions {
        ConversationOptions {
            agent: self.agent.clone(),
            init: self.init.clone(),
        }
    }
}
#[derive(Clone, Default)]
pub struct ConversationEntryQuery {
    pub min_entry_id: Option<EntryId>,
    pub max_entry_id: Option<EntryId>,
    pub order: Option<ScanOrder>,
}

type Report = Arc<dyn Fn(Error, TaskReport, Option<Context>) + Send + Sync>;
struct Inner {
    scheduler: TaskScheduler,
    submissions: Submissions,
    views: ConversationViews,
    registry: Arc<dyn RegistryReader>,
    settings: SettingsReader,
    env: Option<EnvironmentFactory>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    report: Report,
}
#[derive(Clone)]
pub struct Harness {
    inner: Arc<Inner>,
}
impl Deref for Harness {
    type Target = Session;
    fn deref(&self) -> &Session {
        self.inner.scheduler.session_ref()
    }
}
#[derive(Clone)]
pub struct Conversation {
    pub id: ConversationId,
    host: Harness,
}
enum CreateTarget {
    Root,
    Independent(ConversationOwnership),
    Fork {
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    },
}

pub fn default_harness_identity() -> ClientIdentity {
    ClientIdentity {
        attribution: Some(Some(pi_attribution())),
        ..Default::default()
    }
}
pub fn create_registry<T, S>() -> Result<Registry<T, S>>
where
    T: RegisteredTool + super::declaration::DeclaredTool,
    S: PromptSection<GenerationPromptInput<T, S>>,
{
    Registry::new([
        generation_task::<T, S>().erased(),
        super::tool::tool_task().erased(),
        super::compaction::compaction_task().erased(),
    ])
}
fn wall_now() -> f64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}
pub fn safe_now(
    now: Option<HostClock>,
    report: HarnessReport,
) -> Arc<dyn Fn() -> f64 + Send + Sync> {
    let reported = AtomicBool::new(false);
    Arc::new(move || {
        let Some(now) = &now else { return wall_now() };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| now()))
            .unwrap_or_else(|p| Err(crate::session::panic_error(p)))
        {
            Ok(value) => value,
            Err(error) => {
                if !reported.swap(true, Ordering::AcqRel) {
                    report(error, report_info("host_error", Some("now")));
                }
                wall_now()
            }
        }
    })
}
fn report_info(kind: &'static str, callback: Option<&'static str>) -> TaskReport {
    TaskReport {
        kind,
        callback,
        task: None,
        reason: None,
        extension: None,
        conversation: None,
    }
}
fn safe_report(host: Option<HarnessReport>, telemetry: ContextRef) -> Report {
    let metrics = telemetry
        .metrics()
        .unwrap_or_else(pi_telemetry::noop_telemetry_metrics);
    let counter =
        pi_telemetry::create_typed_metrics(metrics, &[&super::telemetry::DURABLE_TELEMETRY_SCHEMA])
            .counter("pi.durable.reports");
    Arc::new(move |error, info, context| {
        if let Some(host) = &host {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                host(error, info.clone())
            }));
        }
        let mut attributes =
            SpanAttributes::from([("type".into(), AttributeValue::String(info.kind.into()))]);
        if matches!(info.kind, "task_faulted" | "task_outdated")
            && let Some(reason) = info.reason
        {
            attributes.insert("reason".into(), AttributeValue::String(reason.into()));
        }
        if matches!(
            info.kind,
            "task_faulted"
                | "task_outdated"
                | "task_parked"
                | "migration_failed"
                | "task_error"
                | "extension_error"
        ) && let Some(extension) = info.extension
        {
            attributes.insert("extension".into(), AttributeValue::String(extension.into()));
        }
        if let Some(task) = info.task {
            attributes.insert(
                "task.id".into(),
                AttributeValue::Number(task.id.get() as f64),
            );
        }
        counter.add(
            1.0,
            SpanAttributes::from([("type".into(), AttributeValue::String(info.kind.into()))]),
        );
        if let Some(context) = context {
            context
                .telemetry()
                .record_event("pi.durable.report", attributes);
        }
    })
}

fn build_environment(
    session: &Session,
    build: Option<EnvironmentFactory>,
    id: ConversationId,
    context: &Context,
) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>> {
    let Some(build) = build else {
        return ready(Ok(None));
    };
    let state = session.snapshot(&AGENT_DOC, id, context);
    let context = context.clone();
    let read = session.clone();
    eager_on(session, async move {
        let cwd = state.await?.and_then(|state| {
            state
                .0
                .get(&JsonString::from("cwd"))
                .and_then(JsonValue::as_string)
                .cloned()
        });
        build(
            EnvTarget {
                conversation_id: id,
                cwd,
                read,
            },
            context,
        )
        .await
    })
}

impl Harness {
    pub async fn open(
        storage: Arc<dyn Storage>,
        options: HarnessOptions,
        context: &Context,
    ) -> Result<Self> {
        context.check()?;
        let snapshot = read_registry(options.registry.as_ref())?;
        let missing = ["pi.generation", "pi.tool", "pi.compaction"]
            .into_iter()
            .filter(|name| snapshot.task(&(*name).into()).is_none())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(Error::message(format!(
                "Registry lacks built-in tasks {}; create it with createRegistry()",
                missing.join(", ")
            )));
        }
        let report = safe_report(options.on_report, options.telemetry.clone());
        let clock_report = report.clone();
        let now = safe_now(
            options.now,
            Arc::new(move |error, info| clock_report(error, info, None)),
        );
        let settings = options.settings.clone();
        let explicit_retention = (!options.default_document_retention_ms.is_nan()
            && options.default_document_retention_ms >= 0.0)
            .then_some(options.default_document_retention_ms);
        let fallback_retention =
            explicit_retention.unwrap_or_else(crate::session::default_document_retention);
        let resolved: SettingsReader = Arc::new(move || {
            let input = settings()?;
            let retention = input
                .document_retention_ms
                .filter(|value| !value.is_nan() && *value >= 0.0)
                .unwrap_or(fallback_retention);
            let mut settings = resolve_settings(input);
            settings.document_retention_ms = retention;
            Ok(settings)
        });
        let identity =
            merge_client_identity(&[Some(&default_harness_identity()), options.identity.as_ref()])
                .unwrap_or_default();
        let models = options.models;
        let models: ModelsReader =
            Arc::new(move || Ok(read_models(&models)?.with_client_identity(identity.clone())));
        let environment_session = Arc::new(OnceLock::<Session>::new());
        let conversation_services = Arc::new(OnceLock::<Arc<ConversationServices>>::new());
        let environments = environment_session.clone();
        let conversations = conversation_services.clone();
        let build = options.env.clone();
        let conversation: ConversationResolver =
            Arc::new(move |id, binding, context| match conversations.get() {
                Some(services) => services.acquire(id, binding, &context),
                None => ready(Err(Error::message("Conversation services are unavailable"))),
            });
        let scheduler_report = report.clone();
        let session_report = report.clone();
        let timer_report = report.clone();
        let created = options.conversation_created;
        let retention = options.settings;
        let scheduler = TaskScheduler::open(
            storage,
            options.registry.clone(),
            SchedulerOptions {
                settings: resolved.clone(),
                models,
                model_collection: options.model_collection,
                env: Arc::new(move |id, context| match environments.get() {
                    Some(session) => build_environment(session, build.clone(), id, &context),
                    None => ready(Err(Error::message("Harness is unavailable"))),
                }),
                conversation: Some(conversation),
                on_wake: options.on_wake,
                step_gate: options.step_gate,
                telemetry: options.telemetry.clone(),
                on_report: Arc::new(move |error, info, context| {
                    scheduler_report(error, info, context)
                }),
                settle_outcome: Arc::new(|tx, record, outcome| {
                    Box::pin(super::live::settle_scheduler_outcome(tx, record, outcome))
                }),
                withdraw_inputs: Arc::new(|tx, id| {
                    Box::pin(async move {
                        super::inbox::withdraw_queued_inputs(&tx, id, "aborted").await
                    })
                }),
                ..Default::default()
            },
            SessionOptions {
                now: now.clone(),
                set_timer: options.set_timer,
                read_only: options.read_only,
                document_retention_ms: fallback_retention,
                document_retention_reader: options
                    .document_retention_reader
                    .or_else(|| Some(Arc::new(move || Ok(retention()?.document_retention_ms)))),
                // The scheduler's foreground timer wrapper uses this legacy
                // callback; Session events use the typed callback below.
                report: Arc::new(move |error| {
                    timer_report(error, report_info("host_error", Some("setTimer")), None);
                }),
                report_event: Some(Arc::new(move |error, kind, context| {
                    let info = match kind {
                        SessionReportKind::Listener => report_info("listener_error", None),
                        SessionReportKind::Failed => report_info("session_failed", None),
                        SessionReportKind::Timer => report_info("host_error", Some("setTimer")),
                    };
                    session_report(error, info, context);
                })),
                conversation_created: Some(Arc::new(move |tx, record| {
                    let hook = created.clone();
                    Box::pin(async move {
                        super::create_conversation_documents(&tx, record.clone()).await?;
                        if let Some(hook) = hook {
                            hook(tx, record).await?;
                        }
                        Ok(())
                    })
                })),
                ..Default::default()
            },
            context,
        )
        .await?;
        let _ = environment_session.set(scheduler.session());
        let result = (|| {
            let resume = scheduler.downgrade();
            let session = scheduler.session();
            let modes = resolved.clone();
            let submissions = Submissions::new(
                scheduler.session(),
                SubmissionsOptions {
                    now: now.clone(),
                    queue_modes: Arc::new(move || {
                        Ok(QueueModes::from(&super::settings::read_settings(&modes)?))
                    }),
                    resume: Arc::new(move || {
                        resume
                            .upgrade()
                            .ok_or_else(|| super::util::closed_error(&session))?
                            .resume()
                    }),
                    telemetry: options.telemetry,
                },
            )?;
            let views = ConversationViews::new(scheduler.session())?;
            let conversations = ConversationServices::for_scheduler(
                scheduler.session(),
                submissions.clone(),
                &scheduler,
            );
            let _ = conversation_services.set(conversations);
            let inner = Arc::new(Inner {
                scheduler: scheduler.clone(),
                submissions,
                views,
                registry: options.registry,
                settings: resolved,
                env: options.env,
                now,
                report,
            });
            Ok(Self { inner })
        })();
        if result.is_err() {
            let _ = scheduler
                .session()
                .close(&pi_chord::context::without_abort_signal(context))
                .await;
        }
        result
    }
    pub fn scheduler(&self) -> &TaskScheduler {
        &self.inner.scheduler
    }
    pub fn submissions(&self) -> &Submissions {
        &self.inner.submissions
    }
    pub fn views(&self) -> &ConversationViews {
        &self.inner.views
    }
    pub fn resume(&self) -> Result<()> {
        self.usable()?;
        self.inner.scheduler.resume()
    }
    pub fn wake(&self) -> Result<Wake> {
        self.usable()?;
        Ok(self.inner.scheduler.wake())
    }
    pub fn report(&self, error: Error, info: TaskReport, context: Option<Context>) {
        (self.inner.report)(error, info, context);
    }
    pub fn root(
        &self,
        options: ConversationOptions,
        context: &Context,
    ) -> StorageFuture<Conversation> {
        self.create(CreateTarget::Root, options, context)
    }
    pub fn create_conversation(
        &self,
        options: ConversationCreateOptions,
        context: &Context,
    ) -> StorageFuture<Conversation> {
        self.create(
            CreateTarget::Independent(options.ownership),
            options.options(),
            context,
        )
    }
    fn create(
        &self,
        target: CreateTarget,
        options: ConversationOptions,
        context: &Context,
    ) -> StorageFuture<Conversation> {
        let operation = self.commit(
            move |tx| async move {
                if matches!(target, CreateTarget::Root)
                    && tx.conversation(ROOT_CONVERSATION_ID).await?.is_some()
                {
                    return Ok(ROOT_CONVERSATION_ID);
                }
                let record = match target {
                    CreateTarget::Root => tx.create_root_conversation().await?,
                    CreateTarget::Independent(ownership) => {
                        tx.create_conversation(ownership).await?
                    }
                    CreateTarget::Fork {
                        parent,
                        at,
                        ownership,
                    } => tx.fork_conversation(parent, at, ownership).await?,
                };
                if let Some(agent) = options.agent {
                    agent::configure(&tx, record.id, agent).await?;
                }
                if let Some(init) = options.init {
                    init(tx, record.id).await?;
                }
                Ok(record.id)
            },
            context,
        );
        let host = self.clone();
        eager_on(self, async move {
            Ok(Conversation {
                id: operation.await?,
                host,
            })
        })
    }
    pub fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<Conversation>> {
        let storage = self.guarded_storage();
        let context = context.clone();
        let host = self.clone();
        self.read_on_line(move || async move {
            Ok(storage
                .conversation(id, &context)
                .await?
                .map(|record| Conversation {
                    id: record.id,
                    host,
                }))
        })
    }
    pub fn get_task(&self, id: TaskId, context: &Context) -> StorageFuture<Option<TaskRecord>> {
        let storage = self.guarded_storage();
        let context = context.clone();
        self.read_on_line(move || async move { storage.task(id, &context).await })
    }
    pub fn inspect(&self, context: &Context) -> StorageFuture<HarnessInspection> {
        self.inner.scheduler.inspect(context)
    }
    pub fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageFuture<Option<Submission>> {
        let operation = self.inner.submissions.get(id, context);
        let host = self.clone();
        eager_on(self, async move {
            Ok(operation
                .await?
                .map(|submission| submission.retaining(host)))
        })
    }
    pub fn abort_submission(
        &self,
        id: SubmissionId,
        conversation: Option<ConversationId>,
        context: &Context,
    ) -> StorageFuture<Option<super::submissions::SubmissionAbort>> {
        self.inner.submissions.abort(id, context, conversation)
    }
    pub fn abort_task(&self, id: TaskId, context: &Context) -> StorageFuture<AbortResult> {
        self.inner.scheduler.abort(id, context)
    }
    pub fn retry_task(&self, id: TaskId, context: &Context) -> StorageFuture<()> {
        if let Err(error) = self.usable() {
            return ready(Err(error));
        }
        self.inner.scheduler.retry(id, context)
    }
    /// Wait until the task is terminal; the receipt carries its outcome.
    pub fn wait_for_task(&self, id: TaskId, context: &Context) -> StorageFuture<SettledTask> {
        if let Err(error) = self.inner.scheduler.resume() {
            return ready(Err(error));
        }
        let waited = self.inner.scheduler.wait_for_task(id, context);
        Box::pin(async move { SettledTask::try_from(waited.await?) })
    }
    pub fn wait_for_idle(&self, context: &Context) -> StorageFuture<()> {
        if let Err(error) = self.inner.scheduler.resume() {
            return ready(Err(error));
        }
        self.inner.scheduler.wait_for_idle(None, context)
    }
    pub fn watch_task_graph(
        &self,
        context: &Context,
    ) -> StorageFuture<super::task_graph::TaskGraphWatch> {
        self.inner.scheduler.watch_task_graph(context)
    }
    pub fn watch_events(
        &self,
        conversation: ConversationId,
        context: &Context,
    ) -> StorageFuture<AgentEventStream> {
        events::watch_events(&self.inner.views, conversation, context)
    }
    pub fn usage(&self, context: &Context) -> StorageFuture<UsageState> {
        let storage = self.guarded_storage();
        let context = context.clone();
        let host = self.clone();
        let scan_context = context.clone();
        let conversations = self.read_on_line(move || async move {
            let mut records = Vec::new();
            let mut cursor = None;
            loop {
                let page = storage
                    .scan_conversations(
                        &ConversationQuery::default(),
                        256,
                        cursor.as_ref(),
                        &scan_context,
                    )
                    .await?;
                records.extend(page.items);
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            Ok(records)
        });
        eager_on(self, async move {
            let mut total = UsageState::default();
            for record in conversations.await? {
                if let Some(state) = host.snapshot(&USAGE_DOC, record.id, &context).await? {
                    add_usage_state(&mut total, &state)?;
                }
            }
            Ok(total)
        })
    }
    pub fn resolve_agent(
        &self,
        id: ConversationId,
        snapshot: Option<RegistryView>,
        context: &Context,
    ) -> StorageFuture<AgentView> {
        let snapshot =
            match snapshot.map_or_else(|| read_registry(self.inner.registry.as_ref()), Ok) {
                Ok(view) => view,
                Err(error) => return ready(Err(error)),
            };
        let report = self.inner.report.clone();
        let call_context = context.clone();
        let resolution = agent::resolve_agent_in(agent::AgentRequest {
            session: self.inner.scheduler.session(),
            conversation: id,
            snapshot,
            settings: self.inner.settings.clone(),
            context: context.clone(),
            report: Arc::new(move |error, extension| {
                let mut info = report_info("extension_error", None);
                info.extension = Some(extension);
                info.conversation = Some(id);
                report(error, info, Some(call_context.clone()));
            }),
        });
        eager_on(self, resolution)
    }
    pub fn build_env(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<Option<Arc<dyn crate::env::ExecutionEnv>>> {
        build_environment(self, self.inner.env.clone(), id, context)
    }
}
impl Conversation {
    pub fn agent(&self, context: &Context) -> StorageFuture<AgentView> {
        self.host.resolve_agent(self.id, None, context)
    }
    pub fn configure(&self, change: JsonObject, context: &Context) -> StorageFuture<()> {
        let id = self.id;
        self.host.commit(
            move |tx| async move { agent::configure(&tx, id, change).await },
            context,
        )
    }
    pub fn submit(&self, draft: SubmissionDraft, context: &Context) -> StorageFuture<Submission> {
        let operation = self.host.inner.submissions.submit(self.id, draft, context);
        let host = self.host.clone();
        eager_on(
            &self.host,
            async move { Ok(operation.await?.retaining(host)) },
        )
    }
    pub fn compact(
        &self,
        instructions: Option<JsonString>,
        context: &Context,
    ) -> StorageFuture<TypedTaskId<CompactionResult>> {
        compact_conversation(&self.host.inner.scheduler, self.id, instructions, context)
    }
    pub fn reset(&self, handoff: Option<JsonString>, context: &Context) -> StorageFuture<()> {
        let mut entry = EntryDraft::new(RESET_ENTRY.kind().clone());
        entry.head = Some(EntryHead::ThisEntry);
        if let Some(text) = handoff {
            let mut message = wire::object();
            wire::put(&mut message, "role", "user");
            wire::put(&mut message, "content", &text);
            wire::put(&mut message, "timestamp", &(self.host.inner.now)());
            entry.model = Some(vec![message]);
        }
        let admission = self.submit(SubmissionDraft::write(entry), context);
        eager_on(&self.host, async move {
            admission.await?;
            Ok(())
        })
    }
    pub fn commit<T, F, Fut>(&self, change: F, context: &Context) -> StorageFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(Transaction) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.host.commit_scoped(
            change,
            context,
            TransactionScope {
                conversation_id: Some(self.id),
                task_id: None,
            },
        )
    }
    pub fn context(
        &self,
        at: Option<EntryId>,
        context: &Context,
    ) -> StorageFuture<super::context::ContextView> {
        super::context::read_context(
            &self.host,
            self.host.guarded_storage(),
            self.id,
            context,
            at,
        )
    }
    pub fn entries(
        &self,
        query: ConversationEntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        let query = EntryQuery {
            conversation_id: self.id,
            min_entry_id: query.min_entry_id,
            max_entry_id: query.max_entry_id,
            order: query.order,
        };
        let storage = self.host.guarded_storage();
        let context = context.clone();
        self.host.read_on_line(move || async move {
            storage
                .scan_entries(&query, limit, cursor.as_ref(), &context)
                .await
        })
    }
    pub fn fork(
        &self,
        at: EntryId,
        options: ConversationCreateOptions,
        context: &Context,
    ) -> StorageFuture<Conversation> {
        self.host.create(
            CreateTarget::Fork {
                parent: self.id,
                at,
                ownership: options.ownership,
            },
            options.options(),
            context,
        )
    }
    pub fn abort(&self, background: bool, context: &Context) -> StorageFuture<()> {
        if let Err(error) = self.host.inner.scheduler.resume() {
            return ready(Err(error));
        }
        self.host
            .inner
            .scheduler
            .abort_conversation(self.id, background, context)
    }
    pub fn wait_for_idle(&self, context: &Context) -> StorageFuture<()> {
        if let Err(error) = self.host.inner.scheduler.resume() {
            return ready(Err(error));
        }
        self.host
            .inner
            .scheduler
            .wait_for_idle(Some(self.id), context)
    }
    pub fn watch(&self, context: &Context) -> StorageFuture<ConversationWatch> {
        self.host.inner.views.watch(self.id, context)
    }
}

#[cfg(test)]
#[path = "report_telemetry_tests.rs"]
mod report_telemetry_tests;
