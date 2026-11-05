use parking_lot::Mutex;
use pi_ai::{models::Models, providers::faux::*};
use pi_durable::{
    harness::{
        agent::AGENT_DOC,
        compaction::compaction_task,
        declaration::ToolDeclaration,
        generation::*,
        live::{self, LIVE_DOC},
        prompt::PromptSection,
        registry::*,
        scheduler::*,
        settings::*,
        submissions::*,
        tool::*,
        types::{QueueModes, SubmissionDraft},
    },
    *,
};
use std::{sync::Arc, time::Duration};
#[path = "generation_runtime/collection.rs"]
mod collection;

struct Section(JsonString);
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.0
    }
}
impl PromptSection<GenerationPromptInput<NativeTool, Section>> for Section {
    fn tagged(&self) -> bool {
        false
    }
    fn render(
        &self,
        _: Arc<GenerationPromptInput<NativeTool, Section>>,
        _: &Context,
    ) -> storage::StorageFuture<Option<JsonString>> {
        Box::pin(async { Ok(Some("Be precise.".into())) })
    }
}
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).unwrap()
}
struct Opened {
    scheduler: TaskScheduler,
    submissions: Submissions,
    root: ConversationId,
    storage: Arc<MemoryStorage>,
    faux: FauxProviderHandle,
    settings: Arc<Mutex<Settings>>,
    reports: Arc<Mutex<Vec<Error>>>,
}
async fn open(
    retention: u64,
    configure: bool,
    extension: Option<Extension<NativeTool, Section>>,
) -> Opened {
    open_with_collection(retention, configure, extension, None).await
}
async fn open_with_collection(
    retention: u64,
    configure: bool,
    extension: Option<Extension<NativeTool, Section>>,
    model_collection: Option<pi_durable::harness::models::ModelCollectionReader>,
) -> Opened {
    let models = Models::default();
    let faux = faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    models.set_provider(faux.provider.clone());
    let registry = Registry::new([
        generation_task::<NativeTool, Section>().erased(),
        tool_task().erased(),
        compaction_task().erased(),
    ])
    .unwrap();
    if let Some(extension) = extension {
        registry.install(Arc::new(extension)).unwrap();
    }
    let settings = Arc::new(Mutex::new(resolve_settings(HarnessSettings {
        retry: Some(object(r#"{"baseDelayMs":1}"#)),
        ..Default::default()
    })));
    let policy = settings.clone();
    let storage = Arc::new(MemoryStorage::new());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let reported = reports.clone();
    let scheduler = TaskScheduler::open(
        storage.clone(),
        registry,
        SchedulerOptions {
            model_collection,
            on_report: Arc::new(move |error, _, _| reported.lock().push(error)),
            models: Arc::new(move || Ok(models.clone())),
            settings: Arc::new(move || Ok(policy.lock().clone())),
            settle_outcome: Arc::new(|tx, record, outcome| {
                Box::pin(live::settle_scheduler_outcome(tx, record, outcome))
            }),
            withdraw_inputs: Arc::new(|tx, conversation| {
                Box::pin(async move {
                    pi_durable::harness::inbox::withdraw_queued_inputs(&tx, conversation, "aborted")
                        .await
                })
            }),
            ..Default::default()
        },
        SessionOptions {
            document_retention_ms: retention as f64,
            conversation_created: Some(Arc::new(|tx, record| {
                Box::pin(async move { harness::create_conversation_documents(&tx, record).await })
            })),
            ..Default::default()
        },
        &Context::default(),
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            move |tx| async move {
                let root = tx.create_root_conversation().await?.id;
                if configure {
                    tx.doc(&AGENT_DOC, root).await?.update(|agent| {
                        agent.0.insert(
                            "model".into(),
                            json(r#"{"provider":"faux","modelId":"faux-1"}"#),
                        );
                        Ok(())
                    })?;
                }
                Ok(root)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let resume = scheduler.clone();
    let policy = settings.clone();
    let submissions = Submissions::new(
        scheduler.session(),
        SubmissionsOptions {
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as f64
            }),
            queue_modes: Arc::new(move || Ok(QueueModes::from(&*policy.lock()))),
            resume: Arc::new(move || resume.resume()),
            telemetry: pi_telemetry::noop_telemetry_context(),
        },
    )
    .unwrap();
    Opened {
        scheduler,
        submissions,
        root,
        storage,
        faux,
        settings,
        reports,
    }
}
async fn submit(opened: &Opened, text: &str) -> SettledSubmissionRecord {
    let submission = opened
        .submissions
        .submit(
            opened.root,
            SubmissionDraft::input(text),
            &Context::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), submission.wait(&Context::default()))
        .await
        .unwrap()
        .unwrap()
}
async fn entries(opened: &Opened) -> Vec<EntryRecord> {
    opened
        .storage
        .scan_entries(
            &EntryQuery {
                conversation_id: opened.root,
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            100,
            None,
            &Context::default(),
        )
        .await
        .unwrap()
        .items
}
async fn close(opened: Opened) {
    opened
        .scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn native_generation_prepares_answers_and_settles_input_with_usage_atomically() {
    for retention in [600_000, 0] {
        let mut extension = Extension::new("prompt");
        extension
            .sections
            .push(Arc::new(Section("preamble".into())));
        let opened = open(retention, true, Some(extension)).await;
        opened
            .faux
            .set_responses(vec![faux_text_message("Hello there").into()]);
        let result = submit(&opened, "hi").await;
        let result = pi_chord::json::to_value(&result).unwrap();
        assert!(result["status"].string_equals("done"));
        let entries = entries(&opened).await;
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.kind.to_string_lossy())
                .collect::<Vec<_>>(),
            ["pi.user", "pi.system", "pi.assistant"]
        );
        assert!(entries[0].by_task_id.is_none());
        assert_eq!(entries[1].by_task_id, entries[2].by_task_id);
        assert_eq!(
            entries[1].model.as_ref().unwrap()[0]["sections"],
            json(r#"{"preamble":"Be precise."}"#)
        );
        assert_eq!(
            entries[2].model.as_ref().unwrap()[0]["content"][0]["text"],
            json(r#""Hello there""#)
        );
        assert_eq!(
            entries[2].id.get() as f64,
            result["answer"].as_f64().unwrap()
        );
        assert_eq!(
            pi_chord::json::to_value(
                opened
                    .scheduler
                    .session()
                    .snapshot(&LIVE_DOC, opened.root, &Context::default())
                    .await
                    .unwrap()
                    .unwrap()
                    .as_ref()
            )
            .unwrap(),
            json("{}")
        );
        close(opened).await;
    }
}
#[tokio::test]
async fn unavailable_models_fail_and_do_not_send_requests() {
    for retention in [600_000, 0] {
        let opened = open(retention, false, None).await;
        let result = pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap();
        assert!(result["status"].string_equals("unanswered"));
        assert!(result["reason"].string_equals("no_model"));
        assert_eq!(opened.faux.state().call_count, 0);
        close(opened).await;
    }
}
#[tokio::test]
async fn retry_reprepares_with_live_settings_and_persists_each_attempt() {
    for retention in [600_000, 0] {
        let opened = open(retention, true, None).await;
        let settings = opened.settings.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let second = seen.clone();
        opened.settings.lock().stream = object(r#"{"timeoutMs":111}"#);
        opened.faux.set_responses(vec![
            FauxResponseStep::factory(move |_, options, _, _| {
                capture.lock().push(options.timeout_ms);
                settings.lock().stream = object(r#"{"timeoutMs":222}"#);
                faux_assistant_message(
                    vec![],
                    FauxMessageOptions {
                        stop_reason: Some(pi_ai::StopReason::Error),
                        error_message: Some("503 Service Unavailable".into()),
                        ..Default::default()
                    },
                )
            }),
            FauxResponseStep::factory(move |_, options, _, _| {
                second.lock().push(options.timeout_ms);
                faux_text_message("ok")
            }),
        ]);
        assert!(
            pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap()["status"]
                .string_equals("done")
        );
        assert_eq!(*seen.lock(), vec![Some(111), Some(222)]);
        assert_eq!(opened.faux.state().call_count, 2);
        let entries = entries(&opened).await;
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.kind.equals_str("pi.assistant"))
                .count(),
            2
        );
        close(opened).await;
    }
}
#[tokio::test]
async fn tool_round_uses_native_owned_tool_tasks_before_the_successor_answers() {
    for retention in [600_000, 0] {
        let mut extension = Extension::new("tools");
        extension.tools.push(Arc::new(NativeTool::new(
            ToolDeclaration::new("echo", "Echo", json(r#"{"type":"object","properties":{}}"#)),
            Arc::new(|_, _, _| {
                Box::pin(async {
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content("tool result".into())])),
                        ..Default::default()
                    })
                })
            }),
        )));
        let opened = open(retention, true, Some(extension)).await;
        opened.faux.set_responses(vec![
            faux_assistant_message(
                vec![faux_tool_call("echo", serde_json::Map::new(), None)],
                FauxMessageOptions {
                    stop_reason: Some(pi_ai::StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
            faux_text_message("done").into(),
        ]);
        let result = pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap();
        assert!(result["status"].string_equals("done"));
        let entries = entries(&opened).await;
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.kind.to_string_lossy())
                .collect::<Vec<_>>(),
            [
                "pi.user",
                "pi.system",
                "pi.assistant",
                "pi.tool-result",
                "pi.assistant"
            ]
        );
        assert_ne!(entries[2].by_task_id, entries[3].by_task_id);
        assert_ne!(entries[2].by_task_id, entries[4].by_task_id);
        close(opened).await;
    }
}

#[tokio::test]
async fn response_and_yield_hooks_share_the_message_and_only_first_continuation_wins() {
    for retention in [600_000, 0] {
        let saved: Arc<Mutex<Option<GenerationMessage>>> = Arc::new(Mutex::new(None));
        let captured = saved.clone();
        let prior = saved.clone();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let yields = count.clone();
        let mut extension = Extension::new("hooks");
        extension.hooks.push(HookRegistration::new(
            "pi.generation",
            Arc::new(GenerationHooks {
                after_response: Some(Arc::new(move |message, _, _| {
                    *captured.lock() = Some(message);
                    Box::pin(async { Ok(()) })
                })),
                on_yield: Some(Arc::new(move |message, _, _| {
                    assert!(Arc::ptr_eq(prior.lock().as_ref().unwrap(), &message));
                    let first = yields.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    Box::pin(async move { Ok(first.then(|| json(r#""continue""#))) })
                })),
                ..Default::default()
            }),
        ));
        let opened = open(retention, true, Some(extension)).await;
        opened.faux.set_responses(vec![
            faux_text_message("first").into(),
            faux_text_message("second").into(),
        ]);
        assert!(
            pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap()["status"]
                .string_equals("done")
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            entries(&opened)
                .await
                .iter()
                .map(|entry| entry.kind.to_string_lossy())
                .collect::<Vec<_>>(),
            ["pi.user", "pi.assistant", "pi.user", "pi.assistant"]
        );
        close(opened).await;
    }
}

#[tokio::test]
async fn sequential_round_starts_each_call_in_order_and_unanimous_termination_ends_the_run() {
    for retention in [600_000, 0] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let mut tool = NativeTool::new(
            ToolDeclaration::new("echo", "Echo", json(r#"{"type":"object","properties":{}}"#)),
            Arc::new(move |_, api, _| {
                recorded.lock().push(api.call_id.clone());
                Box::pin(async {
                    Ok(ToolExecutionResult {
                        control: Some(ToolControl {
                            terminate: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                })
            }),
        );
        tool.execution_mode = Some(ToolExecutionMode::Sequential);
        let mut extension = Extension::new("sequential");
        extension.tools.push(Arc::new(tool));
        let opened = open(retention, true, Some(extension)).await;
        opened.faux.set_responses(vec![
            faux_assistant_message(
                vec![
                    faux_tool_call("echo", serde_json::Map::new(), Some("a")),
                    faux_tool_call("echo", serde_json::Map::new(), Some("b")),
                ],
                FauxMessageOptions {
                    stop_reason: Some(pi_ai::StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
            faux_text_message("never").into(),
        ]);
        assert!(
            pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap()["status"]
                .string_equals("done")
        );
        assert_eq!(
            *calls.lock(),
            vec![JsonString::from("a"), JsonString::from("b")]
        );
        assert_eq!(opened.faux.state().call_count, 1);
        assert_eq!(
            entries(&opened)
                .await
                .iter()
                .map(|entry| entry.kind.to_string_lossy())
                .collect::<Vec<_>>(),
            [
                "pi.user",
                "pi.system",
                "pi.assistant",
                "pi.tool-result",
                "pi.tool-result"
            ]
        );
        close(opened).await;
    }
}
