use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::{
    AbortController,
    context::{AbortError, AbortReason, with_abort_signal},
};
use pi_durable::{
    harness::{agent::*, hooks::*, registry::*, scheduler::*, settings::*},
    storage::StorageFuture,
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;

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
type Ext = Extension<Tool, Section>;
type JsonTask<H = ()> = Task<JsonValue, JsonValue, JsonValue, H>;
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn definition<H: Send + Sync + 'static>(
    name: &str,
    phases: IndexMap<JsonString, PhaseHandler<JsonValue, JsonValue, JsonValue, H>>,
) -> JsonTask<H> {
    define_task(TaskDefinition {
        name: name.into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"a"}"#))),
        phases,
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
async fn open<H: Send + Sync + 'static>(
    task: &JsonTask<H>,
    registry: Registry<Tool, Section>,
    options: SchedulerOptions,
) -> (TaskScheduler, ConversationId, TaskId) {
    let mut ext = Ext::new("tasks");
    ext.tasks.push(task.erased());
    registry.install(Arc::new(ext)).unwrap();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        registry,
        options,
        SessionOptions {
            conversation_created: Some(Arc::new(|tx, record| create_agent(&tx, record))),
            ..Default::default()
        },
        &Context::default(),
    )
    .await
    .unwrap();
    let task = task.clone();
    let (root, id) = scheduler
        .session()
        .commit(
            move |tx| async move {
                let root = tx.create_root_conversation().await?;
                let id = tx
                    .create_task(
                        &task,
                        JsonValue::Null,
                        TaskOptions {
                            conversation_id: Some(root.id),
                            ..TaskOptions::new(TaskOwnership::Conversation)
                        },
                    )
                    .await?;
                Ok((root.id, id.untyped()))
            },
            &Context::default(),
        )
        .await
        .unwrap();
    (scheduler, root, id)
}
async fn complete<H: Send + Sync + 'static>(
    runtime: TaskRuntime<JsonValue, JsonValue, JsonValue, H>,
    context: Context,
) -> Result<()> {
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
}
async fn acquire(gate: &Semaphore) {
    gate.acquire().await.unwrap().forget();
}

type Ping = Arc<dyn Fn() -> StorageFuture<()> + Send + Sync>;
struct Hooks {
    label: &'static str,
    enabled: bool,
    fail: bool,
    calls: Arc<Mutex<Vec<&'static str>>>,
}
fn ping() -> HookKey<Hooks, Ping> {
    HookKey::new("ping", |hooks: Arc<Hooks>| {
        hooks.enabled.then(|| {
            Arc::new(move || {
                let hooks = hooks.clone();
                Box::pin(async move {
                    hooks.calls.lock().push(hooks.label);
                    if hooks.fail {
                        Err(Error::message(hooks.label))
                    } else {
                        Ok(())
                    }
                }) as StorageFuture<()>
            }) as Ping
        })
    })
}
fn install_hook(
    registry: &Registry<Tool, Section>,
    task: &JsonTask<Hooks>,
    label: &'static str,
    enabled: bool,
    fail: bool,
    calls: &Arc<Mutex<Vec<&'static str>>>,
) {
    let mut ext = Ext::new(label);
    ext.hooks.push(hook(
        task,
        Arc::new(Hooks {
            label,
            enabled,
            fail,
            calls: calls.clone(),
        }),
    ));
    registry.install(Arc::new(ext)).unwrap();
}

