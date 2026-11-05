//! Port of packages/durable/test-workers (`storage.test.ts`, `wake.test.ts`) and
//! `test/images-wasm-module.test.ts`. The Durable Object's SQL storage, alarm and
//! key-value store are in-process hosts: SQLite behind the Cloudflare SQL shim of
//! pi-chord's tests, with eviction modelled by dropping the session and opening a
//! new one on the same hosts.
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::{
    models::Models,
    providers::faux::{
        FauxResponseStep, RegisterFauxProviderOptions, faux_provider, faux_text_message,
    },
};
use pi_chord::sqlite::{SqliteExecutor, SqliteValue, cloudflare::*};
use pi_durable::{
    harness::{runtime::TaskRuntime, scheduler::Wake, state::NextTaskState},
    storage::{StorageFuture, sqlite::*},
    testing::{StorageBackend, StorageConformanceProvider, run_storage_conformance},
    worker::*,
    *,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
#[path = "../../pi-chord/tests/support/sqlite_host.rs"]
mod sqlite_host;
use sqlite_host::Host;

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}

// ---- hosts -------------------------------------------------------------------------------------

#[derive(Default)]
struct Alarm(Mutex<Option<f64>>);
impl AlarmHost for Alarm {
    fn get_alarm(&self) -> StorageFuture<Option<f64>> {
        let value = *self.0.lock();
        Box::pin(async move { Ok(value) })
    }
    fn set_alarm(&self, at: f64) -> StorageFuture<()> {
        *self.0.lock() = Some(at);
        Box::pin(async { Ok(()) })
    }
    fn delete_alarm(&self) -> StorageFuture<()> {
        *self.0.lock() = None;
        Box::pin(async { Ok(()) })
    }
}
#[derive(Default)]
struct Kv(Mutex<HashMap<String, JsonValue>>);
impl KvHost for Kv {
    fn get(&self, key: &str) -> Option<JsonValue> {
        self.0.lock().get(key).cloned()
    }
    fn put(&self, key: &str, value: JsonValue) {
        self.0.lock().insert(key.into(), value);
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

struct NoSection;
impl pi_durable::harness::registry::RegisteredSection for NoSection {
    fn key(&self) -> &JsonString {
        panic!("no sections are registered")
    }
}
impl
    PromptSection<
        pi_durable::harness::generation::GenerationPromptInput<
            pi_durable::harness::tool::NativeTool,
            NoSection,
        >,
    > for NoSection
{
    fn render(
        &self,
        _: Arc<
            pi_durable::harness::generation::GenerationPromptInput<
                pi_durable::harness::tool::NativeTool,
                NoSection,
            >,
        >,
        _: &Context,
    ) -> StorageFuture<Option<JsonString>> {
        Box::pin(async { Ok(None) })
    }
}

/// Reminder: sleeps until `at`, then completes with the Harness time it woke at.
type Reminder = Task<JsonValue, JsonValue, JsonValue>;
fn reminder() -> Reminder {
    let phase = |name: &str| json(&format!(r#"{{"phase":"{name}"}}"#));
    define_task(TaskDefinition {
        name: "test.reminder".into(),
        version: 1,
        initial: Arc::new({
            let initial = phase("wait");
            move |_| Ok(initial.clone())
        }),
        phases: IndexMap::from([
            (
                "wait".into(),
                Arc::new(
                    move |task: RunningTask, runtime: TaskRuntime, context: Context| {
                        let at = task.record().info.input["at"].as_f64().unwrap();
                        let deliver = phase("deliver");
                        Box::pin(async move { runtime.sleep_until(at, deliver, &context).await })
                            as StorageFuture<()>
                    },
                ) as PhaseHandler<JsonValue, JsonValue, JsonValue>,
            ),
            (
                "deliver".into(),
                Arc::new(
                    |_task: RunningTask, runtime: TaskRuntime, context: Context| {
                        Box::pin(async move {
                            let woke = runtime.now().unwrap();
                            runtime
                                .commit(
                                    move |_, _| async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed {
                                                result: wire_number(woke),
                                            },
                                        }))
                                    },
                                    &context,
                                )
                                .await
                        }) as StorageFuture<()>
                    },
                ),
            ),
        ]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Aborted {
                                    reason: None,
                                    result: None,
                                },
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
}
fn wire_number(value: f64) -> JsonValue {
    json(&format!("{value}"))
}

/// The Durable Object: its SQL database, alarm, key-value store and reset counter outlive instances.
struct ObjectState {
    sql: Arc<Host>,
    alarm: Arc<Alarm>,
    kv: Arc<Kv>,
    aborts: Arc<Mutex<Vec<String>>>,
    hanging: Arc<tokio::sync::Notify>,
}
impl ObjectState {
    fn new() -> Self {
        Self {
            sql: Arc::new(Host::new()),
            alarm: Arc::default(),
            kv: Arc::default(),
            aborts: Arc::default(),
            hanging: Arc::default(),
        }
    }
    fn storage(&self) -> Arc<DurableObjectSqliteStorage> {
        Arc::new(DurableObjectSqliteStorage::new(self.sql.clone()))
    }
    fn host(&self) -> DurableObjectHost {
        let aborts = self.aborts.clone();
        DurableObjectHost {
            alarms: self.alarm.clone(),
            kv: self.kv.clone(),
            abort: Arc::new(move |reason| aborts.lock().push(reason.into())),
            now: Arc::new(now_ms),
        }
    }
    /// A new instance, as after an eviction: storage, alarm and key-value store are kept.
    async fn start(&self) -> Instance {
        let context = Context::default();
        let faux = faux_provider(RegisterFauxProviderOptions {
            tokens_per_second: Some(f64::INFINITY),
            ..Default::default()
        });
        faux.set_responses((0..8).map(|_| self.step()).collect());
        let models = Models::default();
        models.set_provider(faux.provider);
        let registry =
            create_registry::<pi_durable::harness::tool::NativeTool, NoSection>().unwrap();
        let mut extension = Extension::new("test");
        extension.tasks.push(reminder().erased());
        registry.install(Arc::new(extension)).unwrap();
        let storage =
            open_durable_object_sqlite_storage(self.storage(), SqliteStorageOptions::default())
                .await
                .unwrap();
        let session = WakeSession::open(
            Arc::new(storage),
            HarnessOptions::new(registry, models),
            self.host(),
            &context,
        )
        .await
        .unwrap();
        Instance {
            session,
            storage: self.kv.clone(),
        }
    }
    /// The next scripted response; the position is stored before the response is used.
    fn step(&self) -> FauxResponseStep {
        let kv = self.kv.clone();
        let hanging = self.hanging.clone();
        FauxResponseStep::Factory(Arc::new(move |_, options, _, _| {
            let script = kv
                .get("script")
                .unwrap_or_else(|| json(r#"{"responses":[],"next":0}"#));
            let next = script["next"].as_u64().unwrap() as usize;
            let text = script["responses"][next]
                .to_utf8()
                .map(|text| text.to_string())
                .unwrap_or_else(|| "unscripted".into());
            kv.put(
                "script",
                json(&format!(
                    r#"{{"responses":{},"next":{}}}"#,
                    script["responses"],
                    next + 1
                )),
            );
            let signal = options.signal.clone();
            let hanging = hanging.clone();
            Box::pin(async move {
                if text != "hang" {
                    return Ok(faux_text_message(text));
                }
                hanging.notify_one();
                match signal {
                    Some(signal) => signal.cancelled().await,
                    None => std::future::pending().await,
                }
                Err(anyhow::anyhow!("Request interrupted"))
            })
        }))
    }
    fn script(&self, responses: &[&str]) {
        let list: Vec<String> = responses.iter().map(|text| format!("{text:?}")).collect();
        self.kv.put(
            "script",
            json(&format!(r#"{{"responses":[{}],"next":0}}"#, list.join(","))),
        );
    }
    /// `runDurableObjectAlarm`: run the alarm now if one is set.
    async fn run_alarm(&self, instance: &Instance) -> bool {
        if self.alarm.0.lock().is_none() {
            return false;
        }
        instance.session.alarm().await.unwrap();
        true
    }
}
struct Instance {
    session: WakeSession,
    #[allow(dead_code)]
    storage: Arc<Kv>,
}
impl Instance {
    async fn root(&self) -> Conversation {
        self.session
            .harness()
            .root(
                ConversationOptions {
                    agent: Some(
                        JsonObject::try_from_value(json(
                            r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
                        ))
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                &Context::default(),
            )
            .await
            .unwrap()
    }
    async fn submit(&self, text: &str, request_id: &str) -> Submission {
        // Arm before admitting work: a process that dies after the commit is still woken.
        self.session.arm_active().await.unwrap();
        self.resubmit(text, request_id).await
    }
    async fn resubmit(&self, text: &str, request_id: &str) -> Submission {
        let draft = SubmissionDraft::Input {
            request_id: Some(request_id.into()),
            content: text.into(),
            when_busy: None,
            entry: None,
        };
        self.root()
            .await
            .submit(draft, &Context::default())
            .await
            .unwrap()
    }
    async fn remind(&self, at: f64) -> TaskId {
        let root = self.root().await;
        let conversation = root.id;
        let task = reminder();
        let id = root
            .commit(
                move |tx| async move {
                    let mut options = TaskOptions::new(TaskOwnership::Conversation);
                    options.conversation_id = Some(conversation);
                    Ok(tx
                        .create_task(&task, json(&format!(r#"{{"at":{at}}}"#)), options)
                        .await?
                        .untyped())
                },
                &Context::default(),
            )
            .await
            .unwrap();
        self.session.harness().resume().unwrap();
        id
    }
}
async fn eventually<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Condition not reached");
}

// ---- wake.test.ts ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleeps_through_an_eviction_and_resumes_the_task_from_its_alarm() {
    let object = ObjectState::new();
    let context = Context::default();
    let sleep = 600_000.0;
    let first = object.start().await;
    let at = first.session.now() + sleep;
    let id = first.remind(at).await;
    eventually(|| async {
        let record = first
            .session
            .harness()
            .get_task(id, &context)
            .await
            .unwrap();
        record.is_some_and(|record| matches!(record.state, TaskState::Sleeping { .. }))
            && *object.alarm.0.lock() == Some(at)
    })
    .await;
    let expected = Wake {
        active: false,
        at: Some(at),
    };
    assert_eq!(first.session.harness().wake().unwrap(), expected);
    assert_eq!(first.session.wakes().last(), Some(&expected));

    // The old instance is closed first: a dropped one could still hold the shared connection's transaction.
    first.session.harness().close(&context).await.unwrap();
    drop(first);
    // A new instance: the sleep is read from storage, and the alarm stays.
    let second = object.start().await;
    // Recovery of the sleeping task settles asynchronously here.
    eventually(|| async { second.session.harness().wake().unwrap() == expected }).await;
    assert_eq!(*object.alarm.0.lock(), Some(at));
    // Time passes while the object sleeps.
    second.session.advance(sleep);
    second.session.harness().close(&context).await.unwrap();
    drop(second);
    let third = object.start().await;
    assert!(object.run_alarm(&third).await);

    let settled = third
        .session
        .harness()
        .wait_for_task(id, &context)
        .await
        .unwrap();
    let TaskOutcome::Completed { result } = settled.outcome() else {
        panic!("reminder did not complete");
    };
    assert!(result.as_f64().unwrap() >= at);
    // Nothing is left to do: the alarm is gone.
    eventually(|| async { object.alarm.0.lock().is_none() }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovers_a_turn_an_eviction_cut_off_mid_stream_through_the_heartbeat_alarm() {
    let object = ObjectState::new();
    let context = Context::default();
    let first = object.start().await;
    object.script(&["hang", "recovered"]);
    first.submit("hi", "turn-1").await;
    tokio::time::timeout(Duration::from_secs(5), object.hanging.notified())
        .await
        .expect("the model request never started");
    assert!(first.session.harness().wake().unwrap().active);
    // The heartbeat alarm armed before the input was admitted.
    assert!(object.alarm.0.lock().is_some());

    // The eviction cuts the stream off: the instance and its in-flight work are dropped.
    first.session.harness().close(&context).await.ok();
    drop(first);
    let second = object.start().await;
    assert!(object.run_alarm(&second).await);

    // The same request ID finds the submission admitted before the eviction.
    let submission = second.resubmit("hi", "turn-1").await;
    let settled = tokio::time::timeout(Duration::from_secs(10), submission.wait(&context))
        .await
        .expect("turn did not settle")
        .unwrap();
    let SubmissionState::InputDone { answer, .. } = settled.as_record().state.clone() else {
        panic!("Unexpected {:?}", settled.as_record().state);
    };
    let storage =
        open_durable_object_sqlite_storage(object.storage(), SqliteStorageOptions::default())
            .await
            .unwrap();
    let entry = storage
        .entry(answer, &context)
        .await
        .unwrap()
        .unwrap()
        .entry;
    let message = serde_json::to_value(&entry.model.unwrap()[0]).unwrap();
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["content"][0]["text"], "recovered");
    eventually(|| async { object.alarm.0.lock().is_none() }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn can_evict_an_object_whose_harness_keeps_idle_documents_and_contexts_in_memory() {
    let object = ObjectState::new();
    let context = Context::default();
    let instance = object.start().await;
    object.script(&["answer"]);
    let submission = instance.submit("hi", "turn-1").await;
    tokio::time::timeout(Duration::from_secs(10), submission.wait(&context))
        .await
        .unwrap()
        .unwrap();
    // Retention timers would keep the object running; Workers decline them.
    let timers = workers_timer();
    assert!(timers(Box::new(|| {}), 10.0, true).is_none());
    let foreground = timers(Box::new(|| {}), 10.0, false).expect("foreground timers run");
    foreground();
    let evicted = tokio::time::timeout(Duration::from_secs(2), async {
        instance.session.harness().close(&context).await.unwrap();
    })
    .await;
    assert!(evicted.is_ok(), "still running");
}

#[test]
fn the_alarm_is_the_earliest_of_the_heartbeat_and_the_sleep_in_wall_clock_time() {
    let wake = |active, at| Wake { active, at };
    assert_eq!(alarm_at(&wake(false, None), 1_000.0, 0.0), None);
    assert_eq!(
        alarm_at(&wake(true, None), 1_000.0, 0.0),
        Some(1_000.0 + HEARTBEAT_MS)
    );
    assert_eq!(
        alarm_at(&wake(false, Some(5_000.0)), 1_000.0, 2_000.0),
        Some(3_000.0)
    );
    assert_eq!(
        alarm_at(&wake(true, Some(5_000.0)), 1_000.0, 0.0),
        Some(5_000.0)
    );
    assert_eq!(
        alarm_at(&wake(true, Some(500_000.0)), 1_000.0, 0.0),
        Some(1_000.0 + HEARTBEAT_MS)
    );
}

// ---- storage.test.ts ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_storage_on_durable_object_sqlite_passes_the_storage_conformance_suite() {
    let object = Arc::new(ObjectState::new());
    let suite = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: StorageConformanceProvider = Arc::new(move |body| {
        let (object, suite) = (object.clone(), suite.clone());
        Box::pin(async move {
            // Tables of their own, next to the ones the object's Harness opened.
            let prefix = format!(
                "conformance{}_",
                suite.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            let storage = open_durable_object_sqlite_storage(
                object.storage(),
                SqliteStorageOptions {
                    prefix: Some(prefix),
                },
            )
            .await?;
            body(Arc::new(storage)).await
        })
    });
    run_storage_conformance(
        "SqliteStorage on Durable Object SQLite",
        StorageBackend::Sqlite,
        &provider,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn migrates_a_version_1_schema_to_admit_sleeping_tasks_keeping_tasks_and_indexes() {
    let object = ObjectState::new();
    let database = DurableObjectSqliteDatabase::new(object.storage());
    let schema = create_sqlite_schema(Some("legacy_")).unwrap();
    let tasks = schema.tables.tasks.clone();
    let insert = |id: i64, status: &'static str| {
        let sql = format!(
            "INSERT INTO {tasks} (id, conversation_id, kind, status, abort_requested, background, record) VALUES (?, 1, 'test', ?, 0, 0, '{{}}')"
        );
        let database = database.clone();
        async move { database.run(&sql, &[id.into(), status.into()]).await }
    };
    // A database written by a version that knew schema version 1 only.
    let migrations = create_sqlite_migrations(&schema);
    apply_sqlite_migrations_for_schema(&database, &schema, Some(&migrations[..1]))
        .await
        .unwrap();
    insert(5, "waiting").await.unwrap();
    assert!(insert(6, "sleeping").await.is_err());

    open_durable_object_sqlite_storage(
        object.storage(),
        SqliteStorageOptions {
            prefix: Some("legacy_".into()),
        },
    )
    .await
    .unwrap();
    let version = database
        .get(
            &format!("SELECT version FROM {}", schema.tables.durable_schema),
            &[],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(version["version"], SqliteValue::Real(2.0));
    let status = database
        .get(&format!("SELECT status FROM {tasks} WHERE id = 5"), &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status["status"], SqliteValue::Text("waiting".into()));
    insert(6, "sleeping").await.unwrap();
    let indexes = database
        .all(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ? ORDER BY name",
            &[tasks.as_str().into()],
        )
        .await
        .unwrap();
    let names: Vec<_> = indexes
        .iter()
        .map(|row| match &row["name"] {
            SqliteValue::Text(name) => name.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let mut expected = vec![
        schema.indexes.tasks_by_abort_requested.clone(),
        schema.indexes.tasks_by_background.clone(),
        schema.indexes.tasks_by_conversation.clone(),
        schema.indexes.tasks_by_kind.clone(),
        schema.indexes.tasks_by_status.clone(),
    ];
    expected.sort();
    assert_eq!(names, expected);
}

// ---- images-wasm-module.test.ts ----------------------------------------------------------------

#[tokio::test]
async fn the_cloudflare_photon_processor_decodes_without_a_module_or_a_thread() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use pi_durable::{images::create_cloudflare_photon_images, tools::image_processor::*};
    let images = create_cloudflare_photon_images();
    // A 1x1 GIF; returned as it is only once it decodes.
    let data = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    let prepared = images
        .prepare(
            STANDARD.decode(data).unwrap(),
            "image/gif".into(),
            DEFAULT_IMAGE_LIMITS,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prepared.data, data);
    assert_eq!(prepared.mime_type, "image/gif");
}
