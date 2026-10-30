#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_durable::{
    harness::{catalog::*, scheduler::*},
    *,
};
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

type JsonTask = Task<JsonValue, JsonValue, JsonValue>;
fn json(value: &str) -> JsonValue {
    pi_chord::json::parse(value).unwrap()
}
fn task(name: &str, run: PhaseHandler<JsonValue, JsonValue, JsonValue>) -> JsonTask {
    define_task(TaskDefinition {
        name: name.into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([("run".into(), run)]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    })
}
fn completed() -> Option<NextTaskState> {
    Some(NextTaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: JsonValue::Null,
        },
    })
}
async fn start(
    scheduler: &TaskScheduler,
    root: ConversationId,
    task: &JsonTask,
    input: JsonValue,
) -> TaskId {
    let task = task.clone();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
                Ok(tx.create_task(&task, input, options).await?.untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap()
}
async fn settled(scheduler: &TaskScheduler, id: TaskId) {
    let receipt = tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.wait_for_task(id, &Context::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(
            receipt.state,
            TaskState::Terminal {
                outcome: TaskOutcome::Completed { .. }
            }
        ),
        "{receipt:?}"
    );
}
async fn open(
    storage: Arc<support::ControlledStorage>,
    tasks: &[JsonTask],
    options: SchedulerOptions,
    session_options: SessionOptions,
) -> (TaskScheduler, ConversationId) {
    let catalog = TaskCatalog::default();
    catalog
        .replace(tasks.iter().map(|task| InstalledTask {
            task: task.erased(),
            extension: None,
        }))
        .unwrap();
    let scheduler = TaskScheduler::open(
        storage,
        catalog,
        options,
        session_options,
        &Context::default(),
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            |tx| async move {
                let root = tx.create_root_conversation().await?.id;
                for index in 0..21 {
                    let mut draft = EntryDraft::new("message");
                    draft.model = Some(vec![json(&format!(
                        r#"{{"role":"user","content":"{index}","timestamp":0}}"#
                    ))]);
                    tx.append_entry(root, draft).await?;
                }
                Ok(root)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    (scheduler, root)
}

#[tokio::test]
async fn native_scheduler_keeps_busy_ranges_and_applies_idle_retention_across_tasks() {
    for retention in [0.0, 1_000.0, f64::INFINITY] {
        let storage = Arc::new(support::ControlledStorage::default());
        let now = Arc::new(Mutex::new(1_000.0));
        let rows = Arc::new(Mutex::new(HashMap::new()));
        let counts = storage.entry_rows.clone();
        let seen = rows.clone();
        let probe = task(
            "probe",
            Arc::new(move |task, runtime, context| {
                let counts = counts.clone();
                let seen = seen.clone();
                Box::pin(async move {
                    let mut reads = Vec::new();
                    for _ in 0..2 {
                        let before = counts.load(Ordering::SeqCst);
                        runtime
                            .context(runtime.conversation_id(), None, &context)
                            .await?;
                        reads.push(counts.load(Ordering::SeqCst) - before);
                    }
                    seen.lock()
                        .insert(task.record().info.input.to_utf8().unwrap(), reads);
                    runtime
                        .commit(|_, _| async { Ok(completed()) }, &context)
                        .await
                })
            }),
        );
        let clock = now.clone();
        let (scheduler, root) = open(
            storage,
            std::slice::from_ref(&probe),
            SchedulerOptions {
                settings: Arc::new(move || {
                    Ok(pi_durable::harness::settings::resolve_settings(
                        pi_durable::harness::settings::HarnessSettings {
                            context_retention_ms: Some(retention),
                            ..Default::default()
                        },
                    ))
                }),
                ..Default::default()
            },
            SessionOptions {
                now: Arc::new(move || *clock.lock()),
                ..Default::default()
            },
        )
        .await;
        for label in ["first", "second"] {
            let id = start(&scheduler, root, &probe, JsonValue::from(label)).await;
            settled(&scheduler, id).await;
            scheduler
                .wait_for_idle(Some(root), &Context::default())
                .await
                .unwrap();
        }
        assert_eq!(rows.lock()["first"], [22, 1]);
        assert_eq!(
            rows.lock()["second"],
            if retention > 0.0 {
                vec![1, 1]
            } else {
                vec![22, 1]
            }
        );
        assert_eq!(scheduler.retained_contexts(), usize::from(retention > 0.0));
        *now.lock() = 2_000.0;
        let id = start(&scheduler, root, &probe, JsonValue::from("expired")).await;
        settled(&scheduler, id).await;
        assert_eq!(
            rows.lock()["expired"],
            if retention.is_infinite() {
                vec![1, 1]
            } else {
                vec![22, 1]
            }
        );
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
        assert_eq!(scheduler.retained_contexts(), 0);
    }
}

#[tokio::test]
async fn a_late_context_read_cannot_seed_the_next_invocation_cache() {
    let storage = Arc::new(support::ControlledStorage::default());
    let scan_gate = Arc::new(support::Gate::default());
    let captured = Arc::new(Mutex::new(None::<TaskRuntime>));
    let pending = Arc::new(Mutex::new(None));
    let (held, waiting, scans) = (captured.clone(), pending.clone(), scan_gate.clone());
    let late = task(
        "late",
        Arc::new(move |_, runtime, context| {
            *held.lock() = Some(runtime.clone());
            let scans = scans.clone();
            let waiting = waiting.clone();
            Box::pin(async move {
                // A caller context independent of the invocation keeps the scan alive.
                let read = runtime.context(runtime.conversation_id(), None, &Context::default());
                *waiting.lock() = Some(tokio::spawn(read));
                scans.entered().await;
                runtime
                    .commit(|_, _| async { Ok(completed()) }, &context)
                    .await
            })
        }),
    );
    let proceed = Arc::new(tokio::sync::Notify::new());
    let ready = Arc::new(tokio::sync::Notify::new());
    let rows = Arc::new(Mutex::new(None));
    let (go, entered, seen, counts) = (
        proceed.clone(),
        ready.clone(),
        rows.clone(),
        storage.entry_rows.clone(),
    );
    let next = task(
        "next",
        Arc::new(move |_, runtime, context| {
            let (go, entered, seen, counts) =
                (go.clone(), entered.clone(), seen.clone(), counts.clone());
            Box::pin(async move {
                entered.notify_one();
                go.notified().await;
                let before = counts.load(Ordering::SeqCst);
                runtime
                    .context(runtime.conversation_id(), None, &context)
                    .await?;
                *seen.lock() = Some(counts.load(Ordering::SeqCst) - before);
                runtime
                    .commit(|_, _| async { Ok(completed()) }, &context)
                    .await
            })
        }),
    );
    let (scheduler, root) = open(
        storage.clone(),
        &[late.clone(), next.clone()],
        SchedulerOptions {
            settings: Arc::new(|| {
                Ok(pi_durable::harness::settings::resolve_settings(
                    pi_durable::harness::settings::HarnessSettings {
                        context_retention_ms: Some(0.0),
                        ..Default::default()
                    },
                ))
            }),
            ..Default::default()
        },
        Default::default(),
    )
    .await;
    *storage.range_gate.lock() = Some(scan_gate.clone());
    let id = start(&scheduler, root, &late, JsonValue::Null).await;
    settled(&scheduler, id).await;
    let signal = captured.lock().as_ref().unwrap().signal();
    signal.cancelled().await;
    let next_id = start(&scheduler, root, &next, JsonValue::Null).await;
    ready.notified().await;
    scan_gate.release();
    let read = pending.lock().take().unwrap();
    let read = read.await.unwrap().unwrap();
    assert_eq!(read.entries.len(), 21);
    assert_eq!(scheduler.retained_contexts(), 0);
    proceed.notify_one();
    settled(&scheduler, next_id).await;
    assert_eq!(*rows.lock(), Some(22));
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_background_timer_expires_idle_native_contexts_without_new_commits() {
    let storage = Arc::new(support::ControlledStorage::default());
    let now = Arc::new(Mutex::new(1_000.0));
    let timers = Arc::new(Mutex::new(Vec::new()));
    let probe = task(
        "probe",
        Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .context(runtime.conversation_id(), None, &context)
                    .await?;
                runtime
                    .commit(|_, _| async { Ok(completed()) }, &context)
                    .await
            })
        }),
    );
    let (clock, pending) = (now.clone(), timers.clone());
    let (scheduler, root) = open(
        storage,
        std::slice::from_ref(&probe),
        SchedulerOptions {
            settings: Arc::new(|| {
                Ok(pi_durable::harness::settings::resolve_settings(
                    pi_durable::harness::settings::HarnessSettings {
                        context_retention_ms: Some(1_000.0),
                        ..Default::default()
                    },
                ))
            }),
            ..Default::default()
        },
        SessionOptions {
            now: Arc::new(move || *clock.lock()),
            set_timer: Some(Arc::new(move |callback, _, background| {
                assert!(background);
                pending.lock().push(callback);
                Some(Box::new(|| {}))
            })),
            ..Default::default()
        },
    )
    .await;
    let id = start(&scheduler, root, &probe, JsonValue::Null).await;
    settled(&scheduler, id).await;
    assert_eq!(scheduler.retained_contexts(), 1);
    assert_eq!(timers.lock().len(), 1);
    *now.lock() = 2_000.0;
    let expire = timers.lock().remove(0);
    expire();
    assert_eq!(scheduler.retained_contexts(), 0);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_a_context_waiter_keeps_the_admitted_range_read_running() {
    let storage = Arc::new(support::ControlledStorage::default());
    let gate = Arc::new(support::Gate::default());
    let held = gate.clone();
    let captured = Arc::new(Mutex::new(None::<TaskRuntime>));
    let save = captured.clone();
    let detached = task(
        "detached",
        Arc::new(move |_, runtime, context| {
            let held = held.clone();
            *save.lock() = Some(runtime.clone());
            Box::pin(async move {
                drop(runtime.context(runtime.conversation_id(), None, &Context::default()));
                held.entered().await;
                runtime
                    .commit(|_, _| async { Ok(completed()) }, &context)
                    .await
            })
        }),
    );
    let (scheduler, root) = open(
        storage.clone(),
        std::slice::from_ref(&detached),
        Default::default(),
        Default::default(),
    )
    .await;
    *storage.range_gate.lock() = Some(gate.clone());
    let id = start(&scheduler, root, &detached, JsonValue::Null).await;
    settled(&scheduler, id).await;
    let ended = captured.lock().as_ref().unwrap().signal();
    ended.cancelled().await;
    gate.release();
    for _ in 0..100 {
        if storage.entry_rows.load(Ordering::SeqCst) == 22 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(storage.entry_rows.load(Ordering::SeqCst), 22);
    assert_eq!(scheduler.retained_contexts(), 0);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_a_watch_waiter_still_binds_its_observer_to_the_invocation() {
    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Notes {
        text: String,
    }
    impl DocumentState for Notes {}
    let notes = define_doc(DocDefinition::new(
        "test.notes",
        1,
        SessionScope,
        || {
            Ok(Notes {
                text: String::new(),
            })
        },
        |_, _, _| false,
    ))
    .unwrap();
    let token = notes.clone();
    let captured = Arc::new(Mutex::new(None::<TaskRuntime>));
    let save = captured.clone();
    let detached = task(
        "detached-watch",
        Arc::new(move |_, runtime, context| {
            let token = token.clone();
            *save.lock() = Some(runtime.clone());
            Box::pin(async move {
                drop(runtime.watch_doc(&token, (), &context));
                runtime
                    .commit(|_, _| async { Ok(completed()) }, &context)
                    .await
            })
        }),
    );
    let (scheduler, root) = open(
        Arc::new(support::ControlledStorage::default()),
        std::slice::from_ref(&detached),
        Default::default(),
        Default::default(),
    )
    .await;
    scheduler
        .session()
        .commit(
            move |tx| async move {
                tx.doc(&notes, ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let id = start(&scheduler, root, &detached, JsonValue::Null).await;
    settled(&scheduler, id).await;
    let ended = captured.lock().as_ref().unwrap().signal();
    ended.cancelled().await;
    scheduler
        .session()
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
    assert_eq!(scheduler.session().cache_sizes().attached, 0);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
