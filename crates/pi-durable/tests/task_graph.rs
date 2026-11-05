use indexmap::IndexMap;
use pi_chord::{
    AbortController, WatchEnd,
    context::{create_context_key, with_abort_signal, with_context_value},
};
use pi_durable::{
    harness::{catalog::*, inspect::*, scheduler::*, task_graph::*},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
type JsonTask = Task<JsonValue, JsonValue, JsonValue>;
fn task(name: &str, version: u64) -> JsonTask {
    define_task(TaskDefinition {
        name: name.into(),
        version,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([(
            "run".into(),
            Arc::new(
                |_: RunningTask<JsonValue, JsonValue, JsonValue>,
                 runtime: pi_durable::TaskRuntime<JsonValue, JsonValue, JsonValue>,
                 context|
                 -> pi_durable::storage::StorageFuture<()> {
                    Box::pin(async move {
                        runtime
                            .commit(
                                |_, _| async {
                                    Ok(Some(pi_durable::harness::state::NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: JsonValue::Null,
                                        },
                                    }))
                                },
                                &context,
                            )
                            .await
                    })
                },
            ) as PhaseHandler<_, _, _>,
        )]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    })
}
fn install(catalog: &TaskCatalog, tasks: &[JsonTask]) {
    catalog
        .replace(tasks.iter().map(|task| InstalledTask {
            task: task.erased(),
            extension: None,
        }))
        .unwrap();
}
async fn setup(
    tasks: &[JsonTask],
) -> (
    TaskScheduler,
    ConversationId,
    TaskCatalog,
    Arc<support::ControlledStorage>,
) {
    let catalog = TaskCatalog::default();
    install(&catalog, tasks);
    let storage = Arc::new(support::ControlledStorage::default());
    let scheduler = TaskScheduler::open(
        storage.clone(),
        catalog.clone(),
        Default::default(),
        Default::default(),
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
    (scheduler, root, catalog, storage)
}
async fn add(scheduler: &TaskScheduler, root: ConversationId, task: &JsonTask) -> TaskId {
    let task = task.clone();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
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
async fn frame(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<pi_chord::watch::WatchFrame<TaskGraph>>,
) -> pi_chord::watch::WatchFrame<TaskGraph> {
    tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn graph_shares_revisions_replays_exact_frames_and_advances_only_after_durability() {
    let work = task("work", 1);
    let (scheduler, root, _, storage) = setup(std::slice::from_ref(&work)).await;
    let first_id = add(&scheduler, root, &work).await;
    let inspected = scheduler.inspect(&Context::default()).await.unwrap();
    let inspected_again = scheduler.inspect(&Context::default()).await.unwrap();
    assert!(Arc::ptr_eq(
        &inspected.tasks[0].record,
        &inspected_again.tasks[0].record
    ));
    let first = scheduler
        .watch_task_graph(&Context::default())
        .await
        .unwrap();
    let second = scheduler
        .watch_task_graph(&Context::default())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first.value(), &second.value()));
    let mut replica = pi_chord::json::to_value(first.value().as_ref()).unwrap();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    first
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    let gate = Arc::new(support::Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let prior = first.value();
    let task_clone = work.clone();
    let adding = scheduler.session().commit(
        move |tx| async move {
            let mut options = TaskOptions::new(TaskOwnership::Conversation);
            options.conversation_id = Some(root);
            tx.create_task(&task_clone, JsonValue::Null, options).await
        },
        &Context::default(),
    );
    gate.entered().await;
    assert!(Arc::ptr_eq(&first.value(), &prior));
    assert!(receive.try_recv().is_err());
    gate.release();
    let new_id = adding.await.unwrap().untyped();
    let update = frame(&mut receive).await;
    replica = pi_chord::delta::apply_immutable(Some(&replica), &update.ops).unwrap();
    assert_eq!(
        replica,
        pi_chord::json::to_value(update.value.as_ref()).unwrap()
    );
    assert!(update.value.tasks.contains_key(&new_id));
    assert!(Arc::ptr_eq(
        &prior.tasks[&first_id],
        &update.value.tasks[&first_id]
    ));
    let after = first.value();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut record = tx.task(first_id).await?.unwrap();
                record.state = TaskState::Pending {
                    checkpoint: json(r#"{"phase":"run"}"#),
                    memos: Some(JsonObject::default()),
                };
                tx.set_task(record)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first.value(), &after));
    assert!(receive.try_recv().is_err());
    first.stop();
    second.stop();
    let rebuilt = scheduler
        .watch_task_graph(&Context::default())
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&rebuilt.value(), &after));
    assert_eq!(*rebuilt.value(), *after);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
    assert_eq!(rebuilt.closed().await, WatchEnd::Ended);
}
#[tokio::test]
async fn blocked_reasons_refresh_without_commits_and_inspection_preserves_migration_errors() {
    let old = task("migrating", 1);
    let (scheduler, root, catalog, _) = setup(&[]).await;
    let id = add(&scheduler, root, &old).await;
    let watch = scheduler
        .watch_task_graph(&Context::default())
        .await
        .unwrap();
    assert_eq!(
        watch.value().tasks[&id].blocked,
        Some(BlockedReason::MissingTask)
    );
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    install(&catalog, &[old]);
    let update = frame(&mut receive).await;
    assert_eq!(
        update.ops.as_ref(),
        &[pi_chord::delta::Op::Delete(vec![
            "tasks".into(),
            pi_chord::delta::Seg::Key(id.to_string().into()),
            "blocked".into()
        ])]
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let failure = Error::message("cannot migrate");
    let fail = failure.clone();
    let base = task("migrating", 2);
    let original = base.definition();
    let mut definition = TaskDefinition {
        name: original.name.clone(),
        version: original.version,
        initial: original.initial.clone(),
        phases: original.phases.clone(),
        abort: original.abort.clone(),
        migrate: None,
        hooks: None,
    };
    definition.migrate = Some(Arc::new(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(fail.clone())
    }));
    let newer = define_task(definition);
    install(&catalog, &[newer]);
    let inspection = scheduler.inspect(&Context::default()).await.unwrap();
    assert!(matches!(
        inspection.tasks[0].state,
        TaskInspectionState::Ready { migrates: true }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    scheduler.resume().unwrap();
    let update = frame(&mut receive).await;
    assert_eq!(
        update.value.tasks[&id].blocked,
        Some(BlockedReason::MigrationFailed)
    );
    let inspection = scheduler.inspect(&Context::default()).await.unwrap();
    assert!(
        matches!(&inspection.tasks[0].state,TaskInspectionState::Blocked { error:Some(error),.. } if error.same_error(&failure))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
#[tokio::test]
async fn frames_strip_cancellation_and_reentrant_blocked_readers_can_close_the_view() {
    let work = task("work", 1);
    let (scheduler, root, _, storage) = setup(std::slice::from_ref(&work)).await;
    let listener = Arc::new(parking_lot::Mutex::new(
        None::<Arc<dyn Fn() -> Result<()> + Send + Sync>>,
    ));
    let mounted = Arc::new(parking_lot::Mutex::new(None::<TaskGraphWatch>));
    let held = mounted.clone();
    let capture = listener.clone();
    let graph = TaskGraphView::new(
        scheduler.session(),
        storage,
        TaskGraphBlocking {
            blocked: Arc::new(move |_| {
                if let Some(watch) = held.lock().as_ref() {
                    watch.stop();
                }
                None
            }),
            subscribe: Arc::new(move |callback| {
                *capture.lock() = Some(callback);
                Ok(pi_durable::harness::reader::ReaderSubscription::new(|| {
                    Ok(())
                }))
            }),
        },
    )
    .unwrap();
    let watch = graph.watch(&Context::default()).await.unwrap();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    let controller = AbortController::new();
    let key = create_context_key::<u64>("frame");
    let context = with_context_value(
        &key,
        123,
        &with_abort_signal(controller.signal(), &Context::default()),
    );
    let work_clone = work.clone();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
                tx.create_task(&work_clone, JsonValue::Null, options)
                    .await?;
                Ok(())
            },
            &context,
        )
        .await
        .unwrap();
    let delivered = frame(&mut receive).await;
    assert!(delivered.context.abort_signal().is_none());
    assert_eq!(*delivered.context.value(&key).unwrap(), 123);
    *mounted.lock() = Some(watch.clone());
    listener.lock().as_ref().unwrap()().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), watch.closed())
        .await
        .unwrap();
    add(&scheduler, root, &work).await;
    assert_eq!(watch.closed().await, WatchEnd::Stopped);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_waits_do_not_escape_a_partially_published_terminal_commit() {
    let work = task("work", 1);
    let (scheduler, root, _, _) = setup(std::slice::from_ref(&work)).await;
    let id = add(&scheduler, root, &work).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let reached = entered.clone();
    let (release, gate) = std::sync::mpsc::channel();
    let gate = Arc::new(parking_lot::Mutex::new(gate));
    let _subscription=scheduler.session().subscribe_commits(move |publication,_| {
        if publication.changes.iter().any(|change| matches!(change,CommitChange::Task(record) if record.state.status()==TaskStatus::Terminal)) {
            reached.notify_one(); gate.lock().recv().unwrap();
        } Ok(())
    }).unwrap();
    let mut waiting = scheduler.wait_for_task(id, &Context::default());
    let terminal = scheduler.session().commit(
        move |tx| async move {
            let mut record = tx.task(id).await?.unwrap();
            record.state = TaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: JsonValue::Null,
                },
            };
            tx.set_task(record)
        },
        &Context::default(),
    );
    entered.notified().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), waiting.as_mut())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    terminal.await.unwrap();
    assert_eq!(waiting.await.unwrap().state.status(), TaskStatus::Terminal);
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn inspection_lists_only_queued_and_placed_submissions_in_id_order() {
    let (scheduler, root, _, _) = setup(&[]).await;
    let ids = scheduler
        .session()
        .commit(
            move |tx| async move {
                let queued = tx
                    .create_submission(SubmissionCreate {
                        conversation_id: root,
                        trace: None,
                        request_id: None,
                        state: SubmissionState::InputQueued,
                    })
                    .await?
                    .id();
                let placed = tx
                    .create_submission(SubmissionCreate {
                        conversation_id: root,
                        trace: None,
                        request_id: None,
                        state: SubmissionState::InputQueued,
                    })
                    .await?
                    .id();
                let done = tx
                    .create_submission(SubmissionCreate {
                        conversation_id: root,
                        trace: None,
                        request_id: None,
                        state: SubmissionState::WriteQueued,
                    })
                    .await?
                    .id();
                let entry = tx.append_entry(root, EntryDraft::new("note")).await?.id;
                tx.place_submission(placed, entry)?;
                tx.place_submission(done, entry)?;
                Ok(vec![queued, placed])
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let inspection = scheduler.inspect(&Context::default()).await.unwrap();
    assert_eq!(inspection.scheduling, Scheduling::Paused);
    assert!(inspection.tasks.is_empty());
    assert_eq!(
        inspection
            .submissions
            .iter()
            .map(SubmissionRecord::id)
            .collect::<Vec<_>>(),
        ids
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
