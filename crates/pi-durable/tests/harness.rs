use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::{models::Models, providers::faux::*};
use pi_durable::{
    harness::{
        agent::AGENT_DOC,
        conversation::{BoundSubmission, InputSubmissionDraft},
        generation::GenerationPromptInput,
        registry::*,
        settings::HarnessSettings,
        tool::NativeTool,
    },
    *,
};
use std::sync::Arc;

struct Section(JsonString);
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        &self.0
    }
}
impl PromptSection<GenerationPromptInput<NativeTool, Section>> for Section {
    fn render(
        &self,
        _: Arc<GenerationPromptInput<NativeTool, Section>>,
        _: &Context,
    ) -> storage::StorageFuture<Option<JsonString>> {
        Box::pin(async { Ok(None) })
    }
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(pi_chord::json::parse(text).unwrap()).unwrap()
}
fn registry() -> Registry<NativeTool, Section> {
    create_registry().unwrap()
}
fn options(
    registry: Registry<NativeTool, Section>,
    models: Models,
    retention: f64,
) -> HarnessOptions {
    HarnessOptions::new(registry, models).with_settings(HarnessSettings {
        document_retention_ms: Some(retention),
        ..Default::default()
    })
}
fn agent() -> ConversationOptions {
    ConversationOptions {
        agent: Some(object(
            r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#,
        )),
        ..Default::default()
    }
}

#[test]
fn builtin_definitions_keep_identity_across_public_registry_creations() {
    let first = registry();
    let second = registry();
    for name in ["pi.generation", "pi.tool", "pi.compaction"] {
        assert!(
            first
                .snapshot()
                .task(&name.into())
                .unwrap()
                .same_definition(&second.snapshot().task(&name.into()).unwrap()),
            "{name} must be the same built-in definition"
        );
    }
}

#[tokio::test]
async fn public_submissions_and_active_work_keep_their_harness_services_alive() {
    use pi_durable::harness::{declaration::ToolDeclaration, tool::ToolExecutionResult};
    use std::sync::atomic::{AtomicBool, Ordering};
    for mode in ["submission", "lookup", "active"] {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let seen = reports.clone();
        let reached = Arc::new(AtomicBool::new(false));
        let ran = reached.clone();
        let probe = NativeTool::new(
            ToolDeclaration::new(
                "probe",
                "probe",
                object(r#"{"type":"object","properties":{}}"#).into_value(),
            ),
            Arc::new(move |_, api, context| {
                let ran = ran.clone();
                Box::pin(async move {
                    let id = api
                        .commit(
                            |tx| async move {
                                Ok(tx
                                    .create_conversation(ConversationOwnership::Ownerless)
                                    .await?
                                    .id)
                            },
                            &context,
                        )
                        .await?;
                    assert!(api.conversation(id, &context).await?.is_some());
                    ran.store(true, Ordering::SeqCst);
                    Ok(ToolExecutionResult::default())
                })
            }),
        );
        let registry = registry();
        let mut extension = Extension::new("probe");
        extension.tools.push(Arc::new(probe));
        registry.install(Arc::new(extension)).unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let wait = gate.clone();
        let models = Models::default();
        let faux = faux_provider(RegisterFauxProviderOptions {
            tokens_per_second: Some(f64::INFINITY),
            ..Default::default()
        });
        faux.set_responses(vec![
            FauxResponseStep::Factory(Arc::new(move |_, _, _, _| {
                let wait = wait.clone();
                Box::pin(async move {
                    wait.notified().await;
                    Ok(faux_assistant_message(
                        vec![faux_tool_call("probe", Default::default(), Some("probe-1"))],
                        FauxMessageOptions {
                            stop_reason: Some(pi_ai::StopReason::ToolUse),
                            ..Default::default()
                        },
                    ))
                })
            })),
            faux_text_message("done").into(),
        ]);
        models.set_provider(faux.provider);
        let lifetime = Arc::new(());
        let released = Arc::downgrade(&lifetime);
        let mut options = HarnessOptions::new(registry, models);
        options.on_report = Some(Arc::new(move |error, _| {
            seen.lock().push(error.to_string())
        }));
        options.env = Some(Arc::new(move |_, _| {
            let _ = &lifetime;
            Box::pin(async { Ok(None) })
        }));
        let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &Context::default())
            .await
            .unwrap();
        let session = harness.scheduler().session();
        let root = harness.root(agent(), &Context::default()).await.unwrap();
        let submission = root
            .submit(SubmissionDraft::input("probe"), &Context::default())
            .await
            .unwrap();
        let wait: storage::StorageFuture<()> = if mode == "lookup" {
            let found = harness
                .submission(submission.id, &Context::default())
                .await
                .unwrap()
                .unwrap();
            drop(submission);
            let wait = found.wait(&Context::default());
            drop(found);
            drop(root);
            drop(harness);
            Box::pin(async move { wait.await.map(|_| ()) })
        } else if mode == "submission" {
            drop(root);
            drop(harness);
            let wait = submission.wait(&Context::default());
            drop(submission);
            Box::pin(async move { wait.await.map(|_| ()) })
        } else {
            let wait = harness.wait_for_idle(&Context::default());
            drop(submission);
            drop(root);
            drop(harness);
            wait
        };
        gate.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), wait)
            .await
            .unwrap()
            .unwrap();
        session.close(&Context::default()).await.unwrap();
        assert!(
            reached.load(Ordering::SeqCst),
            "{mode} must keep environment and conversation services available"
        );
        {
            let reports = reports.lock();
            assert!(reports.is_empty(), "{mode}: {:?}", *reports);
        }
        assert!(
            released.upgrade().is_none(),
            "finished handles must release their Harness owner"
        );
    }
}

