use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::{
    AbortController,
    context::{AbortError, AbortReason as SignalReason, with_abort_signal},
};
use pi_durable::{
    harness::{catalog::*, runtime::TaskRuntime, scheduler::*, state::*, util::Waiters},
    *,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn json(value: &str) -> JsonValue {
    pi_chord::json::parse(value).unwrap()
}
type JsonTask = Task<JsonValue, JsonValue, JsonValue>;
fn one_step(name: &str, handler: PhaseHandler<JsonValue, JsonValue, JsonValue>) -> JsonTask {
    define_task(TaskDefinition {
        name: name.into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([("run".into(), handler)]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Aborted {
                                    reason: Some("test".into()),
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
fn complete(result: JsonValue) -> Option<NextTaskState> {
    Some(NextTaskState::Terminal {
        outcome: TaskOutcome::Completed { result },
    })
}
fn catalog(tasks: &[JsonTask]) -> TaskCatalog {
    let catalog = TaskCatalog::default();
    catalog
        .replace(tasks.iter().map(|task| InstalledTask {
            task: task.erased(),
            extension: Some("tests".into()),
        }))
        .unwrap();
    catalog
}
async fn open(tasks: &[JsonTask]) -> (TaskScheduler, ConversationId) {
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog(tasks),
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    (scheduler, root)
}
async fn start(scheduler: &TaskScheduler, conversation: ConversationId, task: &JsonTask) -> TaskId {
    let task = task.clone();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                Ok(tx
                    .create_task(&task, JsonValue::Null, options)
                    .await?
                    .untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap()
}
async fn receipt(scheduler: &TaskScheduler, id: TaskId) -> TaskRecord {
    tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.wait_for_task(id, &Context::default()),
    )
    .await
    .expect("task did not settle")
    .unwrap()
}
async fn record(scheduler: &TaskScheduler, id: TaskId) -> TaskRecord {
    scheduler
        .session()
        .commit(
            move |tx| async move { Ok(tx.task(id).await?.unwrap()) },
            &Context::default(),
        )
        .await
        .unwrap()
}
async fn close(scheduler: &TaskScheduler) {
    tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.session().close(&Context::default()),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_admission_before_scheduler_notification_does_not_fail_the_session() {
    #[derive(Clone, Serialize, Deserialize)]
    struct Counter {
        count: u32,
    }
    impl DocumentState for Counter {}
    let document = define_doc(DocDefinition::new(
        "close-window",
        1,
        SessionScope,
        || Ok(Counter { count: 0 }),
        |_, _, _| false,
    ))
    .unwrap();
    let (entered, reached) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let (release, released) = std::sync::mpsc::channel();
    let released = Arc::new(Mutex::new(released));
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog(&[]),
        SchedulerOptions::default(),
        SessionOptions {
            set_timer: Some(Arc::new(move |_, _, background| {
                assert!(background);
                let (entered, released) = (entered.clone(), released.clone());
                Some(Box::new(move || {
                    if std::thread::current().name() != Some("durable-close-probe") {
                        return;
                    }
                    if let Some(entered) = entered.lock().take() {
                        let _ = entered.send(());
                        released.lock().recv().unwrap();
                    }
                }))
            })),
            ..Default::default()
        },
        &Context::default(),
    )
    .await
    .unwrap();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                tx.doc(&document, ()).await?.update(|counter| {
                    counter.count += 1;
                    Ok(())
                })
            },
            &Context::default(),
        )
        .await
        .unwrap();
    // Startup reconciliation can still sweep and replace the timer after the
    // document commit settles. Hold a read job on the mutation line so close
    // disarms an already installed timer, rather than racing its registration.
    let (line_entered, line_ready) = tokio::sync::oneshot::channel();
    let (line_release, line_released) = tokio::sync::oneshot::channel();
    let held_line = scheduler.session().read_on_line(move || async move {
        line_entered.send(()).unwrap();
        line_released.await.unwrap();
        Ok(())
    });
    tokio::time::timeout(Duration::from_secs(3), line_ready)
        .await
        .unwrap()
        .unwrap();
    let session = scheduler.session();
    let runtime = tokio::runtime::Handle::current();
    let closing = std::thread::Builder::new()
        .name("durable-close-probe".into())
        .spawn(move || {
            let _entered = runtime.enter();
            session.close(&Context::default())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), reached)
        .await
        .unwrap()
        .unwrap();
    // Session close disarms its cache timer after sealing admission and before
    // notifying the scheduler. Drive a reservation in that exact window.
    scheduler.resume().unwrap();
    let (polled, finished) = tokio::sync::oneshot::channel();
    scheduler.session().spawn(async move {
        let _ = polled.send(());
    });
    finished.await.unwrap();
    let failure = scheduler.session().failure();
    release.send(()).unwrap();
    line_release.send(()).unwrap();
    held_line.await.unwrap();
    closing.join().unwrap().await.unwrap();
    assert!(
        failure.is_none(),
        "ordinary close became a failure: {failure:?}"
    );
    assert!(matches!(
        scheduler.session().closed().await,
        SessionEnd::Closed
    ));
}

#[tokio::test]
async fn typed_task_progress_keeps_one_invocation_and_returns_a_typed_result() {
    #[derive(Clone, Serialize, Deserialize)]
    struct Input {
        to: u32,
    }
    #[derive(Clone, Serialize, Deserialize)]
    struct Checkpoint {
        phase: String,
        n: u32,
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let first_runtime = Arc::new(Mutex::new(None::<TaskRuntime<Input, Checkpoint, u32>>));
    let (trace, identity) = (seen.clone(), first_runtime.clone());
    let task = define_task(TaskDefinition {
        name: "counter".into(),
        version: 1,
        initial: Arc::new(|_: &Input| {
            Ok(Checkpoint {
                phase: "count".into(),
                n: 0,
            })
        }),
        phases: IndexMap::from([(
            "count".into(),
            Arc::new(
                move |task: RunningTask<Input, Checkpoint, u32>,
                      runtime: TaskRuntime<Input, Checkpoint, u32>,
                      context| {
                    trace.lock().push(task.checkpoint().n);
                    let mut first = identity.lock();
                    if let Some(first) = first.as_ref() {
                        assert!(runtime.same_invocation(first));
                    } else {
                        *first = Some(runtime.clone());
                    }
                    Box::pin(async move {
                        runtime
                            .commit(
                                move |_, current| async move {
                                    let n = current.checkpoint().n + 1;
                                    Ok(Some(if n == current.record().info.input.to {
                                        NextTaskState::Terminal {
                                            outcome: TaskOutcome::Completed { result: n },
                                        }
                                    } else {
                                        NextTaskState::Running {
                                            checkpoint: Checkpoint {
                                                phase: "count".into(),
                                                n,
                                            },
                                        }
                                    }))
                                },
                                &context,
                            )
                            .await
                    }) as pi_durable::storage::StorageFuture<()>
                },
            ) as PhaseHandler<Input, Checkpoint, u32>,
        )]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let catalog = TaskCatalog::default();
    catalog
        .replace([InstalledTask {
            task: task.erased(),
            extension: None,
        }])
        .unwrap();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog,
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let conversation = tx.create_root_conversation().await?.id;
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                tx.create_task(&task, Input { to: 3 }, options).await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    assert_eq!(
        receipt(&scheduler, id.untyped()).await.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { result: json("3") }
        }
    );
    assert_eq!(*seen.lock(), [0, 1, 2]);
    let stale = first_runtime.lock().take().unwrap();
    // A terminal receipt is published before the handler and decision finish.
    // Wait for invocation cleanup before asserting the post-invocation error.
    tokio::time::timeout(Duration::from_secs(3), stale.signal().cancelled())
        .await
        .unwrap();
    assert!(
        stale
            .commit(|_, _| async { Ok(None) }, &Context::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("invocation has ended")
    );
    close(&scheduler).await;
}

#[tokio::test]
async fn no_progress_and_handler_errors_fault_but_a_committed_outcome_wins() {
    let idle = one_step("idle", Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
    let throws = one_step(
        "throws",
        Arc::new(|_, _, _| Box::pin(async { Err(Error::message("boom")) })),
    );
    let done = one_step(
        "done",
        Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(|_, _| async { Ok(complete(json(r#""ok""#))) }, &context)
                    .await?;
                assert!(
                    runtime
                        .commit(|_, _| async { Ok(None) }, &context)
                        .await
                        .is_err()
                );
                Err(Error::message("after terminal"))
            })
        }),
    );
    let (scheduler, root) = open(&[idle.clone(), throws.clone(), done.clone()]).await;
    let ids = [
        start(&scheduler, root, &idle).await,
        start(&scheduler, root, &throws).await,
        start(&scheduler, root, &done).await,
    ];
    scheduler.resume().unwrap();
    let receipts =
        futures::future::join_all(ids.into_iter().map(|id| receipt(&scheduler, id))).await;
    let messages = receipts[..2]
        .iter()
        .map(|record| match &record.state {
            TaskState::Terminal {
                outcome: TaskOutcome::Faulted { error },
            } => error.message.to_string(),
            state => panic!("unexpected {state:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "Task idle phase run returned without durable progress",
            "boom"
        ]
    );
    assert_eq!(
        receipts[2].state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: json(r#""ok""#)
            }
        }
    );
    close(&scheduler).await;
}

#[tokio::test]
async fn reentrant_close_during_phase_snapshot_prevents_the_next_handler() {
    use pi_durable::harness::reader::{ReaderSubscription, RegistryReader, RegistryView};
    use std::sync::atomic::AtomicBool;
    struct ClosingReader {
        catalog: TaskCatalog,
        close_next: Arc<AtomicBool>,
        session: Arc<Mutex<Option<Session>>>,
    }
    impl RegistryReader for ClosingReader {
        fn snapshot(&self) -> Result<RegistryView> {
            if self.close_next.swap(false, Ordering::SeqCst) {
                // close() seals admission synchronously, even while this read
                // runs inside the scheduler's Session-line decision.
                drop(
                    self.session
                        .lock()
                        .as_ref()
                        .unwrap()
                        .close(&Context::default()),
                );
            }
            Ok(RegistryView::from_catalog(self.catalog.snapshot()))
        }
        fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Result<ReaderSubscription> {
            RegistryReader::subscribe(&self.catalog, listener)
        }
    }
    let close_next = Arc::new(AtomicBool::new(false));
    let session = Arc::new(Mutex::new(None));
    let phases = Arc::new(Mutex::new(Vec::new()));
    let (first, flag, second) = (phases.clone(), close_next.clone(), phases.clone());
    let task: JsonTask = define_task(TaskDefinition {
        name: "close-in-snapshot".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"one"}"#))),
        phases: IndexMap::from([
            (
                "one".into(),
                Arc::new(
                    move |_,
                          runtime: TaskRuntime<JsonValue, JsonValue, JsonValue>,
                          context: Context| {
                        let (phases, flag) = (first.clone(), flag.clone());
                        Box::pin(async move {
                            phases.lock().push("one");
                            runtime
                                .commit(
                                    |_, _| async {
                                        Ok(Some(NextTaskState::Running {
                                            checkpoint: json(r#"{"phase":"two"}"#),
                                        }))
                                    },
                                    &context,
                                )
                                .await?;
                            flag.store(true, Ordering::SeqCst);
                            Ok(())
                        }) as pi_durable::storage::StorageFuture<()>
                    },
                ) as PhaseHandler<JsonValue, JsonValue, JsonValue>,
            ),
            (
                "two".into(),
                Arc::new(move |_, _, _| {
                    second.lock().push("two");
                    Box::pin(async { Ok(()) })
                }),
            ),
        ]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::default()),
        ClosingReader {
            catalog: catalog(std::slice::from_ref(&task)),
            close_next,
            session: session.clone(),
        },
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    *session.lock() = Some(scheduler.session().clone());
    let root = scheduler
        .session()
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    start(&scheduler, root, &task).await;
    scheduler.resume().unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), scheduler.session().closed())
            .await
            .unwrap(),
        SessionEnd::Closed
    ));
    assert_eq!(*phases.lock(), ["one"]);
}

#[tokio::test]
async fn a_terminal_commit_holds_for_ordinary_owned_work() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let (wait, ready) = (gate.clone(), entered.clone());
    let child = one_step(
        "child",
        Arc::new(move |_, runtime, context| {
            let (wait, ready) = (wait.clone(), ready.clone());
            Box::pin(async move {
                ready.notify_one();
                wait.notified().await;
                runtime
                    .commit(|_, _| async { Ok(complete(JsonValue::Null)) }, &context)
                    .await
            })
        }),
    );
    let create = child.clone();
    let parent = one_step(
        "parent",
        Arc::new(move |task, runtime, context| {
            let child = create.clone();
            Box::pin(async move {
                if crate::json(r#"{"phase":"run"}"#) == *task.checkpoint() {
                    runtime
                        .commit(
                            move |tx, _| async move {
                                tx.create_task(
                                    &child,
                                    JsonValue::Null,
                                    TaskOptions::new(TaskOwnership::Task(task.record().id())),
                                )
                                .await?;
                                Ok(Some(NextTaskState::Running {
                                    checkpoint: json(r#"{"phase":"run","created":true}"#),
                                }))
                            },
                            &context,
                        )
                        .await
                } else {
                    runtime
                        .commit(|_, _| async { Ok(complete(json("42"))) }, &context)
                        .await
                }
            })
        }),
    );
    let (scheduler, root) = open(&[parent.clone(), child]).await;
    let id = start(&scheduler, root, &parent).await;
    scheduler.resume().unwrap();
    if tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .is_err()
    {
        panic!(
            "child never entered; parent={:?}; failure={:?}; indexes={:?}",
            record(&scheduler, id).await,
            scheduler.session().failure(),
            scheduler.index_sizes()
        );
    }
    assert!(matches!(
        record(&scheduler, id).await.state,
        TaskState::Completing { .. }
    ));
    assert!(!scheduler.index_sizes().live.eq(&0));
    gate.notify_one();
    assert_eq!(
        receipt(&scheduler, id).await.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { result: json("42") }
        }
    );
    scheduler
        .wait_for_idle(Some(root), &Context::default())
        .await
        .unwrap();
    // Sweep follows the publication that releases the last live descendant.
    scheduler
        .session()
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
    assert_eq!(scheduler.index_sizes().live, 0);
    assert_eq!(scheduler.index_sizes().settled, 0);
    close(&scheduler).await;
}

#[tokio::test]
async fn durable_memos_keep_the_first_winner_and_handle_prototype_property_names() {
    let task = one_step(
        "memo",
        Arc::new(|_, runtime, context| {
            Box::pin(async move {
                for name in ["__proto__", "toString", "constructor"] {
                    assert_eq!(runtime.memo::<u32>(name, &context).await?, None);
                    assert_eq!(runtime.store_memo(name, 7u32, &context).await?, 7);
                    assert_eq!(runtime.store_memo(name, 9u32, &context).await?, 7);
                }
                runtime
                    .commit(|_, _| async { Ok(complete(JsonValue::Null)) }, &context)
                    .await
            })
        }),
    );
    let (scheduler, root) = open(std::slice::from_ref(&task)).await;
    let id = start(&scheduler, root, &task).await;
    scheduler.resume().unwrap();
    let result = receipt(&scheduler, id).await;
    assert!(matches!(
        result.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    assert!(result.state.memos().is_none());
    close(&scheduler).await;
}

#[tokio::test]
async fn sleep_releases_the_invocation_and_rechecks_an_early_host_timer() {
    let now = Arc::new(Mutex::new(10.0));
    let timers = Arc::new(Mutex::new(Vec::<pi_durable::timers::TimerCallback>::new()));
    let runs = Arc::new(AtomicUsize::new(0));
    let calls = runs.clone();
    let task = one_step(
        "sleep",
        Arc::new(move |_, runtime, context| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    runtime
                        .sleep_until(20.5, json(r#"{"phase":"run"}"#), &context)
                        .await
                } else {
                    runtime
                        .commit(|_, _| async { Ok(complete(JsonValue::Null)) }, &context)
                        .await
                }
            })
        }),
    );
    let (clock, pending) = (now.clone(), timers.clone());
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog(std::slice::from_ref(&task)),
        SchedulerOptions::default(),
        SessionOptions {
            now: Arc::new(move || *clock.lock()),
            set_timer: Some(Arc::new(move |callback, _, background| {
                if !background {
                    pending.lock().push(callback);
                }
                Some(Box::new(|| {}))
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
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let id = start(&scheduler, root, &task).await;
    scheduler.resume().unwrap();
    for _ in 0..100 {
        if !timers.lock().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        record(&scheduler, id).await.state,
        TaskState::Sleeping { until: 20.5, .. }
    ));
    let callback = timers.lock().remove(0);
    callback();
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    *now.lock() = 20.5;
    scheduler.resume().unwrap();
    receipt(&scheduler, id).await;
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    close(&scheduler).await;
}

#[tokio::test]
async fn cancellation_signals_the_run_then_reserves_a_fresh_abort_invocation() {
    let seen = Arc::new(tokio::sync::Notify::new());
    let signal = seen.clone();
    let task = one_step(
        "abort",
        Arc::new(move |_, runtime, _| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
                runtime.signal().cancelled().await;
                Err(Error::message("run cancelled"))
            })
        }),
    );
    let (scheduler, root) = open(std::slice::from_ref(&task)).await;
    let id = start(&scheduler, root, &task).await;
    scheduler.resume().unwrap();
    seen.notified().await;
    tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.abort(id, &Context::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        receipt(&scheduler, id).await.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted { .. }
        }
    ));
    close(&scheduler).await;
}

#[tokio::test]
async fn waiter_settlement_order_is_independent_of_when_tokio_polls_it() {
    let waits = Waiters::<u32, u32>::default();
    let controller = AbortController::new();
    let context = with_abort_signal(controller.signal(), &Context::default());
    let first = waits.add(1, &context);
    waits.resolve(&1, 12);
    controller.abort();
    assert_eq!(first.await.unwrap(), 12);
    let controller = AbortController::new();
    let context = with_abort_signal(controller.signal(), &Context::default());
    let cancelled = waits.add(1, &context);
    controller.abort_with_reason(SignalReason::Error(AbortError::new("caller stopped")));
    waits.resolve(&1, 13);
    assert_eq!(cancelled.await.unwrap_err().to_string(), "caller stopped");
    let controller = AbortController::new();
    let context = with_abort_signal(controller.signal(), &Context::default());
    let old = waits.add(1, &context);
    controller.abort();
    assert!(waits.keys().is_empty());
    let new = waits.add(1, &Context::default());
    assert!(old.await.is_err());
    assert_eq!(waits.keys(), [1]);
    waits.resolve(&1, 14);
    assert_eq!(new.await.unwrap(), 14);
    let failure = Error::message("storage failed");
    let pending = waits.add(2, &Context::default());
    waits.reject_all(failure.clone());
    assert!(pending.await.unwrap_err().same_error(&failure));
}

async fn seed(
    storage: Arc<dyn Storage>,
    kind: &str,
    running: bool,
    restarts: Option<u64>,
    abandon: bool,
) -> TaskId {
    let session = Session::new(storage, SessionOptions::default());
    let kind = JsonString::from(kind);
    session
        .commit(
            move |tx| async move {
                let conversation = tx.create_root_conversation().await?.id;
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                options.abandon_on_restart = abandon;
                let id = tx
                    .create_task_record(
                        kind,
                        1,
                        JsonValue::Null,
                        |_| Ok(json(r#"{"phase":"run"}"#)),
                        options,
                    )
                    .await?;
                let mut record = tx
                    .staged_tasks()
                    .into_iter()
                    .find(|record| record.id() == id)
                    .unwrap();
                if running {
                    record.state = TaskState::Running {
                        checkpoint: json(r#"{"phase":"run"}"#),
                        memos: None,
                    };
                }
                record.info.restarts = restarts;
                tx.set_task(record)?;
                Ok(id)
            },
            &Context::default(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn recovery_counts_crashes_parks_at_the_limit_and_retry_resets_the_count() {
    let storage = Arc::new(MemoryStorage::new());
    let id = seed(storage.clone(), "restart", true, Some(1), false).await;
    let reports = Arc::new(Mutex::new(Vec::new()));
    let report = reports.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let task = one_step(
        "restart",
        Arc::new(move |_, runtime, context| {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                runtime
                    .commit(|_, _| async { Ok(complete(JsonValue::Null)) }, &context)
                    .await
            })
        }),
    );
    let scheduler = TaskScheduler::open(
        storage,
        catalog(&[task]),
        SchedulerOptions {
            on_report: Arc::new(move |error, info, _| report.lock().push((error, info))),
            settings: Arc::new(|| {
                Ok(pi_durable::harness::settings::resolve_settings(
                    pi_durable::harness::settings::HarnessSettings {
                        restart_limit: Some(2.0),
                        ..Default::default()
                    },
                ))
            }),
            ..Default::default()
        },
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let recovered = record(&scheduler, id).await;
    assert_eq!(recovered.state.status(), TaskStatus::Pending);
    assert_eq!(recovered.info.restarts, Some(2));
    assert_eq!(reports.lock().len(), 1);
    assert_eq!(reports.lock()[0].1.kind, "task_parked");
    assert_eq!(reports.lock()[0].1.task.as_ref().unwrap().id, id);
    assert_eq!(
        scheduler.blocked_reason(&recovered),
        Some(BlockedReason::RestartLimit)
    );
    scheduler.resume().unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    scheduler.retry(id, &Context::default()).await.unwrap();
    assert!(matches!(
        receipt(&scheduler, id).await.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    close(&scheduler).await;
}

#[tokio::test]
async fn orderly_jsonl_close_resets_running_to_pending_without_counting_a_crash() {
    let directory = tempfile::tempdir().unwrap();
    let context = Context::default();
    let storage = Arc::new(
        JsonlStorage::open_native(directory.path(), &context, Default::default())
            .await
            .unwrap(),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let seen = entered.clone();
    let task = one_step(
        "close",
        Arc::new(move |_, runtime, _| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.notify_one();
                runtime.signal().cancelled().await;
                Err(Error::message("stopped"))
            })
        }),
    );
    let scheduler = TaskScheduler::open(
        storage,
        catalog(std::slice::from_ref(&task)),
        Default::default(),
        Default::default(),
        &context,
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &context,
        )
        .await
        .unwrap();
    let id = start(&scheduler, root, &task).await;
    scheduler.resume().unwrap();
    entered.notified().await;
    close(&scheduler).await;
    let reopened = Arc::new(
        JsonlStorage::open_native(directory.path(), &context, Default::default())
            .await
            .unwrap(),
    );
    let committed = reopened.task(id, &context).await.unwrap().unwrap();
    assert_eq!(committed.state.status(), TaskStatus::Pending);
    let scheduler = TaskScheduler::open(
        reopened,
        catalog(&[task]),
        Default::default(),
        Default::default(),
        &context,
    )
    .await
    .unwrap();
    let recovered = record(&scheduler, id).await;
    assert_eq!(recovered.state.status(), TaskStatus::Pending);
    assert_eq!(recovered.info.restarts, None);
    close(&scheduler).await;
}

#[tokio::test]
async fn migrations_are_cached_by_definition_identity_and_replacement_can_repair_them() {
    let storage = Arc::new(MemoryStorage::new());
    let context = Context::default();
    let id = seed(storage.clone(), "migrating", false, None, false).await;
    let migrations = Arc::new(AtomicUsize::new(0));
    let count = migrations.clone();
    let base = one_step(
        "migrating",
        Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(|_, _| async { Ok(complete(JsonValue::Null)) }, &context)
                    .await
            })
        }),
    );
    let broken = TaskDefinition {
        name: base.definition().name.clone(),
        version: 2,
        initial: base.definition().initial.clone(),
        phases: base.definition().phases.clone(),
        abort: base.definition().abort.clone(),
        migrate: Some(Arc::new(move |_, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Err(Error::message("migration failed"))
        })),
        hooks: None,
    };
    let fixed = TaskDefinition {
        name: broken.name.clone(),
        version: 2,
        initial: broken.initial.clone(),
        phases: broken.phases.clone(),
        abort: broken.abort.clone(),
        migrate: Some(Arc::new(|input, checkpoint, from| {
            assert_eq!(from, 1);
            Ok((input, checkpoint))
        })),
        hooks: None,
    };
    let installed = define_task(broken);
    let catalog = catalog(&[installed]);
    let wakes = Arc::new(Mutex::new(Vec::new()));
    let delivered = Arc::new(tokio::sync::Notify::new());
    let capture = wakes.clone();
    let notification = delivered.clone();
    let scheduler = TaskScheduler::open(
        storage,
        catalog.clone(),
        SchedulerOptions {
            on_wake: Some(Arc::new(move |wake| {
                capture.lock().push(wake);
                notification.notify_one();
                Ok(())
            })),
            ..Default::default()
        },
        Default::default(),
        &context,
    )
    .await
    .unwrap();
    scheduler.resume().unwrap();
    for _ in 0..100 {
        if migrations.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        scheduler.blocked_reason(&record(&scheduler, id).await),
        Some(BlockedReason::MigrationFailed)
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while wakes.lock().last().is_none_or(|wake| wake.active) {
            delivered.notified().await;
        }
    })
    .await
    .unwrap();
    scheduler.resume().unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(migrations.load(Ordering::SeqCst), 1);
    catalog
        .replace([InstalledTask {
            task: define_task(fixed).erased(),
            extension: None,
        }])
        .unwrap();
    let result = receipt(&scheduler, id).await;
    assert_eq!(result.info.version, 2);
    assert!(matches!(
        result.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ));
    close(&scheduler).await;
}

#[tokio::test]
async fn runtime_validates_invalid_sleep_times_before_json_can_coerce_them_to_null() {
    for (time, spelling) in [(f64::NAN, "NaN"), (f64::INFINITY, "Infinity"), (-1.0, "-1")] {
        let task = one_step(
            "invalid-sleep",
            Arc::new(move |_, runtime, context| {
                Box::pin(async move {
                    runtime
                        .sleep_until(time, json(r#"{"phase":"run"}"#), &context)
                        .await
                })
            }),
        );
        let (scheduler, root) = open(std::slice::from_ref(&task)).await;
        let id = start(&scheduler, root, &task).await;
        scheduler.resume().unwrap();
        let TaskState::Terminal {
            outcome: TaskOutcome::Faulted { error },
        } = receipt(&scheduler, id).await.state
        else {
            panic!("invalid sleep should fault");
        };
        assert_eq!(
            error.message.to_string(),
            format!("Task {id} cannot sleep until {spelling}: not a time in milliseconds")
        );
        close(&scheduler).await;
    }
}
