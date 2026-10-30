use parking_lot::Mutex;
use pi_chord::output::{OutputLimits, ProgressWithError, Retain};
use pi_durable::{
    harness::{
        declaration::ToolDeclaration,
        live::{LIVE_DOC, SlotProgress, ToolSlot},
        registry::*,
        scheduler::*,
        tool::*,
    },
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
struct Section;
impl RegisteredSection for Section {
    fn key(&self) -> &JsonString {
        panic!("no sections")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_calls_from_external_threads_keep_their_admission_order() {
    for _ in 0..24 {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let child = native(
            "child",
            Arc::new(move |_, api, _| {
                record.lock().push(api.call_id.clone());
                Box::pin(async { Ok(ToolExecutionResult::default()) })
            }),
        );
        let (send, receive) = tokio::sync::oneshot::channel();
        let send = Arc::new(Mutex::new(Some(send)));
        let (release, released) = tokio::sync::oneshot::channel();
        let released = Arc::new(Mutex::new(Some(released)));
        let parent = native(
            "parent",
            Arc::new(move |_, api, _| {
                send.lock()
                    .take()
                    .unwrap()
                    .send(api)
                    .unwrap_or_else(|_| panic!("API receiver dropped"));
                let released = released.lock().take().unwrap();
                Box::pin(async move {
                    released.await.unwrap();
                    Ok(ToolExecutionResult::default())
                })
            }),
        );
        let (scheduler, conversation) = open(vec![parent, child], vec![], Default::default()).await;
        let parent = start(&scheduler, conversation, "parent", json(r#"{"n":1}"#), None).await;
        scheduler.resume().unwrap();
        // This test task runs outside the Session's executor. The public API is
        // also used this way by host runtimes and extension callback threads.
        let api = receive.await.unwrap();
        let first = api.execute_tool(
            "child",
            object(r#"{"n":1}"#),
            &Context::default(),
            Default::default(),
        );
        let second = api.execute_tool(
            "child",
            object(r#"{"n":2}"#),
            &Context::default(),
            NestedCallOptions {
                key: Some("named".into()),
                ..Default::default()
            },
        );
        let (a, b) = tokio::join!(first, second);
        a.unwrap();
        b.unwrap();
        assert_eq!(
            *seen.lock(),
            [JsonString::from("call/1"), JsonString::from("call/named")]
        );
        release.send(()).unwrap();
        scheduler
            .wait_for_task(parent, &Context::default())
            .await
            .unwrap();
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).unwrap()
}
fn native(name: &str, execute: ExecuteTool) -> NativeTool {
    NativeTool::new(
        ToolDeclaration::new(
            name,
            "",
            json(r#"{"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}"#),
        ),
        execute,
    )
}
async fn open(
    tools: Vec<NativeTool>,
    hooks: Vec<Arc<ToolHooks>>,
    options: SchedulerOptions,
) -> (TaskScheduler, ConversationId) {
    let registry = Registry::<NativeTool, Section>::new([TOOL_TASK.erased()]).unwrap();
    let mut extension = Extension::new("test-tools");
    extension.tools = tools.into_iter().map(Arc::new).collect();
    registry.install(Arc::new(extension)).unwrap();
    for (index, hook) in hooks.into_iter().enumerate() {
        let mut extension = Extension::new(format!("hook-{index}"));
        extension.hooks.push(HookRegistration::new("pi.tool", hook));
        registry.install(Arc::new(extension)).unwrap();
    }
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::default()),
        registry,
        options,
        SessionOptions {
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
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    (scheduler, conversation)
}
async fn start(
    scheduler: &TaskScheduler,
    conversation: ConversationId,
    name: &str,
    arguments: JsonValue,
    checkpoint: Option<ToolTaskCheckpoint>,
) -> TaskId {
    let name = JsonString::from(name);
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let call = ToolCall::new("call", name.clone(), arguments);
                let mut entry = EntryDraft::new("pi.assistant");
                let mut message = object(r#"{"role":"assistant","content":[]}"#);
                message.insert(
                    "content".into(),
                    pi_chord::json::to_value(&vec![call]).unwrap(),
                );
                entry.model = Some(vec![message.into_value()]);
                let entry = tx.append_entry(conversation, entry).await?;
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                let id = tx
                    .create_task(&TOOL_TASK, ToolTaskInput::model(entry.id, "call"), options)
                    .await?;
                tx.doc(&LIVE_DOC, conversation).await?.update(|live| {
                    live.tools = Some(vec![ToolSlot {
                        progress: SlotProgress::new("call", name),
                        task_id: Some(id.untyped()),
                        entry: None,
                    }]);
                    Ok(())
                })?;
                Ok(id.untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    if let Some(checkpoint) = checkpoint {
        scheduler
            .session()
            .commit(
                move |tx| async move {
                    let mut record = tx.task(id).await?.unwrap();
                    record.state = TaskState::Pending {
                        checkpoint: pi_chord::json::to_value(&checkpoint).unwrap(),
                        memos: None,
                    };
                    tx.set_task(record)
                },
                &Context::default(),
            )
            .await
            .unwrap();
    }
    id
}
async fn settled(scheduler: &TaskScheduler, id: TaskId) -> TaskRecord {
    scheduler.resume().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        scheduler.wait_for_task(id, &Context::default()),
    )
    .await
    .expect("tool timed out")
    .unwrap()
}
fn outcome(record: &TaskRecord) -> Option<&TaskOutcome> {
    match &record.state {
        TaskState::Terminal { outcome } => Some(outcome),
        _ => None,
    }
}
async fn message(scheduler: &TaskScheduler, record: &TaskRecord) -> JsonValue {
    let outcome = outcome(record).unwrap();
    let result = match outcome {
        TaskOutcome::Completed { result }
        | TaskOutcome::Failed {
            result: Some(result),
            ..
        }
        | TaskOutcome::Aborted {
            result: Some(result),
            ..
        } => result,
        other => panic!("missing tool receipt: {other:?}"),
    };
    let receipt: ToolTaskResult = serde_json::from_str(&result.to_string()).unwrap();
    let ToolTaskResult::Model { entry_id, .. } = receipt else {
        panic!("model receipt")
    };
    scheduler.session().commit(move |tx| async move { Ok(tx.entry(entry_id).await?.unwrap().model.unwrap().remove(0)) }, &Context::default()).await.unwrap()
}

#[tokio::test]
async fn validation_unavailability_and_hook_blocks_complete_without_executing() {
    for (name, args, block, code) in [
        ("missing", r#"{"n":1}"#, false, "tool_unavailable"),
        ("echo", r#"{"n":"bad"}"#, false, "invalid_arguments"),
        ("echo", r#"{"n":1}"#, true, "blocked"),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let tool = native(
            "echo",
            Arc::new(move |_, _, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(ToolExecutionResult::default()) })
            }),
        );
        let hooks = if block {
            vec![Arc::new(ToolHooks {
                before_tool: Some(Arc::new(|_, _, _| {
                    Box::pin(async {
                        Err(Error::host(
                            "HookError",
                            JsonString::from_utf16(vec![0xd800]),
                        ))
                    })
                })),
                ..Default::default()
            })]
        } else {
            vec![]
        };
        let (scheduler, conversation) = open(vec![tool], hooks, SchedulerOptions::default()).await;
        let id = start(&scheduler, conversation, name, json(args), None).await;
        let record = settled(&scheduler, id).await;
        assert!(
            matches!(outcome(&record), Some(TaskOutcome::Completed { .. })),
            "{record:?}"
        );
        let message = message(&scheduler, &record).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            message["content"][0]["text"]
                .as_string()
                .unwrap()
                .to_string_lossy()
                .contains(match code {
                    "tool_unavailable" => "not available",
                    "blocked" => "blocked",
                    _ => "Validation failed",
                })
        );
        if block {
            assert!(
                message["content"][0]["text"]
                    .as_string()
                    .unwrap()
                    .units()
                    .contains(&0xd800)
            );
        }
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn durable_intent_progress_and_after_hook_have_distinct_lifetimes() {
    let retained = Arc::new(Mutex::new(None::<ToolExecutionApi>));
    let held = retained.clone();
    let tool = native(
        "echo",
        Arc::new(move |args, api, context| {
            *held.lock() = Some(api.clone());
            Box::pin(async move {
                assert_eq!(args.value()["n"].as_f64(), Some(2.0));
                let record = api.get_task(api.task_id(), &context).await?.unwrap();
                assert_eq!(
                    record.state.checkpoint().unwrap()["phase"]
                        .to_utf8()
                        .as_deref(),
                    Some("execute")
                );
                api.output("prefix", None)?;
                api.details(json(r#"{"line":"\ud800","counter":1}"#), &context)
                    .await?;
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    let observed = Arc::new(AtomicUsize::new(0));
    let count = observed.clone();
    let held = retained.clone();
    let hook = ToolHooks {
        after_tool: Some(Arc::new(move |_, result, _, context| {
            let api = held.lock().clone().unwrap();
            let count = count.clone();
            Box::pin(async move {
                assert!(
                    api.output("late", None)
                        .unwrap_err()
                        .to_string()
                        .contains("has settled")
                );
                assert!(
                    api.details(JsonValue::Null, &context)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("has settled")
                );
                assert!(api.agent(&context).await.is_ok());
                count.fetch_add(1, Ordering::SeqCst);
                Ok(Some(result))
            })
        })),
        ..Default::default()
    };
    let (scheduler, conversation) = open(
        vec![tool],
        vec![Arc::new(hook)],
        SchedulerOptions::default(),
    )
    .await;
    let id = start(&scheduler, conversation, "echo", json(r#"{"n":"2"}"#), None).await;
    let record = settled(&scheduler, id).await;
    let message = message(&scheduler, &record).await;
    assert_eq!(message["content"][0]["text"], json(r#""prefix""#));
    assert_eq!(message["details"], json(r#"{"line":"\ud800","counter":1}"#));
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    let api = retained.lock().clone().unwrap();
    assert!(
        api.agent(&Context::default())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("ended")
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn safe_recovery_requires_both_policies_and_clears_durable_partial_progress() {
    for (stored, current, execute) in [
        (ReplayPolicy::Safe, ReplayPolicy::Safe, true),
        (ReplayPolicy::Safe, ReplayPolicy::Unsafe, false),
        (ReplayPolicy::Unsafe, ReplayPolicy::Safe, false),
        (ReplayPolicy::Unsafe, ReplayPolicy::Unsafe, false),
    ] {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let mut tool = native(
            "echo",
            Arc::new(move |_, api, context| {
                let calls = calls.clone();
                Box::pin(async move {
                    let live = api
                        .snapshot(&LIVE_DOC, api.conversation_id(), &context)
                        .await?
                        .unwrap();
                    assert!(live.tools.as_ref().unwrap()[0].progress.output.is_none());
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content("fresh".into())])),
                        ..Default::default()
                    })
                })
            }),
        );
        tool.replay = current;
        let (scheduler, conversation) = open(vec![tool], vec![], SchedulerOptions::default()).await;
        let id = start(
            &scheduler,
            conversation,
            "echo",
            json(r#"{"n":1}"#),
            Some(ToolTaskCheckpoint::Execute {
                arguments: object(r#"{"n":1}"#),
                replay: stored,
            }),
        )
        .await;
        scheduler
            .session()
            .commit(
                move |tx| async move {
                    tx.doc(&LIVE_DOC, conversation).await?.update(|live| {
                        live.tools.as_mut().unwrap()[0].progress.output =
                            Some("durable partial".into());
                        Ok(())
                    })
                },
                &Context::default(),
            )
            .await
            .unwrap();
        let record = settled(&scheduler, id).await;
        let message = message(&scheduler, &record).await;
        assert_eq!(count.load(Ordering::SeqCst), usize::from(execute));
        assert_eq!(
            message["content"][0]["text"],
            json(if execute {
                r#""fresh""#
            } else {
                r#""durable partial""#
            })
        );
        if !execute {
            assert!(matches!(
                outcome(&record),
                Some(TaskOutcome::Failed {
                    result: Some(_),
                    ..
                })
            ));
        }
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn nested_calls_reattach_by_key_ignore_argument_order_and_reject_changed_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let leaf = native(
        "leaf",
        Arc::new(move |args, api, context| {
            let count = count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                api.output(format!("nested-{}", args.value()["n"]), None)?;
                api.details(JsonValue::Null, &context).await?;
                Ok(ToolExecutionResult {
                    usage: Some(object(r#"{"count":1}"#)),
                    ..Default::default()
                })
            })
        }),
    );
    let parent = native(
        "parent",
        Arc::new(|_, api, context| {
            Box::pin(async move {
                let first = api
                    .execute_tool(
                        "leaf",
                        object(r#"{"n":2,"extra":1}"#),
                        &context,
                        NestedCallOptions {
                            key: Some("same".into()),
                            progress: Some(false),
                        },
                    )
                    .await?;
                let second = api
                    .execute_tool(
                        "leaf",
                        object(r#"{"extra":1,"n":2}"#),
                        &context,
                        NestedCallOptions {
                            key: Some("same".into()),
                            ..Default::default()
                        },
                    )
                    .await?;
                assert_eq!(first.task_id, second.task_id);
                assert_eq!(first.structured_output, Some(json(r#""nested-2""#)));
                assert_eq!(first.details, Some(JsonValue::Null));
                let error = api
                    .execute_tool(
                        "leaf",
                        object(r#"{"n":3}"#),
                        &context,
                        NestedCallOptions {
                            key: Some("same".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("already made"));
                let record = api
                    .get_task(
                        TypedTaskId::<ToolTaskResult>::from_untyped(first.task_id),
                        &context,
                    )
                    .await?
                    .unwrap();
                assert!(record.info.abandon_on_restart.is_some());
                Ok(ToolExecutionResult {
                    output: Some(Arc::new(vec![text_content("parent only".into())])),
                    ..Default::default()
                })
            })
        }),
    );
    let (scheduler, conversation) =
        open(vec![parent, leaf], vec![], SchedulerOptions::default()).await;
    let id = start(&scheduler, conversation, "parent", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        message(&scheduler, &record).await["content"][0]["text"],
        json(r#""parent only""#)
    );
    assert!(
        scheduler
            .session()
            .snapshot(&LIVE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .nested_tools
            .is_none()
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn nested_structured_output_is_checked_and_plain_output_is_bounded() {
    let mut schema = native(
        "schema",
        Arc::new(|_, _, _| {
            Box::pin(async {
                Ok(ToolExecutionResult {
                    structured_output: Some(json(r#"{"wrong":true}"#).into()),
                    ..Default::default()
                })
            })
        }),
    );
    schema.structured_output_schema = Some(json(
        r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#,
    ));
    let mut plain = native(
        "plain",
        Arc::new(|_, api, _| {
            Box::pin(async move {
                api.output("abcdef", None)?;
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    plain.output_limits = OutputLimits {
        max_bytes: 3,
        max_lines: 2,
        retain: Retain::Head,
    };
    let parent = native(
        "parent",
        Arc::new(|_, api, context| {
            Box::pin(async move {
                let schema = api
                    .execute_tool(
                        "schema",
                        object(r#"{"n":1}"#),
                        &context,
                        NestedCallOptions::default(),
                    )
                    .await?;
                assert!(schema.is_error);
                assert!(schema.structured_output.is_none());
                assert_eq!(
                    schema.diagnostics[0].code,
                    Some("invalid_structured_output".into())
                );
                let plain = api
                    .execute_tool(
                        "plain",
                        object(r#"{"n":1}"#),
                        &context,
                        NestedCallOptions::default(),
                    )
                    .await?;
                assert_eq!(plain.structured_output, Some(json(r#""abc""#)));
                assert_eq!(plain.diagnostics[0].code, Some("truncated".into()));
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    let (scheduler, conversation) = open(
        vec![parent, schema, plain],
        vec![],
        SchedulerOptions::default(),
    )
    .await;
    let id = start(&scheduler, conversation, "parent", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    assert!(
        matches!(outcome(&record), Some(TaskOutcome::Completed { .. })),
        "{record:?}"
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn execution_and_environment_errors_fail_with_lossless_diagnostics() {
    for environment in [false, true] {
        let error = Error::host("FixtureFailure", JsonString::from_utf16(vec![0xd800, 33]));
        let thrown = error.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let tool = native(
            "echo",
            Arc::new(move |_, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                let error = thrown.clone();
                Box::pin(async move { Err(error) })
            }),
        );
        let mut options = SchedulerOptions::default();
        if environment {
            options.env = Arc::new(move |_, _| {
                let error = error.clone();
                Box::pin(async move { Err(error) })
            });
        }
        let (scheduler, conversation) = open(vec![tool], vec![], options).await;
        let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
        let record = settled(&scheduler, id).await;
        assert!(
            matches!(
                outcome(&record),
                Some(TaskOutcome::Failed {
                    result: Some(_),
                    ..
                })
            ),
            "{record:?}"
        );
        let message = message(&scheduler, &record).await;
        assert!(
            message["content"][0]["text"]
                .as_string()
                .unwrap()
                .units()
                .contains(&0xd800)
        );
        assert_eq!(message["durationMs"].is_null(), environment);
        assert_eq!(count.load(Ordering::SeqCst), usize::from(!environment));
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn progress_preserves_native_error_identity_and_captures_before_mark_returns() {
    let error = Error::host("CustomProgressError", JsonString::from_utf16(vec![0xd800]));
    let failure = error.clone();
    let reported = Arc::new(Mutex::new(None));
    let capture = reported.clone();
    let done = Arc::new(Semaphore::new(0));
    let notify = done.clone();
    let captured = Arc::new(AtomicUsize::new(0));
    let count = captured.clone();
    let progress = ProgressWithError::<Error>::new(
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            let error = failure.clone();
            async move { Err(error) }
        },
        move |error| {
            *capture.lock() = Some(error);
            notify.add_permits(1);
        },
        0.0,
    );
    let wait = progress.mark_and_wait();
    assert_eq!(captured.load(Ordering::SeqCst), 1);
    let received = wait.await.unwrap().unwrap_err();
    assert!(received.same_error(&error));
    done.acquire().await.unwrap().forget();
    assert!(reported.lock().as_ref().unwrap().same_error(&error));
    assert!(progress.stop().await.is_empty());
}

#[tokio::test]
async fn settling_marks_all_leftover_nested_calls_before_waiting_for_either() {
    let started = Arc::new(Semaphore::new(0));
    let sibling_cancelled = Arc::new(Semaphore::new(0));
    let ended = Arc::new(AtomicUsize::new(0));
    let ready = started.clone();
    let sibling = sibling_cancelled.clone();
    let count = ended.clone();
    let leaf = native(
        "leaf",
        Arc::new(move |args, api, context| {
            let ready = ready.clone();
            let sibling = sibling.clone();
            let count = count.clone();
            Box::pin(async move {
                api.details(json(r#"{"durable":true}"#), &context).await?;
                ready.add_permits(1);
                api.signal().cancelled().await;
                if args.value()["n"].as_f64() == Some(1.0) {
                    sibling.acquire().await.unwrap().forget();
                } else {
                    sibling.add_permits(1);
                }
                count.fetch_add(1, Ordering::SeqCst);
                Err(Error::message("cancelled execute"))
            })
        }),
    );
    let parent = native(
        "parent",
        Arc::new(move |_, api, context| {
            let ready = started.clone();
            Box::pin(async move {
                drop(api.execute_tool(
                    "leaf",
                    object(r#"{"n":1}"#),
                    &context,
                    NestedCallOptions::default(),
                ));
                drop(api.execute_tool(
                    "leaf",
                    object(r#"{"n":2}"#),
                    &context,
                    NestedCallOptions::default(),
                ));
                ready.acquire_many(2).await.unwrap().forget();
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    let (scheduler, conversation) =
        open(vec![parent, leaf], vec![], SchedulerOptions::default()).await;
    let id = start(&scheduler, conversation, "parent", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    assert!(
        matches!(outcome(&record), Some(TaskOutcome::Completed { .. })),
        "{record:?}"
    );
    assert_eq!(ended.load(Ordering::SeqCst), 2);
    let tasks = scheduler
        .session()
        .commit(
            move |tx| async move {
                tx.scan_tasks(
                    TaskQuery {
                        conversation_id: Some(conversation),
                        ..Default::default()
                    },
                    20,
                    None,
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        tasks
            .items
            .iter()
            .filter(|task| matches!(
                task.state,
                TaskState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        result: Some(_),
                        ..
                    }
                }
            ))
            .count(),
        2
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelling_a_nested_wait_leaves_the_admitted_call_running() {
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let ready = started.clone();
    let gate = release.clone();
    let calls = count.clone();
    let leaf = native(
        "leaf",
        Arc::new(move |_, _, _| {
            let ready = ready.clone();
            let gate = gate.clone();
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                ready.add_permits(1);
                gate.acquire().await.unwrap().forget();
                Ok(ToolExecutionResult {
                    output: Some(Arc::new(vec![text_content("done".into())])),
                    ..Default::default()
                })
            })
        }),
    );
    let parent = native(
        "parent",
        Arc::new(move |_, api, context| {
            let ready = started.clone();
            let release = release.clone();
            Box::pin(async move {
                let controller = pi_chord::AbortController::new();
                let cancelled = pi_chord::context::with_abort_signal(controller.signal(), &context);
                let wait = api.execute_tool(
                    "leaf",
                    object(r#"{"n":1}"#),
                    &cancelled,
                    NestedCallOptions {
                        key: Some("one".into()),
                        ..Default::default()
                    },
                );
                ready.acquire().await.unwrap().forget();
                controller.abort();
                assert_eq!(wait.await.unwrap_err().name(), "AbortError");
                release.add_permits(1);
                let result = api
                    .execute_tool(
                        "leaf",
                        object(r#"{"n":1}"#),
                        &context,
                        NestedCallOptions {
                            key: Some("one".into()),
                            ..Default::default()
                        },
                    )
                    .await?;
                assert!(!result.is_error);
                assert_eq!(result.structured_output, Some(json(r#""done""#)));
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    let (scheduler, conversation) =
        open(vec![parent, leaf], vec![], SchedulerOptions::default()).await;
    let id = start(&scheduler, conversation, "parent", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    assert!(
        matches!(outcome(&record), Some(TaskOutcome::Completed { .. })),
        "{record:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn model_structured_output_breaks_report_as_extension_errors_even_without_an_owner() {
    for owned in [true, false] {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let capture = reports.clone();
        let options = SchedulerOptions {
            on_report: Arc::new(move |error, info, _| capture.lock().push((error, info))),
            ..Default::default()
        };
        let tool = native(
            "echo",
            Arc::new(|_, _, _| {
                Box::pin(async {
                    Ok(ToolExecutionResult {
                        output: Some(Arc::new(vec![text_content("visible".into())])),
                        structured_output: Some(JsonValue::Null.into()),
                        ..Default::default()
                    })
                })
            }),
        );
        let (scheduler, conversation) = if owned {
            open(vec![tool], vec![], options).await
        } else {
            let tool = Arc::new(tool);
            let mut options = options;
            options.agent = Arc::new(move |_| {
                let agent = pi_durable::harness::agent::Agent::<NativeTool, Section> {
                    model: None,
                    thinking_level: "off".into(),
                    extensions: vec![],
                    tools: vec![tool.clone()],
                    callable: vec![],
                    deferred: vec![],
                    sections: vec![],
                    instructions: None,
                    cwd: None,
                };
                Box::pin(async move { Ok(Arc::new(agent).into()) })
            });
            open(vec![], vec![], options).await
        };
        let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
        let record = settled(&scheduler, id).await;
        let message = message(&scheduler, &record).await;
        assert_eq!(message["content"][0]["text"], json(r#""visible""#));
        assert_eq!(message["isError"], JsonValue::Bool(false));
        assert_eq!(reports.lock().len(), 1);
        {
            let report = reports.lock();
            assert_eq!(report[0].1.kind, "extension_error");
            assert_eq!(report[0].1.extension, owned.then(|| "test-tools".into()));
            assert!(
                report[0]
                    .0
                    .to_string()
                    .contains("declares no structuredOutputSchema")
            );
        }

        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn abort_uses_durable_partial_output_and_discards_the_unflushed_buffer() {
    let ready = Arc::new(Semaphore::new(0));
    let started = ready.clone();
    let tool = native(
        "echo",
        Arc::new(move |_, api, context| {
            let started = started.clone();
            Box::pin(async move {
                api.output("durable", None)?;
                api.details(json(r#"{"saved":true}"#), &context).await?;
                api.output(" buffered", None)?;
                started.add_permits(1);
                api.signal().cancelled().await;
                Err(Error::message("execute interrupted"))
            })
        }),
    );
    let (scheduler, conversation) = open(vec![tool], vec![], SchedulerOptions::default()).await;
    let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
    scheduler.resume().unwrap();
    ready.acquire().await.unwrap().forget();
    scheduler.abort(id, &Context::default()).await.unwrap();
    let record = settled(&scheduler, id).await;
    assert!(
        matches!(
            outcome(&record),
            Some(TaskOutcome::Aborted {
                result: Some(_),
                ..
            })
        ),
        "{record:?}"
    );
    let message = message(&scheduler, &record).await;
    assert_eq!(message["content"][0]["text"], json(r#""durable""#));
    assert_eq!(message["details"], json(r#"{"saved":true}"#));
    assert_eq!(message["isError"], JsonValue::Bool(true));
    assert!(
        message["content"][1]["text"]
            .to_utf8()
            .unwrap()
            .contains("was aborted")
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn tail_window_policy_is_captured_before_building_the_environment() {
    use pi_durable::harness::settings::{HarnessSettings, resolve_settings};
    let settings = Arc::new(Mutex::new(resolve_settings(HarnessSettings::default())));
    settings.lock().progress.output_interval_ms = 17.0;
    let read = settings.clone();
    let changed = settings.clone();
    let options = SchedulerOptions {
        settings: Arc::new(move || Ok(read.lock().clone())),
        env: Arc::new(move |_, _| {
            changed.lock().progress.output_interval_ms = 999.0;
            Box::pin(async { Ok(None) })
        }),
        ..Default::default()
    };
    let mut tool = native(
        "echo",
        Arc::new(|_, api, _| {
            Box::pin(async move {
                assert_eq!(api.output_window.as_ref().unwrap().min_interval_ms, 17.0);
                assert_eq!(api.settings()?.progress.output_interval_ms, 999.0);
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    tool.output_limits.retain = Retain::Tail;
    let (scheduler, conversation) = open(vec![tool], vec![], options).await;
    let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    assert!(
        matches!(outcome(&record), Some(TaskOutcome::Completed { .. })),
        "{record:?}"
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn parallel_model_calls_begin_in_admission_order_when_services_are_ready() {
    for _ in 0..16 {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ready = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let tools = ["a", "b"]
            .into_iter()
            .map(|name| {
                let seen = seen.clone();
                let ready = ready.clone();
                let release = release.clone();
                native(
                    name,
                    Arc::new(move |_, _, _| {
                        seen.lock().push(name);
                        ready.add_permits(1);
                        let release = release.clone();
                        Box::pin(async move {
                            release.acquire().await.unwrap().forget();
                            Ok(ToolExecutionResult::default())
                        })
                    }),
                )
            })
            .collect();
        let (scheduler, conversation) = open(tools, vec![], SchedulerOptions::default()).await;
        let ids = scheduler
            .session()
            .commit(
                move |tx| async move {
                    let calls = ["a", "b"]
                        .into_iter()
                        .map(|name| ToolCall::new(name, name, json(r#"{"n":1}"#)))
                        .collect::<Vec<_>>();
                    let mut message = object(r#"{"role":"assistant"}"#);
                    message.insert("content".into(), pi_chord::json::to_value(&calls).unwrap());
                    let mut entry = EntryDraft::new("pi.assistant");
                    entry.model = Some(vec![message.into_value()]);
                    let entry = tx.append_entry(conversation, entry).await?;
                    let mut ids = Vec::new();
                    let mut slots = Vec::new();
                    for name in ["a", "b"] {
                        let mut options = TaskOptions::new(TaskOwnership::Conversation);
                        options.conversation_id = Some(conversation);
                        let id = tx
                            .create_task(&TOOL_TASK, ToolTaskInput::model(entry.id, name), options)
                            .await?
                            .untyped();
                        ids.push(id);
                        slots.push(ToolSlot {
                            progress: SlotProgress::new(name, name),
                            task_id: Some(id),
                            entry: None,
                        });
                    }
                    tx.doc(&LIVE_DOC, conversation).await?.update(|live| {
                        live.tools = Some(slots);
                        Ok(())
                    })?;
                    Ok(ids)
                },
                &Context::default(),
            )
            .await
            .unwrap();
        scheduler.resume().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), ready.acquire_many(2))
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(*seen.lock(), vec!["a", "b"]);
        release.add_permits(2);
        for id in ids {
            settled(&scheduler, id).await;
        }
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

struct FailingHookLookup(Error);
impl ToolHookProvider for FailingHookLookup {
    fn before_tool(&self) -> Result<Option<BeforeTool>> {
        Err(self.0.clone())
    }
    fn after_tool(&self) -> Result<Option<AfterTool>> {
        Ok(None)
    }
}
#[tokio::test]
async fn fallible_tool_hook_selection_faults_instead_of_becoming_a_block_decision() {
    let original = Error::host("LookupFailure", JsonString::from_utf16(vec![0xd800]));
    let reports = Arc::new(Mutex::new(Vec::new()));
    let capture = reports.clone();
    let hook = ToolHooks {
        provider: Some(Arc::new(FailingHookLookup(original.clone()))),
        ..Default::default()
    };
    let tool = native(
        "echo",
        Arc::new(|_, _, _| panic!("a lookup failure must not execute the tool")),
    );
    let (scheduler, conversation) = open(
        vec![tool],
        vec![Arc::new(hook)],
        SchedulerOptions {
            on_report: Arc::new(move |error, _, _| capture.lock().push(error)),
            ..Default::default()
        },
    )
    .await;
    let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    let Some(TaskOutcome::Faulted { error }) = outcome(&record) else {
        panic!("expected fault: {record:?}")
    };
    assert_eq!(error.message, original.message_text());
    assert!(
        reports
            .lock()
            .iter()
            .any(|error| error.same_error(&original))
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn erased_runtime_shares_typed_memos_and_retains_the_invocation_guards() {
    let retained = Arc::new(Mutex::new(None));
    let capture = retained.clone();
    let tool = native(
        "echo",
        Arc::new(move |_, api, context| {
            let runtime = api.erased();
            assert!(runtime.same_invocation(&api.erased()));
            *capture.lock() = Some(runtime.clone());
            Box::pin(async move {
                let value = json(r#"{"value":"\ud800"}"#);
                runtime.store_memo("wire", value.clone(), &context).await?;
                assert_eq!(api.memo::<JsonValue>("wire", &context).await?, Some(value));
                Ok(ToolExecutionResult::default())
            })
        }),
    );
    let (scheduler, conversation) = open(vec![tool], vec![], SchedulerOptions::default()).await;
    let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
    let record = settled(&scheduler, id).await;
    assert!(matches!(
        outcome(&record),
        Some(TaskOutcome::Completed { .. })
    ));
    let runtime = retained.lock().clone().unwrap();
    assert!(
        runtime
            .memo::<JsonValue>("wire", &Context::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("invocation has ended")
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

struct DeferredDetails {
    repaired: Arc<std::sync::atomic::AtomicBool>,
    copies: Arc<AtomicUsize>,
    error: Error,
}
impl HostedToolValue for DeferredDetails {
    fn copy_json(&self) -> Result<JsonValue> {
        self.copies.fetch_add(1, Ordering::SeqCst);
        if self.repaired.load(Ordering::SeqCst) {
            Ok(json(r#"{"repaired":"\ud800"}"#))
        } else {
            Err(self.error.clone())
        }
    }
}
#[tokio::test]
async fn hosted_details_are_copied_at_settlement_and_hooks_can_repair_them_first() {
    for repair in [false, true] {
        let repaired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let copies = Arc::new(AtomicUsize::new(0));
        let error = Error::host("JsonValueError", "Details contain a function".into());
        let value = ToolValue::Hosted(Arc::new(DeferredDetails {
            repaired: repaired.clone(),
            copies: copies.clone(),
            error,
        }));
        let tool = native(
            "echo",
            Arc::new(move |_, _, _| {
                let value = value.clone();
                Box::pin(async move {
                    Ok(ToolExecutionResult {
                        details: Some(value),
                        ..Default::default()
                    })
                })
            }),
        );
        let hooks = if repair {
            vec![Arc::new(ToolHooks {
                after_tool: Some(Arc::new(move |_, result, _, _| {
                    assert_eq!(copies.load(Ordering::SeqCst), 0);
                    assert!(
                        result
                            .details
                            .unwrap()
                            .hosted::<DeferredDetails>()
                            .is_some()
                    );
                    repaired.store(true, Ordering::SeqCst);
                    Box::pin(async { Ok(None) })
                })),
                ..Default::default()
            })]
        } else {
            vec![]
        };
        let (scheduler, conversation) = open(vec![tool], hooks, SchedulerOptions::default()).await;
        let id = start(&scheduler, conversation, "echo", json(r#"{"n":1}"#), None).await;
        let record = settled(&scheduler, id).await;
        if repair {
            assert!(matches!(
                outcome(&record),
                Some(TaskOutcome::Completed { .. })
            ));
            assert_eq!(
                message(&scheduler, &record).await["details"],
                json(r#"{"repaired":"\ud800"}"#)
            );
        } else {
            assert!(
                matches!(outcome(&record), Some(TaskOutcome::Faulted { .. })),
                "{record:?}"
            );
        }
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}
