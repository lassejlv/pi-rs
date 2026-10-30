use parking_lot::Mutex;
use pi_ai::{models::Models, providers::faux::*};
use pi_durable::{
    harness::{
        agent::AGENT_DOC,
        compaction::*,
        live::{CompactionReason, LIVE_DOC},
        registry::*,
        scheduler::*,
        settings::*,
    },
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
#[path = "compaction_runtime/collection.rs"]
mod collection;
struct Tool(JsonString);
impl RegisteredTool for Tool {
    fn name(&self) -> &JsonString {
        &self.0
    }
    fn callers(&self) -> Option<&[ToolCaller]> {
        None
    }
    fn deferred(&self) -> bool {
        false
    }
}
struct Section(JsonString);
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.0
    }
}
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).unwrap()
}
fn models() -> (Models, FauxProviderHandle) {
    let models = Models::default();
    let faux = faux_provider(RegisterFauxProviderOptions {
        provider: Some("compaction-test".into()),
        models: vec![FauxModelDefinition {
            id: "fixture".into(),
            max_tokens: Some(8),
            ..Default::default()
        }],
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    models.set_provider(faux.provider.clone());
    (models, faux)
}
fn settings() -> Settings {
    resolve_settings(HarnessSettings {
        compaction: Some(object(r#"{"keepRecentTokens":5,"reserveTokens":1000}"#)),
        retry: Some(object(r#"{"baseDelayMs":0}"#)),
        stream: Some(object(
            r#"{"timeoutMs":123,"metadata":{"pinned":true},"deferred":true}"#,
        )),
        ..Default::default()
    })
}
fn owner_task() -> Task<JsonValue, JsonValue, JsonValue> {
    define_task(TaskDefinition {
        name: "compaction-owner".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"start"}"#))),
        phases: indexmap::IndexMap::from_iter([
            (
                "start".into(),
                Arc::new(
                    |_, runtime: TaskRuntime, context| -> pi_durable::storage::StorageFuture<()> {
                        Box::pin(async move {
                            let conversation = runtime.conversation_id();
                            let owner = runtime.task_id().untyped();
                            runtime
                                .commit(
                                    move |tx, _| async move {
                                        let child = create_compaction(
                                            &tx,
                                            conversation,
                                            CompactionInput {
                                                reason: CompactionReason::Threshold,
                                                instructions: None,
                                            },
                                            Some(owner),
                                        )
                                        .await?;
                                        let mut checkpoint = json(r#"{"phase":"finish"}"#);
                                        checkpoint.as_object_mut().unwrap().insert(
                                            "child".into(),
                                            pi_chord::json::to_value(&child).unwrap(),
                                        );
                                        Ok(Some(NextTaskState::Waiting {
                                            checkpoint,
                                            on: vec![child.untyped()],
                                            policy: JoinPolicy::AllSettled,
                                        }))
                                    },
                                    &context,
                                )
                                .await
                        })
                    },
                ) as PhaseHandler<_, _, _>,
            ),
            (
                "finish".into(),
                Arc::new(
                    |task: RunningTask,
                     runtime: TaskRuntime,
                     context|
                     -> pi_durable::storage::StorageFuture<()> {
                        Box::pin(async move {
                            let child: TypedTaskId<CompactionResult> =
                                serde_json::from_str(&task.checkpoint()["child"].to_string())
                                    .unwrap();
                            let result = pi_chord::json::to_value(
                                &runtime.outcomes(&[child], &context).await?,
                            )
                            .unwrap();
                            runtime
                                .commit(
                                    move |_, _| async move {
                                        Ok(Some(NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed { result },
                                        }))
                                    },
                                    &context,
                                )
                                .await
                        })
                    },
                ) as PhaseHandler<_, _, _>,
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
async fn open(
    retention: f64,
    models: Arc<Mutex<Models>>,
    settings: Arc<Mutex<Settings>>,
    hooks: Vec<Arc<CompactionHooks>>,
    configure: bool,
) -> (TaskScheduler, ConversationId, Arc<MemoryStorage>) {
    open_with_collection(retention, models, settings, hooks, configure, None).await
}
async fn open_with_collection(
    retention: f64,
    models: Arc<Mutex<Models>>,
    settings: Arc<Mutex<Settings>>,
    hooks: Vec<Arc<CompactionHooks>>,
    configure: bool,
    model_collection: Option<pi_durable::harness::models::ModelCollectionReader>,
) -> (TaskScheduler, ConversationId, Arc<MemoryStorage>) {
    let registry =
        Registry::<Tool, Section>::new([COMPACTION_TASK.erased(), owner_task().erased()]).unwrap();
    for (index, hook) in hooks.into_iter().enumerate() {
        let mut extension = Extension::new(format!("hook-{index}"));
        extension
            .hooks
            .push(HookRegistration::new("pi.compaction", hook));
        registry.install(Arc::new(extension)).unwrap();
    }
    let storage = Arc::new(MemoryStorage::default());
    let scheduler = TaskScheduler::open(
        storage.clone(),
        registry,
        SchedulerOptions {
            model_collection,
            models: Arc::new(move || Ok(models.lock().clone())),
            settings: Arc::new(move || Ok(settings.lock().clone())),
            settle_outcome: Arc::new(|tx, record, outcome| {
                Box::pin(pi_durable::harness::live::settle_scheduler_outcome(
                    tx, record, outcome,
                ))
            }),
            ..Default::default()
        },
        SessionOptions {
            now: Arc::new(|| 42.0),
            document_retention_ms: retention,
            conversation_created: Some(Arc::new(|tx, record| {
                Box::pin(async move {
                    pi_durable::harness::create_conversation_documents(&tx, record).await
                })
            })),
            ..Default::default()
        },
        &Context::default(),
    )
    .await
    .unwrap();
    let conversation = scheduler
        .session()
        .commit(
            move |tx| async move {
                let root = tx.create_root_conversation().await?;
                if configure {
                    tx.doc(&AGENT_DOC, root.id).await?.update(|agent| {
                        agent.0.insert(
                            "model".into(),
                            json(r#"{"provider":"compaction-test","modelId":"fixture"}"#),
                        );
                        Ok(())
                    })?;
                }
                Ok(root.id)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    (scheduler, conversation, storage)
}
async fn seed(scheduler: &TaskScheduler, conversation: ConversationId) {
    scheduler
        .session()
        .commit(
            move |tx| async move {
                for text in [
                    "old user",
                    "old assistant",
                    "recent user",
                    "recent assistant",
                ] {
                    let mut entry = EntryDraft::new("fixture");
                    entry.model = Some(vec![json(&format!(
                        r#"{{"role":"user","content":"{}","timestamp":1}}"#,
                        text.repeat(100)
                    ))]);
                    tx.append_entry(conversation, entry).await?;
                }
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
}
async fn start(scheduler: &TaskScheduler, conversation: ConversationId) -> TaskId {
    scheduler
        .session()
        .commit(
            move |tx| async move {
                Ok(create_compaction(
                    &tx,
                    conversation,
                    CompactionInput {
                        reason: CompactionReason::Manual,
                        instructions: Some("focus on exact paths".into()),
                    },
                    None,
                )
                .await?
                .untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap()
}
async fn settled(scheduler: &TaskScheduler, id: TaskId) -> TaskRecord {
    scheduler.resume().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        scheduler.wait_for_task(id, &Context::default()),
    )
    .await
    .unwrap()
    .unwrap()
}
fn completed(record: &TaskRecord) -> CompactionResult {
    let TaskState::Terminal {
        outcome: TaskOutcome::Completed { result },
    } = &record.state
    else {
        panic!("Unexpected outcome: {:?}", record.state);
    };
    serde_json::from_str(&result.to_string()).unwrap()
}

#[tokio::test]
async fn native_summary_places_through_submission_with_usage_and_pinned_options() {
    for retention in [600000.0, 0.0] {
        let (models, faux) = models();
        let seen = Arc::new(AtomicUsize::new(0));
        let capture = seen.clone();
        faux.set_responses(vec![FauxResponseStep::factory(
            move |context, options, _, model| {
                assert_eq!(model.id, "fixture");
                assert_eq!(options.max_tokens, Some(8));
                assert_eq!(options.cache_retention, Some(pi_ai::CacheRetention::None));
                assert!(options.deferred.is_none());
                assert_eq!(options.timeout_ms, Some(123));
                assert!(options.session_id.is_some());
                assert!(options.signal.is_some());
                assert!(options.telemetry.is_some());
                assert_eq!(context.messages.len(), 2);
                let prompt = serde_json::to_string(&context.messages[1]).unwrap();
                assert!(prompt.contains("focus on exact paths"));
                assert!(prompt.contains("[User]: old user"));
                assert!(!prompt.contains("recent assistant"));
                capture.fetch_add(1, Ordering::Relaxed);
                faux_text_message("\u{feff}  summary ready  \u{feff}")
            },
        )]);
        let (scheduler, conversation, storage) = open(
            retention,
            Arc::new(Mutex::new(models)),
            Arc::new(Mutex::new(settings())),
            vec![],
            true,
        )
        .await;
        seed(&scheduler, conversation).await;
        let id = start(&scheduler, conversation).await;
        let result = completed(&settled(&scheduler, id).await);
        assert!(result.entry_id.is_none());
        let submission = storage
            .submission(result.submission_id.unwrap(), &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            submission.state,
            SubmissionState::WriteDone { .. }
        ));
        let entries = storage
            .scan_entries(
                &EntryQuery {
                    conversation_id: conversation,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                },
                100,
                None,
                &Context::default(),
            )
            .await
            .unwrap();
        let summary = entries
            .items
            .iter()
            .find(|entry| entry.kind.equals_str("pi.compaction"))
            .unwrap();
        assert!(summary.head.is_some());
        assert!(
            summary.model.as_ref().unwrap()[0]["content"][0]["text"]
                .to_utf8()
                .unwrap()
                .contains("<summary>\nsummary ready\n</summary>")
        );
        let live = scheduler
            .session()
            .snapshot(&LIVE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert!(live.compactions.is_none());
        assert!(live.compaction_failure.is_none());
        let usage = scheduler
            .session()
            .snapshot(&USAGE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.models.len(), 1);
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn retry_resends_pinned_request_and_counts_each_attempt() {
    let (models, faux) = models();
    let settings = Arc::new(Mutex::new(settings()));
    let updates = settings.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    faux.set_responses(vec![
        FauxResponseStep::factory(move |context, options, _, _| {
            seen.lock()
                .push((serde_json::to_string(context).unwrap(), options.timeout_ms));
            updates.lock().stream = object(r#"{"timeoutMs":999}"#);
            let mut error = faux_text_message("");
            error.stop_reason = pi_ai::StopReason::Error;
            error.error_message = Some("provider overloaded".into());
            error
        }),
        FauxResponseStep::factory({
            let requests = requests.clone();
            move |context, options, _, _| {
                requests
                    .lock()
                    .push((serde_json::to_string(context).unwrap(), options.timeout_ms));
                faux_text_message("summary")
            }
        }),
    ]);
    let (scheduler, conversation, _storage) =
        open(0.0, Arc::new(Mutex::new(models)), settings, vec![], true).await;
    seed(&scheduler, conversation).await;
    let id = start(&scheduler, conversation).await;
    completed(&settled(&scheduler, id).await);
    assert_eq!(faux.state().call_count, 2);
    let seen = requests.lock().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0].1, Some(123));
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn hook_decisions_avoid_model_spend_and_models_remain_live_after_invocation() {
    let (models, faux) = models();
    let current = Arc::new(Mutex::new(models));
    let retained = Arc::new(Mutex::new(None));
    let capture = retained.clone();
    let skipped = Arc::new(AtomicUsize::new(0));
    let skipped_call = skipped.clone();
    let hooks = vec![
        Arc::new(CompactionHooks {
            before_compact: Some(Arc::new(move |source, runtime, _| {
                assert!(!source.entries.is_empty());
                assert!(!source.messages.is_empty());
                *capture.lock() = Some(runtime);
                Box::pin(async {
                    Ok(Some(CompactionDecision::Summary(JsonString::from_utf16(
                        vec![0xd800],
                    ))))
                })
            })),
            ..Default::default()
        }),
        Arc::new(CompactionHooks {
            before_compact: Some(Arc::new(move |_, _, _| {
                skipped_call.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { Ok(Some(CompactionDecision::Decline)) })
            })),
            ..Default::default()
        }),
    ];
    let (scheduler, conversation, _storage) = open(
        600000.0,
        current.clone(),
        Arc::new(Mutex::new(settings())),
        hooks,
        true,
    )
    .await;
    seed(&scheduler, conversation).await;
    let id = start(&scheduler, conversation).await;
    completed(&settled(&scheduler, id).await);
    assert_eq!(faux.state().call_count, 0);
    assert_eq!(skipped.load(Ordering::Relaxed), 0);
    let runtime: CompactionRuntime = retained.lock().take().unwrap();
    assert!(
        runtime
            .models()
            .unwrap()
            .get_model("compaction-test", "fixture")
            .is_some()
    );
    *current.lock() = Models::default();
    assert!(
        runtime
            .models()
            .unwrap()
            .get_model("compaction-test", "fixture")
            .is_none()
    );
    let error = Error::message("models getter failed");
    let observed = error.clone();
    let reader: pi_durable::harness::models::ModelsReader = Arc::new(move || Err(observed.clone()));
    assert!(
        pi_durable::harness::models::read_models(&reader)
            .err()
            .unwrap()
            .same_error(&error)
    );

    let usage = scheduler
        .session()
        .snapshot(&USAGE_DOC, conversation, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(usage.models.is_empty());
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
    assert!(runtime.models().is_ok());
}

#[tokio::test]
async fn absent_model_and_empty_range_finish_with_matching_cleanup() {
    for configure in [false, true] {
        let (models, faux) = models();
        let (scheduler, conversation, _storage) = open(
            0.0,
            Arc::new(Mutex::new(models)),
            Arc::new(Mutex::new(settings())),
            vec![],
            configure,
        )
        .await;
        let id = start(&scheduler, conversation).await;
        let task = settled(&scheduler, id).await;
        if configure {
            assert!(completed(&task).submission_id.is_none());
        } else {
            let encoded = pi_chord::json::to_value(&task).unwrap();
            assert!(encoded["status"].is_null());
            assert!(
                encoded["state"]["outcome"]["error"]["message"]
                    .string_equals("No model is configured")
            );
        }
        let live = scheduler
            .session()
            .snapshot(&LIVE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert!(live.compactions.is_none());
        assert_eq!(live.compaction_failure.is_some(), !configure);
        assert_eq!(faux.state().call_count, 0);
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn abort_cancels_provider_and_rejects_a_late_summary() {
    let (models, faux) = models();
    let reached = Arc::new(Semaphore::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let signal = Arc::new(Mutex::new(None));
    faux.set_responses(vec![FauxResponseStep::Factory(Arc::new({
        let reached = reached.clone();
        let gate = gate.clone();
        let signal = signal.clone();
        move |_, options, _, _| {
            *signal.lock() = options.signal.clone();
            reached.add_permits(1);
            let gate = gate.clone();
            Box::pin(async move {
                gate.acquire().await.unwrap().forget();
                Ok(faux_text_message("too late"))
            })
        }
    }))]);
    let (scheduler, conversation, _storage) = open(
        0.0,
        Arc::new(Mutex::new(models)),
        Arc::new(Mutex::new(settings())),
        vec![],
        true,
    )
    .await;
    seed(&scheduler, conversation).await;
    let id = start(&scheduler, conversation).await;
    scheduler.resume().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), reached.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let abort = scheduler.abort(id, &Context::default());
    let token = signal.lock().as_ref().unwrap().clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), token.cancelled())
        .await
        .unwrap();
    gate.add_permits(1);
    abort.await.unwrap();
    let task = settled(&scheduler, id).await;
    assert!(matches!(
        task.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    assert!(signal.lock().as_ref().unwrap().is_cancelled());
    let live = scheduler
        .session()
        .snapshot(&LIVE_DOC, conversation, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(live.compactions.is_none());
    let usage = scheduler
        .session()
        .snapshot(&USAGE_DOC, conversation, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(usage.models.is_empty());
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn rejected_summaries_record_failure_and_spend_and_declines_spend_nothing() {
    for (stop, text, error, expected) in [
        (
            pi_ai::StopReason::Length,
            "partial",
            None,
            "Summarization hit the token limit; the summary is incomplete",
        ),
        (
            pi_ai::StopReason::Error,
            "",
            Some("billing quota exhausted"),
            "Summarization failed: billing quota exhausted",
        ),
        (
            pi_ai::StopReason::Aborted,
            "partial",
            None,
            "Summarization failed: aborted",
        ),
        (
            pi_ai::StopReason::Stop,
            "  ",
            None,
            "Summarization produced no text",
        ),
    ] {
        let (models, faux) = models();
        let mut response = faux_text_message(text);
        response.stop_reason = stop;
        response.error_message = error.map(String::from);
        faux.set_responses(vec![response.into()]);
        let (scheduler, conversation, storage) = open(
            0.0,
            Arc::new(Mutex::new(models)),
            Arc::new(Mutex::new(settings())),
            vec![],
            true,
        )
        .await;
        seed(&scheduler, conversation).await;
        let id = start(&scheduler, conversation).await;
        let task = settled(&scheduler, id).await;
        assert!(matches!(
            task.state,
            TaskState::Terminal {
                outcome: TaskOutcome::Failed { .. }
            }
        ));
        let live = scheduler
            .session()
            .snapshot(&LIVE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert!(live.compactions.is_none());
        assert!(
            live.compaction_failure
                .as_ref()
                .unwrap()
                .error
                .equals_str(expected)
        );
        let usage = scheduler
            .session()
            .snapshot(&USAGE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.models.len(), 1);
        assert_eq!(faux.state().call_count, 1);
        assert!(
            storage
                .scan_submissions(&SubmissionQuery::default(), 100, None, &Context::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
    let (models, faux) = models();
    let hooks = vec![Arc::new(CompactionHooks {
        before_compact: Some(Arc::new(|_, _, _| {
            Box::pin(async { Ok(Some(CompactionDecision::Decline)) })
        })),
        ..Default::default()
    })];
    let (scheduler, conversation, _) = open(
        0.0,
        Arc::new(Mutex::new(models)),
        Arc::new(Mutex::new(settings())),
        hooks,
        true,
    )
    .await;
    seed(&scheduler, conversation).await;
    let id = start(&scheduler, conversation).await;
    assert!(
        completed(&settled(&scheduler, id).await)
            .submission_id
            .is_none()
    );
    assert_eq!(faux.state().call_count, 0);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_blocking_compaction_appends_directly_before_releasing_its_parent() {
    let (models, faux) = models();
    faux.set_responses(vec![faux_text_message("blocking summary").into()]);
    let (scheduler, conversation, storage) = open(
        0.0,
        Arc::new(Mutex::new(models)),
        Arc::new(Mutex::new(settings())),
        vec![],
        true,
    )
    .await;
    seed(&scheduler, conversation).await;
    let parent = scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                Ok(tx
                    .create_task(&owner_task(), JsonValue::Null, options)
                    .await?
                    .untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let record = settled(&scheduler, parent).await;
    let TaskState::Terminal {
        outcome: TaskOutcome::Completed { result },
    } = record.state
    else {
        panic!("parent failed: {:?}", record.state);
    };
    assert!(result[0]["status"].string_equals("completed"));
    assert!(result[0]["result"]["entryId"].as_u64().is_some());
    assert!(result[0]["result"]["submissionId"].is_null());
    assert!(
        storage
            .scan_submissions(&SubmissionQuery::default(), 100, None, &Context::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let live = scheduler
        .session()
        .snapshot(&LIVE_DOC, conversation, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(live.compactions.is_none());
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn busy_summary_queues_and_a_newer_reset_settles_it_stale_without_refunding_usage() {
    use pi_durable::harness::{generation::start_run, inbox::*};
    let (models, faux) = models();
    faux.set_responses(vec![faux_text_message("queued summary").into()]);
    let (scheduler, conversation, storage) = open(
        0.0,
        Arc::new(Mutex::new(models)),
        Arc::new(Mutex::new(settings())),
        vec![],
        true,
    )
    .await;
    seed(&scheduler, conversation).await;
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                start_run(&tx, conversation, &live, Vec::new()).await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let id = start(&scheduler, conversation).await;
    let result = completed(&settled(&scheduler, id).await);
    let submission = result.submission_id.unwrap();
    assert!(matches!(
        storage
            .submission(submission, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::WriteQueued
    ));
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut reset = EntryDraft::new("pi.reset");
                reset.head = Some(EntryHead::ThisEntry);
                tx.append_entry(conversation, reset).await?;
                // Reads for admission stay before the boundary's first table write.
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut boundary =
                    prepare_boundary(&tx, conversation, QueueModes::default()).await?;
                apply_boundary(&tx, &mut boundary, BoundaryAt::Final, 42.0).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let record = storage
        .submission(submission, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(record.state, SubmissionState::WriteUnanswered { reason, .. } if reason.equals_str("stale"))
    );
    let usage = scheduler
        .session()
        .snapshot(&USAGE_DOC, conversation, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.models.len(), 1);
    assert_eq!(faux.state().call_count, 1);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