#[tokio::test]
async fn lazy_agent_and_hooks_are_shared_within_a_phase_and_refresh_after_progress() {
    let before = Arc::new(Semaphore::new(0));
    let after = Arc::new(Semaphore::new(0));
    let first = Arc::new(Semaphore::new(0));
    let used = Arc::new(Semaphore::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let retained = Arc::new(Mutex::new(None));
    let handler: PhaseHandler<JsonValue, JsonValue, JsonValue, Hooks> = {
        let (before, after, first, used, seen, calls, retained) = (
            before.clone(),
            after.clone(),
            first.clone(),
            used.clone(),
            seen.clone(),
            calls.clone(),
            retained.clone(),
        );
        Arc::new(move |task, runtime, context| {
            let (before, after, first, used, seen, calls, retained) = (
                before.clone(),
                after.clone(),
                first.clone(),
                used.clone(),
                seen.clone(),
                calls.clone(),
                retained.clone(),
            );
            Box::pin(async move {
                if task.checkpoint()["phase"].string_equals("a") {
                    first.add_permits(1);
                    acquire(&before).await;
                    let agent = runtime.agent(&context).await?;
                    runtime.hooks().each(&ping(), |handler| handler()).await?;
                    seen.lock().push((
                        agent.typed::<Tool, Section>()?.thinking_level.clone(),
                        calls.lock().drain(..).collect::<Vec<_>>(),
                    ));
                    used.add_permits(1);
                    acquire(&after).await;
                    assert!(agent.same_agent(&runtime.agent(&context).await?));
                    runtime.hooks().each(&ping(), |handler| handler()).await?;
                    seen.lock().push((
                        agent.typed::<Tool, Section>()?.thinking_level.clone(),
                        calls.lock().drain(..).collect(),
                    ));
                    *retained.lock() = Some(runtime.clone());
                    runtime
                        .commit(
                            |_, _| async {
                                Ok(Some(NextTaskState::Running {
                                    checkpoint: json(r#"{"phase":"b"}"#),
                                }))
                            },
                            &context,
                        )
                        .await
                } else {
                    let agent = runtime.agent(&context).await?;
                    runtime.hooks().each(&ping(), |handler| handler()).await?;
                    seen.lock().push((
                        agent.typed::<Tool, Section>()?.thinking_level.clone(),
                        calls.lock().drain(..).collect(),
                    ));
                    complete(runtime, context).await
                }
            })
        })
    };
    let task = definition(
        "phase-agent",
        IndexMap::from([("a".into(), handler.clone()), ("b".into(), handler)]),
    );
    let registry = Registry::new([]).unwrap();
    install_hook(&registry, &task, "before", true, false, &calls);
    let live = Arc::new(AtomicBool::new(false));
    let read = live.clone();
    let (scheduler, root, id) = open(
        &task,
        registry.clone(),
        SchedulerOptions {
            settings: Arc::new(move || {
                Ok(resolve_settings(HarnessSettings {
                    follow_up_mode: Some(
                        if read.load(Ordering::Relaxed) {
                            "all"
                        } else {
                            "one-at-a-time"
                        }
                        .into(),
                    ),
                    ..Default::default()
                }))
            }),
            ..Default::default()
        },
    )
    .await;
    scheduler.resume().unwrap();
    acquire(&first).await;
    scheduler
        .session()
        .commit(
            move |tx| async move {
                configure(
                    &tx,
                    root,
                    serde_json::from_str(r#"{"thinkingLevel":"low"}"#).unwrap(),
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    before.add_permits(1);
    acquire(&used).await;
    install_hook(&registry, &task, "late", true, false, &calls);
    live.store(true, Ordering::Relaxed);
    scheduler
        .session()
        .commit(
            move |tx| async move {
                configure(
                    &tx,
                    root,
                    serde_json::from_str(r#"{"thinkingLevel":"high"}"#).unwrap(),
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    after.add_permits(1);
    scheduler
        .wait_for_task(id, &Context::default())
        .await
        .unwrap();
    assert_eq!(
        *seen.lock(),
        [
            ("low".into(), vec!["before"]),
            ("low".into(), vec!["before"]),
            ("high".into(), vec!["before", "late"])
        ]
    );
    let runtime = retained.lock().take().unwrap();
    assert_eq!(
        runtime.settings().unwrap().follow_up_mode,
        JsonString::from("all")
    );
    assert!(
        runtime
            .agent(&Context::default())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("invocation has ended")
    );
    assert!(
        runtime
            .hooks()
            .each(&ping(), |handler| handler())
            .await
            .is_err()
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_or_dropped_agent_waiters_leave_one_shared_resolution_running() {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let run: PhaseHandler<JsonValue, JsonValue, JsonValue> = Arc::new(|_, runtime, context| {
        Box::pin(async move {
            let controller = AbortController::new();
            controller.abort_with_reason(AbortReason::Error(AbortError::new("caller gone")));
            let caller = with_abort_signal(controller.signal(), &context);
            assert_eq!(
                runtime.agent(&caller).await.err().unwrap().to_string(),
                "caller gone"
            );
            drop(runtime.agent(&context));
            let first = runtime.agent(&context).await?;
            assert!(first.same_agent(&runtime.agent(&context).await?));
            complete(runtime, context).await
        })
    });
    let task = definition("cancel-agent", IndexMap::from([("a".into(), run)]));
    let (count, started, gate) = (calls.clone(), entered.clone(), release.clone());
    let (scheduler, _, id) = open(
        &task,
        Registry::new([]).unwrap(),
        SchedulerOptions {
            agent: Arc::new(move |request| {
                count.fetch_add(1, Ordering::Relaxed);
                let (started, gate) = (started.clone(), gate.clone());
                Box::pin(async move {
                    started.add_permits(1);
                    acquire(&gate).await;
                    resolve_agent_in(request).await
                })
            }),
            ..Default::default()
        },
    )
    .await;
    scheduler.resume().unwrap();
    acquire(&entered).await;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    release.add_permits(1);
    scheduler
        .wait_for_task(id, &Context::default())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn an_agent_failure_is_cached_even_when_its_first_caller_was_cancelled() {
    let calls = Arc::new(AtomicUsize::new(0));
    let failure = Error::message("settings broke");
    let expected = failure.clone();
    let run: PhaseHandler<JsonValue, JsonValue, JsonValue> =
        Arc::new(move |_, runtime, context| {
            let failure = expected.clone();
            Box::pin(async move {
                let controller = AbortController::new();
                controller.abort();
                let caller = with_abort_signal(controller.signal(), &context);
                assert!(runtime.agent(&caller).await.is_err());
                let error = runtime.agent(&context).await.err().unwrap();
                assert!(error.same_error(&failure));
                assert!(
                    runtime
                        .agent(&context)
                        .await
                        .err()
                        .unwrap()
                        .same_error(&error)
                );
                complete(runtime, context).await
            })
        });
    let task = definition("failed-agent", IndexMap::from([("a".into(), run)]));
    let count = calls.clone();
    let (scheduler, _, id) = open(
        &task,
        Registry::new([]).unwrap(),
        SchedulerOptions {
            settings: Arc::new(move || Err(failure.clone())),
            agent: Arc::new(move |request| {
                count.fetch_add(1, Ordering::Relaxed);
                resolve_agent_in(request)
            }),
            ..Default::default()
        },
    )
    .await;
    scheduler.resume().unwrap();
    scheduler
        .wait_for_task(id, &Context::default())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn hooks_bind_their_owners_skip_missing_members_and_continue_after_reported_failures() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let reports = Arc::new(Mutex::new(Vec::new()));
    let out = reports.clone();
    let run: PhaseHandler<JsonValue, JsonValue, JsonValue, Hooks> =
        Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime.hooks().each(&ping(), |handler| handler()).await?;
                let property = HookKey::<Hooks, Ping>::try_new("property", |_| {
                    Err(Error::message("member access failed"))
                });
                assert_eq!(
                    runtime
                        .hooks()
                        .each(&property, |handler| handler())
                        .await
                        .err()
                        .unwrap()
                        .to_string(),
                    "member access failed"
                );
                complete(runtime, context).await
            })
        });
    let task = definition("hooks", IndexMap::from([("a".into(), run)]));
    let registry = Registry::new([]).unwrap();
    for (label, enabled, fail) in [
        ("a", true, false),
        ("missing", false, false),
        ("bad", true, true),
        ("last", true, false),
    ] {
        install_hook(&registry, &task, label, enabled, fail, &calls);
    }
    let (scheduler, conversation, id) = open(
        &task,
        registry,
        SchedulerOptions {
            on_report: Arc::new(move |error, info, _| out.lock().push((error.to_string(), info))),
            ..Default::default()
        },
    )
    .await;
    scheduler.resume().unwrap();
    scheduler
        .wait_for_task(id, &Context::default())
        .await
        .unwrap();
    assert_eq!(*calls.lock(), ["a", "bad", "last"]);
    {
        let reports = reports.lock();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, "bad");
        assert_eq!(reports[0].1.kind, "extension_error");
        assert_eq!(reports[0].1.extension, Some("bad".into()));
        assert_eq!(
            reports[0].1.task,
            Some(pi_durable::ReportTask {
                id,
                kind: task.definition().name.clone(),
                conversation_id: conversation
            })
        );
        assert!(reports[0].1.conversation.is_none());
    }
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_hook_failure_after_signalling_propagates_without_reporting_or_running_later_hooks() {
    let reached = Arc::new(Semaphore::new(0));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let reports = Arc::new(AtomicUsize::new(0));
    let out = reports.clone();
    let started = reached.clone();
    let run: PhaseHandler<JsonValue, JsonValue, JsonValue, Hooks> =
        Arc::new(move |_, runtime, _| {
            let reached = started.clone();
            Box::pin(async move {
                let signal = runtime.signal();
                let result = runtime
                    .hooks()
                    .each(&ping(), move |handler| {
                        let signal = signal.clone();
                        let reached = reached.clone();
                        async move {
                            handler().await?;
                            reached.add_permits(1);
                            signal.cancelled().await;
                            Err(Error::message("interrupted hook"))
                        }
                    })
                    .await;
                assert_eq!(result.err().unwrap().to_string(), "interrupted hook");
                Ok(())
            })
        });
    let task = define_task(TaskDefinition {
        name: "abort-hooks".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"a"}"#))),
        phases: IndexMap::from([("a".into(), run)]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                assert_eq!(
                    runtime
                        .agent(&context)
                        .await?
                        .typed::<Tool, Section>()?
                        .thinking_level,
                    JsonString::from("high")
                );
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
    });
    let registry = Registry::new([]).unwrap();
    install_hook(&registry, &task, "first", true, false, &calls);
    install_hook(&registry, &task, "never", true, false, &calls);
    let (scheduler, root, id) = open(
        &task,
        registry,
        SchedulerOptions {
            on_report: Arc::new(move |_, _, _| {
                out.fetch_add(1, Ordering::Relaxed);
            }),
            ..Default::default()
        },
    )
    .await;
    scheduler.resume().unwrap();
    acquire(&reached).await;
    scheduler
        .session()
        .commit(
            move |tx| async move {
                configure(
                    &tx,
                    root,
                    serde_json::from_str(r#"{"thinkingLevel":"high"}"#).unwrap(),
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.abort(id, &Context::default()).await.unwrap();
    assert!(matches!(
        scheduler
            .wait_for_task(id, &Context::default())
            .await
            .unwrap()
            .state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    assert_eq!(*calls.lock(), ["first"]);
    assert_eq!(reports.load(Ordering::Relaxed), 0);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