#[tokio::test]
async fn sleeping_work_keeps_services_after_the_public_handles_are_dropped() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    type Callback = Arc<Mutex<Option<timers::TimerCallback>>>;
    let callback: Callback = Arc::default();
    let armed = callback.clone();
    let (send, registered) = tokio::sync::oneshot::channel();
    let send = Arc::new(Mutex::new(Some(send)));
    let clock = Arc::new(AtomicU64::new(100));
    let now = clock.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let built = calls.clone();
    let task = define_task(TaskDefinition::<JsonValue, JsonValue, JsonValue> {
        name: "test.sleeping-services".into(),
        version: 1,
        initial: Arc::new(|_| Ok(object(r#"{"phase":"start"}"#).into_value())),
        phases: IndexMap::from([
            (
                "start".into(),
                Arc::new(|_, runtime: TaskRuntime, context| {
                    Box::pin(async move {
                        runtime
                            .sleep_until(
                                1000.0,
                                object(r#"{"phase":"finish"}"#).into_value(),
                                &context,
                            )
                            .await
                    }) as storage::StorageFuture<()>
                }) as PhaseHandler<_, _, _>,
            ),
            (
                "finish".into(),
                Arc::new(|_, runtime: TaskRuntime, context| {
                    Box::pin(async move {
                        assert!(runtime.env(&context).await?.is_none());
                        assert!(
                            runtime
                                .conversation(runtime.conversation_id(), &context)
                                .await?
                                .is_some()
                        );
                        runtime
                            .commit(
                                |_, _| async {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: JsonValue::Null,
                                        },
                                    }))
                                },
                                &context,
                            )
                            .await
                    }) as storage::StorageFuture<()>
                }) as PhaseHandler<_, _, _>,
            ),
        ]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let registry = registry();
    let mut extension = Extension::new("sleep");
    extension.tasks.push(task.erased());
    registry.install(Arc::new(extension)).unwrap();
    let mut options =
        HarnessOptions::new(registry, Models::default()).with_settings(HarnessSettings {
            document_retention_ms: Some(f64::INFINITY),
            context_retention_ms: Some(0.0),
            ..Default::default()
        });
    options.env = Some(Arc::new(move |_, _| {
        built.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    }));
    options.now = Some(Arc::new(move || Ok(now.load(Ordering::SeqCst) as f64)));
    options.set_timer = Some(Arc::new(move |timer, delay, background| {
        assert!(!background);
        assert_eq!(delay, 900.0);
        *armed.lock() = Some(timer);
        if let Some(send) = send.lock().take() {
            let _ = send.send(());
        }
        let armed = armed.clone();
        Some(Box::new(move || {
            armed.lock().take();
        }))
    }));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &Context::default())
        .await
        .unwrap();
    let session = harness.scheduler().session();
    let root = harness
        .root(Default::default(), &Context::default())
        .await
        .unwrap();
    let id = root
        .commit(
            move |tx| async move {
                tx.create_task(
                    &task,
                    JsonValue::Null,
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let wait = harness.wait_for_task(id.untyped(), &Context::default());
    registered.await.unwrap();
    drop(root);
    drop(harness);
    clock.store(1000, Ordering::SeqCst);
    let fire = callback.lock().take().unwrap();
    fire();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), wait)
        .await
        .unwrap()
        .unwrap();
    session.close(&Context::default()).await.unwrap();
    assert!(matches!(
        result.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn foreground_timer_failures_are_reported_with_zero_document_retention() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = reports.clone();
    let error = Error::message("no timers");
    let thrown = error.clone();
    let mut options = options(registry(), Models::default(), 0.0);
    options.set_timer = Some(Arc::new(move |_, _, _| {
        std::panic::panic_any(thrown.clone())
    }));
    options.on_report = Some(Arc::new(move |error, info| seen.lock().push((error, info))));
    let registry = registry();
    let task = define_task(TaskDefinition::<JsonValue, JsonValue, JsonValue> {
        name: "test.timer".into(),
        version: 1,
        initial: Arc::new(|_| Ok(object(r#"{"phase":"run"}"#).into_value())),
        phases: IndexMap::from([(
            "run".into(),
            Arc::new(|_, runtime: TaskRuntime, context| {
                Box::pin(async move {
                    drop(runtime.set_timer(|| {}, 100.0));
                    runtime
                        .commit(
                            |_, _| async {
                                Ok(Some(NextTaskState::Terminal {
                                    outcome: TaskOutcome::Completed {
                                        result: JsonValue::Null,
                                    },
                                }))
                            },
                            &context,
                        )
                        .await
                }) as storage::StorageFuture<()>
            }) as PhaseHandler<_, _, _>,
        )]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let mut extension = Extension::new("timers");
    extension.tasks.push(task.erased());
    registry.install(Arc::new(extension)).unwrap();
    options.registry = Arc::new(registry);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &Context::default())
        .await
        .unwrap();
    let root = harness
        .root(Default::default(), &Context::default())
        .await
        .unwrap();
    let id = root
        .commit(
            move |tx| async move {
                tx.create_task(
                    &task,
                    JsonValue::Null,
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    harness
        .wait_for_task(id.untyped(), &Context::default())
        .await
        .unwrap();
    {
        let reports = reports.lock();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].0.same_error(&error));
        assert_eq!(reports[0].1.kind, "host_error");
        assert_eq!(reports[0].1.callback, Some("setTimer"));
        assert!(reports[0].1.task.is_none());
    }
    harness.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn a_throwing_clock_reports_once_and_a_throwing_report_handler_does_not_stop_commits() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = reports.clone();
    let mut opts = HarnessOptions::new(registry(), Models::default());
    opts.now = Some(Arc::new(|| Err(Error::message("clock failed"))));
    opts.on_report = Some(Arc::new(move |error, info| {
        seen.lock()
            .push((error.to_string(), info.kind, info.callback));
        panic!("report handler failed");
    }));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), opts, &Context::default())
        .await
        .unwrap();
    let root = harness
        .root(Default::default(), &Context::default())
        .await
        .unwrap();
    root.configure(object(r#"{"cwd":"/repo"}"#), &Context::default())
        .await
        .unwrap();
    assert_eq!(
        reports.lock().as_slice(),
        [("clock failed".to_owned(), "host_error", Some("now"))]
    );
    assert!(harness.failure().is_none());
    harness.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn public_harness_uses_current_models_settings_identity_context_and_session_totals() {
    for retention in [600_000.0, 0.0] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let current = Arc::new(Mutex::new(Models::default()));
        let settings = Arc::new(Mutex::new(HarnessSettings {
            document_retention_ms: Some(retention),
            stream: Some(object(r#"{"timeoutMs":1000}"#)),
            ..Default::default()
        }));
        let make_models = |text: &'static str| {
            let models = Models::default();
            let faux = faux_provider(RegisterFauxProviderOptions {
                tokens_per_second: Some(f64::INFINITY),
                ..Default::default()
            });
            let seen = seen.clone();
            faux.set_responses(vec![FauxResponseStep::factory(move |_, options, _, _| {
                seen.lock()
                    .push((options.timeout_ms, options.identity.clone()));
                faux_text_message(text)
            })]);
            models.set_provider(faux.provider);
            models
        };
        *current.lock() = make_models("first");
        let mut opts = options(registry(), Models::default(), retention);
        let models = current.clone();
        opts.models = Arc::new(move || Ok(models.lock().clone()));
        let policy = settings.clone();
        opts.settings = Arc::new(move || Ok(policy.lock().clone()));
        let harness = Harness::open(Arc::new(MemoryStorage::new()), opts, &Context::default())
            .await
            .unwrap();
        let root = harness.root(agent(), &Context::default()).await.unwrap();
        for text in ["one", "two"] {
            if text == "two" {
                *current.lock() = make_models("second");
                settings.lock().stream = Some(object(r#"{"timeoutMs":2000}"#));
            }
            let receipt = root
                .submit(SubmissionDraft::input(text), &Context::default())
                .await
                .unwrap()
                .wait(&Context::default())
                .await
                .unwrap();
            assert_eq!(
                pi_chord::json::to_value(&receipt).unwrap()["status"],
                JsonValue::from("done")
            );
        }
        {
            let seen = seen.lock();
            assert_eq!(
                seen.iter().map(|(timeout, _)| *timeout).collect::<Vec<_>>(),
                [Some(1000), Some(2000)]
            );
            for (_, identity) in seen.iter() {
                assert_eq!(
                    identity
                        .as_ref()
                        .unwrap()
                        .attribution
                        .as_ref()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .name,
                    "pi"
                );
            }
        }
        let view = root.context(None, &Context::default()).await.unwrap();
        assert_eq!(view.messages.len(), 4);
        assert_eq!(
            view.messages[3]["content"][0]["text"],
            JsonValue::from("second")
        );
        let usage = harness.usage(&Context::default()).await.unwrap();
        assert_eq!(usage.models.len(), 1);
        assert!(
            usage.models.values().next().unwrap().value()["totalTokens"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        let watch = root.watch(&Context::default()).await.unwrap();
        assert_eq!(watch.value().conversation.id, root.id);
        watch.stop();
        root.reset(Some("handoff".into()), &Context::default())
            .await
            .unwrap();
        let reset = root.context(None, &Context::default()).await.unwrap();
        assert_eq!(reset.messages.len(), 1);
        assert!(reset.messages[0]["content"].string_equals("handoff"));
        harness.close(&Context::default()).await.unwrap();
        assert!(harness.wake().is_err());
        assert!(root.agent(&Context::default()).await.is_err());
    }
}

#[tokio::test]
async fn creation_hooks_precede_configuration_and_init_and_failed_init_rolls_back() {
    for retention in [600_000.0, 0.0] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let mut opts = options(registry(), Models::default(), retention);
        opts.conversation_created = Some(Arc::new(move |tx, record| {
            let seen = capture.clone();
            Box::pin(async move {
                assert!(
                    tx.doc(&LIVE_DOC, record.id)
                        .await?
                        .read()?
                        .compactions
                        .is_none()
                );
                let agent = tx.doc(&AGENT_DOC, record.id).await?;
                seen.lock().push(agent.read()?.0.clone());
                agent.update(|state| {
                    state.0.insert("cwd".into(), JsonValue::from("hook"));
                    Ok(())
                })
            })
        }));
        let storage = Arc::new(MemoryStorage::new());
        let harness = Harness::open(storage.clone(), opts, &Context::default())
            .await
            .unwrap();
        let root = harness
            .root(
                ConversationOptions {
                    agent: Some(object(r#"{"cwd":"configured"}"#)),
                    init: Some(Arc::new(|tx, id| {
                        Box::pin(async move {
                            assert!(
                                tx.doc(&AGENT_DOC, id).await?.read()?.0.value()["cwd"]
                                    .string_equals("configured")
                            );
                            Ok(())
                        })
                    })),
                },
                &Context::default(),
            )
            .await
            .unwrap();
        let again = harness
            .root(
                ConversationOptions {
                    init: Some(Arc::new(|_, _| {
                        Box::pin(async { panic!("existing root must skip init") })
                    })),
                    ..Default::default()
                },
                &Context::default(),
            )
            .await
            .unwrap();
        assert_eq!(root.id, again.id);
        let raw = harness
            .commit(
                |tx| async move {
                    tx.create_conversation(ConversationOwnership::Ownerless)
                        .await
                },
                &Context::default(),
            )
            .await
            .unwrap();
        assert!(
            harness
                .snapshot(&AGENT_DOC, raw.id, &Context::default())
                .await
                .unwrap()
                .unwrap()
                .0
                .value()["cwd"]
                .string_equals("hook")
        );
        assert_eq!(seen.lock().len(), 2);
        let before = storage
            .scan_conversations(
                &ConversationQuery::default(),
                100,
                None,
                &Context::default(),
            )
            .await
            .unwrap()
            .items
            .len();
        let failed = Error::message("init failed");
        let original = failed.clone();
        let result = harness
            .create_conversation(
                ConversationCreateOptions {
                    ownership: ConversationOwnership::Ownerless,
                    agent: None,
                    init: Some(Arc::new(move |_, _| {
                        let error = failed.clone();
                        Box::pin(async move { Err(error) })
                    })),
                },
                &Context::default(),
            )
            .await;
        assert!(result.err().unwrap().same_error(&original));
        assert_eq!(
            storage
                .scan_conversations(
                    &ConversationQuery::default(),
                    100,
                    None,
                    &Context::default()
                )
                .await
                .unwrap()
                .items
                .len(),
            before
        );
        assert!(harness.failure().is_none());
        harness.close(&Context::default()).await.unwrap();
    }
}

#[tokio::test]
async fn public_read_only_admission_stays_healthy_and_missing_builtins_do_not_open_storage() {
    let storage = Arc::new(MemoryStorage::new());
    let result = Harness::open(
        storage.clone(),
        HarnessOptions::new(harness::catalog::TaskCatalog::default(), Models::default()),
        &Context::default(),
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("pi.generation, pi.tool, pi.compaction")
    );
    assert!(
        storage
            .scan_conversations(
                &ConversationQuery::default(),
                100,
                None,
                &Context::default()
            )
            .await
            .is_ok()
    );
    let mut opts = HarnessOptions::new(registry(), Models::default());
    opts.read_only = true;
    let harness = Harness::open(storage, opts, &Context::default())
        .await
        .unwrap();
    assert_eq!(
        harness
            .root(Default::default(), &Context::default())
            .await
            .err()
            .unwrap()
            .name(),
        "ReadOnlySession"
    );
    assert_eq!(harness.resume().unwrap_err().name(), "ReadOnlySession");
    assert!(!harness.wake().unwrap().active);
    assert_eq!(
        harness
            .wait_for_idle(&Context::default())
            .await
            .unwrap_err()
            .name(),
        "ReadOnlySession"
    );
    assert_eq!(
        harness
            .commit(|_| async { Ok(7) }, &Context::default())
            .await
            .unwrap(),
        7
    );
    assert!(harness.failure().is_none());
    harness.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn dynamic_document_retention_moves_an_existing_timer_and_contains_reader_errors() {
    use std::sync::atomic::{AtomicU64, Ordering};
    type TimerSlot = Arc<Mutex<Option<pi_durable::timers::TimerCallback>>>;
    let timers = Arc::new(Mutex::new(Vec::<(f64, TimerSlot)>::new()));
    let registrations = timers.clone();
    let retention = Arc::new(Mutex::new(Ok(Some(60_000.0))));
    let read = retention.clone();
    let clock = Arc::new(AtomicU64::new(0));
    let now = clock.clone();
    let mut opts = HarnessOptions::new(registry(), Models::default());
    opts.now = Some(Arc::new(move || Ok(now.load(Ordering::SeqCst) as f64)));
    opts.set_timer = Some(Arc::new(move |callback, delay, background| {
        assert!(background);
        let slot = Arc::new(Mutex::new(Some(callback)));
        registrations.lock().push((delay, slot.clone()));
        Some(Box::new(move || {
            slot.lock().take();
        }))
    }));
    opts.document_retention_reader = Some(Arc::new(move || read.lock().clone()));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), opts, &Context::default())
        .await
        .unwrap();
    let root = harness
        .root(Default::default(), &Context::default())
        .await
        .unwrap();
    let first = harness
        .snapshot(&AGENT_DOC, root.id, &Context::default())
        .await
        .unwrap()
        .unwrap();
    let old = {
        let timers = timers.lock();
        assert_eq!(timers.last().unwrap().0, 60_000.0);
        let slot = timers.last().unwrap().1.clone();
        assert!(slot.lock().is_some());
        slot
    };
    // Leave the source's touch granule (a tenth of the new retention).
    clock.store(100, Ordering::SeqCst);
    *retention.lock() = Ok(Some(500.0));
    harness
        .snapshot(&AGENT_DOC, root.id, &Context::default())
        .await
        .unwrap();
    {
        let timers = timers.lock();
        assert!(
            old.lock().is_none(),
            "shorter retention disarms the old timer"
        );
        assert_eq!(timers.last().unwrap().0, 400.0);
        assert!(timers.last().unwrap().1.lock().is_some());
    }
    *retention.lock() = Ok(Some(0.0));
    harness
        .snapshot(&AGENT_DOC, root.id, &Context::default())
        .await
        .unwrap();
    harness
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
    *retention.lock() = Err(Error::message("retention failed"));
    let second = harness
        .snapshot(&AGENT_DOC, root.id, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(
        &second,
        &harness
            .snapshot(&AGENT_DOC, root.id, &Context::default())
            .await
            .unwrap()
            .unwrap()
    ));
    assert!(harness.failure().is_none());
    harness.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn zero_retention_sweeps_even_after_the_host_declined_background_timers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let retention = Arc::new(Mutex::new(Some(60_000.0)));
    let reader = retention.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = calls.clone();
    let mut options = HarnessOptions::new(registry(), Models::default());
    options.document_retention_reader = Some(Arc::new(move || Ok(*reader.lock())));
    options.set_timer = Some(Arc::new(move |_, _, background| {
        assert!(background);
        attempts.fetch_add(1, Ordering::SeqCst);
        None
    }));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &Context::default())
        .await
        .unwrap();
    let root = harness
        .root(Default::default(), &Context::default())
        .await
        .unwrap();
    harness
        .snapshot(&AGENT_DOC, root.id, &Context::default())
        .await
        .unwrap();
    assert!(harness.cache_sizes().cached > 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    *retention.lock() = Some(0.0);
    harness
        .snapshot(&AGENT_DOC, root.id, &Context::default())
        .await
        .unwrap();
    // A read is a line barrier without a commit's foreground eviction fallback.
    harness
        .get_task(TaskId::from_raw(999_999), &Context::default())
        .await
        .unwrap();
    assert_eq!(harness.cache_sizes().cached, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    harness.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn invocation_bound_conversations_and_submissions_use_the_owned_native_services() {
    let bound: Arc<Mutex<Option<(BoundSubmission, pi_chord::AbortSignal)>>> =
        Arc::new(Mutex::new(None));
    let saved = bound.clone();
    let task = define_task(TaskDefinition::<JsonValue, JsonValue, JsonValue> {
        name: "test.child-conversation".into(),
        version: 1,
        initial: Arc::new(|_| Ok(object(r#"{"phase":"run"}"#).into_value())),
        phases: IndexMap::from([(
            "run".into(),
            Arc::new(
                move |record: RunningTask<_, _, _>,
                      runtime: TaskRuntime<_, _, _>,
                      context: Context| {
                    let saved = saved.clone();
                    Box::pin(async move {
                        let child = Arc::new(Mutex::new(None));
                        let capture = child.clone();
                        let owner = record.record().id();
                        runtime
                            .commit(
                                move |tx, _| async move {
                                    *capture.lock() = Some(
                                        tx.create_conversation(ConversationOwnership::Task(owner))
                                            .await?
                                            .id,
                                    );
                                    Ok(None)
                                },
                                &context,
                            )
                            .await?;
                        let child = child.lock().take().unwrap();
                        let handle = runtime.conversation(child, &context).await?.unwrap();
                        let submission = handle
                            .submit(
                                InputSubmissionDraft::new(JsonValue::from("child")),
                                &context,
                            )
                            .await?;
                        submission.wait(&context).await?;
                        *saved.lock() = Some((submission, runtime.signal()));
                        runtime
                            .commit(
                                |_, _| async {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: JsonValue::Null,
                                        },
                                    }))
                                },
                                &context,
                            )
                            .await
                    }) as storage::StorageFuture<()>
                },
            ) as PhaseHandler<_, _, _>,
        )]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let registry = registry();
    let mut extension = Extension::new("child");
    extension.tasks.push(task.erased());
    registry.install(Arc::new(extension)).unwrap();
    let models = Models::default();
    let faux = faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    faux.set_responses(vec![faux_text_message("child answer").into()]);
    models.set_provider(faux.provider);
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &Context::default(),
    )
    .await
    .unwrap();
    let root = harness.root(agent(), &Context::default()).await.unwrap();
    let id = root
        .commit(
            move |tx| async move {
                tx.create_task(
                    &task,
                    JsonValue::Null,
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        harness.wait_for_task(id.untyped(), &Context::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        result.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    let (bound, signal) = bound.lock().take().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), signal.cancelled())
        .await
        .unwrap();
    assert!(bound.status(&Context::default()).await.is_err());
    assert_eq!(
        harness
            .usage(&Context::default())
            .await
            .unwrap()
            .models
            .len(),
        1
    );
    harness.close(&Context::default()).await.unwrap();
}
