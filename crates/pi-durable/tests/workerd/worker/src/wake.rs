//! The Rust Durable Object under test: the object-side half of upstream's
//! `test-workers/worker.ts`, compiled to wasm and driven from `worker.mjs`. The JS
//! class owns the Durable Object lifecycle; every behaviour under test runs here.
//! Mirrors `tests/worker_wake.rs`, with the object's SQL, alarm and key-value
//! storage supplied by workerd instead of in-process hosts.

use indexmap::IndexMap;
use pi_ai::{
    models::Models,
    providers::faux::{
        FauxResponseStep, RegisterFauxProviderOptions, faux_provider, faux_text_message,
    },
};
use pi_durable::storage::sqlite::cloudflare_js::open_js_durable_object_sqlite_storage;
use pi_durable::{
    harness::{runtime::TaskRuntime, scheduler::Wake, state::NextTaskState},
    storage::{StorageFuture, sqlite::SqliteStorageOptions},
    worker::WakeSession,
    worker::{DurableObjectHost, KvHost, js::JsStorage, workers_timer},
    *,
};
use std::result::Result;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn fail(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

struct Local<T>(T);
// SAFETY: Workers run one thread.
unsafe impl<T> Send for Local<T> {}
unsafe impl<T> Sync for Local<T> {}

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

/// Sleeps until `at`, then completes with the Harness time it woke at.
fn reminder() -> Task<JsonValue, JsonValue, JsonValue> {
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
                                                result: json(&format!("{woke}")),
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

/// The next scripted response; the script position is stored before the response is used.
fn step(kv: Arc<JsStorage>, hanging: Arc<tokio::sync::Notify>) -> FauxResponseStep {
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

/// One Durable Object instance's Harness, as `TestSession` in `worker.ts`.
#[wasm_bindgen]
pub struct WakeCore {
    session: WakeSession,
    hanging: Arc<tokio::sync::Notify>,
    kv: Arc<JsStorage>,
    storage: Arc<dyn Storage>,
}

#[wasm_bindgen]
impl WakeCore {
    /// Run in the object's constructor, inside `blockConcurrencyWhile`. `storage` is
    /// `ctx.storage`; `abort` is called with a reason to reset the object.
    pub async fn open(storage: JsValue, abort: js_sys::Function) -> Result<WakeCore, JsValue> {
        let host = Arc::new(JsStorage::new(storage.clone()));
        let abort = Local(abort);
        let hanging = Arc::new(tokio::sync::Notify::new());
        let faux = faux_provider(RegisterFauxProviderOptions {
            tokens_per_second: Some(f64::INFINITY),
            ..Default::default()
        });
        faux.set_responses(
            (0..8)
                .map(|_| step(host.clone(), hanging.clone()))
                .collect(),
        );
        let models = Models::default();
        models.set_provider(faux.provider);
        let registry =
            create_registry::<pi_durable::harness::tool::NativeTool, NoSection>().map_err(fail)?;
        let mut extension = Extension::new("test");
        extension.tasks.push(reminder().erased());
        registry.install(Arc::new(extension)).map_err(fail)?;
        let sql: Arc<dyn Storage> = Arc::new(
            open_js_durable_object_sqlite_storage(storage, SqliteStorageOptions::default())
                .await
                .map_err(fail)?,
        );
        let object = DurableObjectHost {
            alarms: host.clone(),
            kv: host.clone(),
            abort: Arc::new(move |reason| {
                let _ = abort.0.call1(&JsValue::NULL, &JsValue::from_str(reason));
            }),
            now: Arc::new(js_sys::Date::now),
        };
        let session = WakeSession::open(
            sql.clone(),
            HarnessOptions::new(registry, models),
            object,
            &Context::default(),
        )
        .await
        .map_err(fail)?;
        Ok(WakeCore {
            session,
            hanging,
            kv: host,
            storage: sql,
        })
    }
    pub fn now(&self) -> f64 {
        self.session.now()
    }
    pub fn advance(&self, ms: f64) {
        self.session.advance(ms);
    }
    /// Scripted model responses; `"hang"` waits for an abort.
    pub fn script(&self, responses: Vec<String>) {
        let list: Vec<String> = responses.iter().map(|text| format!("{text:?}")).collect();
        self.kv.put(
            "script",
            json(&format!(r#"{{"responses":[{}],"next":0}}"#, list.join(","))),
        );
    }
    /// `{ active, at }` as JSON.
    pub fn wake(&self) -> Result<String, JsValue> {
        Ok(wake_json(&self.session.harness().wake().map_err(fail)?))
    }
    /// The wakes `on_wake` received, as a JSON array.
    pub fn wakes(&self) -> String {
        let all: Vec<String> = self.session.wakes().iter().map(wake_json).collect();
        format!("[{}]", all.join(","))
    }
    pub async fn alarm(&self) -> Result<(), JsValue> {
        self.session.alarm().await.map_err(fail)
    }
    /// Resolves when a model request starts and hangs.
    pub async fn hanging(&self) {
        self.hanging.notified().await;
    }
    async fn root(&self) -> Result<Conversation, JsValue> {
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
            .map_err(fail)
    }
    async fn admit(&self, text: &str, request_id: &str) -> Result<Submission, JsValue> {
        let draft = SubmissionDraft::Input {
            request_id: Some(request_id.into()),
            content: text.into(),
            when_busy: None,
            entry: None,
        };
        self.root()
            .await?
            .submit(draft, &Context::default())
            .await
            .map_err(fail)
    }
    /// Arm before admitting work: a process that dies after the commit is still woken.
    pub async fn submit(&self, text: String, request_id: String) -> Result<(), JsValue> {
        self.session.arm_active().await.map_err(fail)?;
        self.admit(&text, &request_id).await.map(|_| ())
    }
    /// The assistant text a submission settled with, resubmitting its request ID.
    pub async fn answer(&self, text: String, request_id: String) -> Result<String, JsValue> {
        let context = Context::default();
        let settled = self
            .admit(&text, &request_id)
            .await?
            .wait(&context)
            .await
            .map_err(fail)?;
        let SubmissionState::InputDone { answer, .. } = settled.as_record().state.clone() else {
            return Err(fail(format!("Unexpected {:?}", settled.as_record().state)));
        };
        let entry = self
            .storage
            .entry(answer, &context)
            .await
            .map_err(fail)?
            .ok_or_else(|| fail("Missing answer entry"))?
            .entry;
        let message =
            serde_json::to_value(&entry.model.ok_or_else(|| fail("No model message"))?[0])
                .map_err(fail)?;
        Ok(message["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }
    /// Create a reminder task sleeping until `at`; its id.
    pub async fn remind(&self, at: f64) -> Result<f64, JsValue> {
        let root = self.root().await?;
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
            .map_err(fail)?;
        self.session.harness().resume().map_err(fail)?;
        Ok(id.get() as f64)
    }
    /// The task's status, or the empty string when it does not exist.
    #[wasm_bindgen(js_name = taskStatus)]
    pub async fn task_status(&self, id: f64) -> Result<String, JsValue> {
        let record = self
            .session
            .harness()
            .get_task(TaskId::from_raw(id as i64), &Context::default())
            .await
            .map_err(fail)?;
        Ok(record
            .map(|record| serde_json::to_string(&record.state.status()).unwrap_or_default())
            .unwrap_or_default())
    }
    /// Wait for a reminder to complete; the Harness time it woke at.
    #[wasm_bindgen(js_name = waitForTask)]
    pub async fn wait_for_task(&self, id: f64) -> Result<f64, JsValue> {
        let record = self
            .session
            .harness()
            .wait_for_task(TaskId::from_raw(id as i64), &Context::default())
            .await
            .map_err(fail)?;
        match record.outcome() {
            TaskOutcome::Completed { result } => Ok(result.as_f64().unwrap_or(f64::NAN)),
            other => Err(fail(format!("Task did not complete: {other:?}"))),
        }
    }
    /// Close the Harness within the caller's time limit, as an eviction would.
    pub async fn close(&self) -> Result<(), JsValue> {
        self.session
            .harness()
            .close(&Context::default())
            .await
            .map_err(fail)
    }
}

fn wake_json(wake: &Wake) -> String {
    match wake.at {
        Some(at) => format!(r#"{{"active":{},"at":{at}}}"#, wake.active),
        None => format!(r#"{{"active":{},"at":null}}"#, wake.active),
    }
}

/// Whether Workers decline a timer that only serves background work, and run a foreground one.
#[wasm_bindgen(js_name = timerPolicy)]
pub fn timer_policy() -> bool {
    let timers = workers_timer();
    timers(Box::new(|| {}), 10.0, true).is_none() && timers(Box::new(|| {}), 10.0, false).is_some()
}
