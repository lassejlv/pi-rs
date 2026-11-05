#[path = "support/session_storage.rs"]
mod support;
use parking_lot::Mutex;
use pi_chord::{
    AbortController,
    context::{with_abort_signal, with_cancel},
};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Counter {
    count: u32,
}
impl DocumentState for Counter {}
fn counter() -> SessionDocToken<Counter> {
    define_doc(DocDefinition::new(
        "counter",
        1,
        SessionScope,
        || Ok(Counter { count: 0 }),
        |_, _, _| false,
    ))
    .unwrap()
}
fn open() -> (Arc<ControlledStorage>, Session) {
    let storage = Arc::new(ControlledStorage::default());
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            document_retention_ms: f64::INFINITY,
            report: Arc::new(|_| {}),
            ..Default::default()
        },
    );
    (storage, session)
}
async fn bump(session: &Session) -> Result<()> {
    session
        .commit(
            |tx| async move {
                tx.doc(&counter(), ()).await?.update(|c| {
                    c.count += 1;
                    Ok(())
                })
            },
            &Context::default(),
        )
        .await
}
async fn count(session: &Session) -> u32 {
    session
        .snapshot(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap()
        .count
}
async fn conversation(session: &Session) -> ConversationId {
    session
        .commit(
            |tx| async move {
                Ok(tx
                    .create_conversation(ConversationOwnership::Ownerless)
                    .await?
                    .id)
            },
            &Context::default(),
        )
        .await
        .unwrap()
}
fn task_checkpoint() -> JsonValue {
    pi_chord::json::parse("{\"phase\":\"start\"}").unwrap()
}
async fn task(session: &Session, conversation: ConversationId) -> TaskId {
    session
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(conversation);
                tx.create_task_record(
                    "test".into(),
                    1,
                    JsonValue::Null,
                    |_| Ok(task_checkpoint()),
                    options,
                )
                .await
            },
            &Context::default(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn immutable_revision_adoption_rollback_and_revocation() {
    let (storage, session) = open();
    assert!(
        session
            .snapshot(&counter(), (), &Context::default())
            .await
            .unwrap()
            .is_none()
    );
    let (draft, tx) = session
        .commit(
            |tx| async move {
                let draft = tx.doc(&counter(), ()).await?;
                draft.update(|c| {
                    c.count = 1;
                    Ok(())
                })?;
                Ok((draft, tx))
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert!(draft.read().is_err());
    assert!(tx.doc(&counter(), ()).await.is_err());
    let typed = session
        .snapshot(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let typed_again = session
        .snapshot(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&typed, &typed_again));
    let old = session
        .snapshot_raw(
            counter().definition().clone(),
            counter().address(()),
            &Context::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let again = session
        .snapshot_raw(
            counter().definition().clone(),
            counter().address(()),
            &Context::default(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&old, &again));
    let failure = Error::message("callback failed");
    let expected = failure.clone();
    assert!(
        session
            .commit(
                move |tx| async move {
                    tx.doc(&counter(), ()).await?.update(|c| {
                        c.count = 99;
                        Ok(())
                    })?;
                    Err::<(), _>(failure)
                },
                &Context::default()
            )
            .await
            .unwrap_err()
            .same_error(&expected)
    );
    assert_eq!(count(&session).await, 1);
    assert_eq!(storage.writes.lock().len(), 1);
    bump(&session).await.unwrap();
    assert_eq!(count(&session).await, 2);
    assert_eq!(
        old.as_ref(),
        &pi_chord::json::parse("{\"count\":1}").unwrap()
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn migration_only_commit_replaces_old_observer_shape_without_notifying_new_observer() {
    let (storage, session) = open();
    let old = define_doc(DocDefinition::new(
        "shape",
        1,
        SessionScope,
        || Ok(serde_json::from_str::<JsonObject>("{\"count\":1}")?),
        |_, _, _| false,
    ))
    .unwrap();
    let new = define_doc(
        DocDefinition::new(
            "shape",
            2,
            SessionScope,
            || Ok(JsonObject::default()),
            |_, _, _| false,
        )
        .with_migration(|mut value, _| {
            value.insert("label".into(), "migrated".into());
            Ok(value)
        }),
    )
    .unwrap();
    let token = old.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let before = session
        .snapshot(&old, (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let older = session
        .watch_doc(&old, (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let newer = session
        .watch_doc(&new, (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Arc::new(Mutex::new(Some(send)));
    older
        .start(move |frame| {
            send.lock().take().unwrap().send(frame.ops).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    let new_frames = Arc::new(AtomicUsize::new(0));
    let capture = new_frames.clone();
    newer
        .start(move |_| {
            capture.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        })
        .unwrap();
    let token = new.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let ops = receive.await.unwrap();
    assert!(matches!(&ops[..], [pi_chord::delta::Op::Replace(_)]));
    assert_eq!(new_frames.load(Ordering::SeqCst), 0);
    assert!(!before.contains_key(&JsonString::from("label")));
    assert!(
        session
            .snapshot(&old, (), &Context::default())
            .await
            .is_err()
    );
    assert!(matches!(
        &storage.writes.lock().last().unwrap()[0],
        StorageWrite::DocumentChange {
            content: DocumentContent::Base { version: 2, .. },
            ..
        }
    ));
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn historical_snapshots_use_entry_commit_and_incarnation_and_follow_ancestry() {
    let (_, session) = open();
    let parent = conversation(&session).await;
    let token = define_doc(DocDefinition::new(
        "history",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        || Ok(Counter { count: 0 }),
        |_, _, _| false,
    ))
    .unwrap();
    let t = token.clone();
    let first = session
        .commit(
            move |tx| async move {
                tx.doc(&t, parent).await?.update(|c| {
                    c.count = 1;
                    Ok(())
                })?;
                Ok(tx.append_entry(parent, EntryDraft::new("first")).await?.id)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let t = token.clone();
    let second = session
        .commit(
            move |tx| async move {
                tx.retire_doc(&t, parent).await?;
                tx.doc(&t, parent).await?.update(|c| {
                    c.count = 2;
                    Ok(())
                })?;
                Ok(tx.append_entry(parent, EntryDraft::new("second")).await?.id)
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        session
            .snapshot_as_of(&token, parent, first, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .count,
        1
    );
    assert_eq!(
        session
            .snapshot_as_of(&token, parent, second, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .count,
        2
    );
    let child = session
        .commit(
            move |tx| async move {
                tx.fork_conversation(parent, first, ConversationOwnership::Ownerless)
                    .await
            },
            &Context::default(),
        )
        .await
        .unwrap()
        .id;
    assert_eq!(
        session
            .snapshot_as_of(&token, child, first, &Context::default())
            .await
            .unwrap()
            .unwrap()
            .count,
        1
    );
    assert!(
        session
            .snapshot_as_of(&token, child, second, &Context::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("not visible")
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn queued_reads_survive_the_close_seal_but_existing_document_watches_cannot_attach() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    let gate = Arc::new(Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let held = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?.update(|counter| {
                counter.count += 1;
                Ok(())
            })
        },
        &Context::default(),
    );
    gate.entered().await;
    let snapshot = session.snapshot(&counter(), (), &Context::default());
    let watch = session.watch_doc(&counter(), (), &Context::default());
    let absent = define_doc(DocDefinition::new(
        "absent",
        1,
        SessionScope,
        || Ok(Counter { count: 0 }),
        |_, _, _| false,
    ))
    .unwrap();
    let absent_watch = session.watch_doc(&absent, (), &Context::default());
    let closing = session.close(&Context::default());
    gate.release();
    held.await.unwrap();
    // A cached snapshot captures its revision at the call, as in the source.
    assert_eq!(snapshot.await.unwrap().unwrap().count, 1);
    assert!(absent_watch.await.unwrap().is_none());
    let acquired = watch.await;
    closing.await.unwrap();
    assert!(
        matches!(acquired, Err(ref error) if error.to_string().contains("closed")),
        "an existing document cannot gain an observer after close sealed admission"
    );
    assert_eq!(session.cache_sizes().attached, 0);
}

#[tokio::test]
async fn a_document_watch_loading_during_close_cannot_attach_after_its_read_finishes() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    session.unload_documents().await.unwrap();
    let gate = Arc::new(Gate::default());
    *storage.find_gate.lock() = Some(gate.clone());
    let watch = session.watch_doc(&counter(), (), &Context::default());
    gate.entered().await;
    let closing = session.close(&Context::default());
    gate.release();
    let acquired = watch.await;
    closing.await.unwrap();
    assert!(
        matches!(acquired, Err(ref error) if error.to_string().contains("closed")),
        "finishing a pre-close read must not attach a post-close observer"
    );
    assert_eq!(session.cache_sizes().attached, 0);
}

#[tokio::test]
async fn close_waiter_cancellation_does_not_cancel_cleanup_and_subscriptions_reject_after_seal() {
    let (storage, session) = open();
    let gate = Arc::new(Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let commit = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?;
            Ok(())
        },
        &Context::default(),
    );
    gate.entered().await;
    let (context, cancel) = with_cancel(&Context::default());
    let close = session.close(&context);
    cancel.abort();
    assert_eq!(close.await.unwrap_err().name(), "AbortError");
    assert!(session.subscribe_close(|| {}).is_err());
    assert!(session.subscribe_commits(|_, _| Ok(())).is_err());
    gate.release();
    commit.await.unwrap();
    assert!(matches!(session.closed().await, SessionEnd::Closed));
    assert_eq!(storage.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropped_commit_waiter_still_finishes_on_the_mutation_line() {
    let (_, session) = open();
    let commit = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?.update(|c| {
                c.count = 7;
                Ok(())
            })
        },
        &Context::default(),
    );
    drop(commit);
    session
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
    assert_eq!(count(&session).await, 7);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn first_failure_wakes_failed_waiters_before_shutdown_and_retains_cause() {
    let (_, session) = open();
    let failed = session.failed();
    let first = Error::message("first");
    session.fail(first.clone());
    session.fail(Error::message("second"));
    let error = failed.await.unwrap_err();
    let ErrorKind::SessionFailed { cause } = error.kind() else {
        panic!("must fail")
    };
    assert!(cause.same_error(&first));
    let SessionEnd::Failed(cause) = session.closed().await else {
        panic!("must fail")
    };
    assert!(cause.same_error(&first));
}

#[tokio::test]
async fn durable_entry_events_are_recorded_on_the_active_telemetry_context() {
    use pi_telemetry::{InMemoryTelemetryContext, TelemetryContextExt};
    let telemetry = InMemoryTelemetryContext::new();
    let (_, session) = open();
    let conversation = conversation(&session).await;
    let worker = session.clone();
    telemetry
        .start_span("commit", move |span| async move {
            let context = pi_chord::context::with_telemetry(span, &Context::default());
            worker
                .commit(
                    move |tx| async move {
                        tx.append_entry(conversation, EntryDraft::new("event"))
                            .await?;
                        Ok(())
                    },
                    &context,
                )
                .await
                .unwrap();
        })
        .await;
    let spans = telemetry.get_spans();
    assert_eq!(spans[0].events[0].name, "pi.durable.entry");
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn before_close_writes_follow_admitted_work_without_publication() {
    let storage = Arc::new(ControlledStorage::default());
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let capture = cleanup.clone();
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            before_close: Some(Arc::new(move |session| {
                let writes = capture.lock().clone();
                session.write_at_close(move || Ok(writes))
            })),
            ..Default::default()
        },
    );
    let conversation = conversation(&session).await;
    let id = task(&session, conversation).await;
    let mut record = storage
        .task(id, &Context::default())
        .await
        .unwrap()
        .unwrap();
    record.info.abort_requested = true;
    cleanup.lock().push(StorageWrite::Task {
        value: Box::new(record),
    });
    let published = Arc::new(AtomicUsize::new(0));
    let counter = published.clone();
    let _subscription = session
        .subscribe_commits(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    session.close(&Context::default()).await.unwrap();
    assert_eq!(published.load(Ordering::SeqCst), 0);
    assert_eq!(storage.writes.lock().len(), 3);
    assert!(
        storage.writes.lock()[2]
            .iter()
            .any(|w| matches!(w,StorageWrite::Task {value} if value.info.abort_requested))
    );
}

#[tokio::test]
async fn failed_session_skips_cleanup_writes_and_close_errors_preserve_the_first_cause() {
    let storage = Arc::new(ControlledStorage::default());
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            before_close: Some(Arc::new(|session| {
                session.write_at_close(|| panic!("failed session must skip writes"))
            })),
            ..Default::default()
        },
    );
    let failure = Error::message("storage failed");
    *storage.close_error.lock() = Some(Error::message("close failed"));
    session.fail(failure.clone());
    assert!(session.close(&Context::default()).await.is_err());
    let SessionEnd::Failed(cause) = session.closed().await else {
        panic!("must fail")
    };
    assert!(cause.same_error(&failure));
    assert!(storage.writes.lock().is_empty());
    let (storage, session) = open();
    let close_error = Error::message("close only failed");
    *storage.close_error.lock() = Some(close_error.clone());
    assert!(
        session
            .close(&Context::default())
            .await
            .unwrap_err()
            .same_error(&close_error)
    );
    let SessionEnd::Failed(cause) = session.closed().await else {
        panic!("must fail")
    };
    assert!(cause.same_error(&close_error));
}

#[tokio::test]
async fn listener_lists_are_snapshotted_per_delivery_group() {
    let (_, session) = open();
    let target: Arc<Mutex<Option<session::Subscription>>> = Arc::default();
    let capture = target.clone();
    let _internal = session
        .subscribe_internal(move |_, _| {
            capture.lock().take();
            Ok(())
        })
        .unwrap();
    *target.lock() = Some(
        session
            .subscribe_commits(|_, _| panic!("removed before external snapshot"))
            .unwrap(),
    );
    let other: Arc<Mutex<Option<session::Subscription>>> = Arc::default();
    let capture = other.clone();
    let _first = session
        .subscribe_commits(move |_, _| {
            capture.lock().take();
            Ok(())
        })
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *other.lock() = Some(
        session
            .subscribe_commits(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap(),
    );
    bump(&session).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    bump(&session).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn watches_share_close_listener_registration_order() {
    let (_, session) = open();
    bump(&session).await.unwrap();
    let watch_slot: Arc<Mutex<Option<pi_chord::Watch>>> = Arc::default();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let (slot, log) = (watch_slot.clone(), observations.clone());
    let _early = session
        .subscribe_close(move || {
            log.lock()
                .push(slot.lock().as_ref().unwrap().end_reason().is_some());
        })
        .unwrap();
    let watch = session
        .watch_doc(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    *watch_slot.lock() = Some(watch.raw().clone());
    let (slot, log) = (watch_slot.clone(), observations.clone());
    let _late = session
        .subscribe_close(move || {
            log.lock()
                .push(slot.lock().as_ref().unwrap().end_reason().is_some());
        })
        .unwrap();
    session.close(&Context::default()).await.unwrap();
    assert_eq!(*observations.lock(), vec![false, true]);
}

#[tokio::test]
async fn empty_changes_do_not_write_or_publish() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    let publications = Arc::new(AtomicUsize::new(0));
    let capture = publications.clone();
    let _subscription = session.subscribe_commits(move |_, _| {
        capture.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    session
        .commit(
            |tx| async move {
                tx.doc(&counter(), ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(storage.writes.lock().len(), 1);
    assert_eq!(publications.load(Ordering::SeqCst), 0);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn table_reads_close_at_first_write_but_document_reads_remain_allowed() {
    let (_, session) = open();
    session
        .commit(
            |tx| async move {
                assert!(tx.conversation(ROOT_CONVERSATION_ID).await?.is_none());
                let created = tx.create_conversation(ConversationOwnership::Ownerless);
                assert_eq!(
                    tx.conversation(ROOT_CONVERSATION_ID)
                        .await
                        .unwrap_err()
                        .name(),
                    "ReadAfterWrite"
                );
                created.await?;
                tx.doc(&counter(), ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn duplicate_acquisitions_share_a_draft_and_first_seed() {
    let (_, session) = open();
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = calls.clone();
    let token = define_doc_family(DocFamilyDefinition::new(
        "member",
        1,
        SessionScope,
        move |count: u32| {
            capture.fetch_add(1, Ordering::SeqCst);
            Ok(Counter { count })
        },
        |_, _, _| false,
    ))
    .unwrap();
    let inner = token.clone();
    session
        .commit(
            move |tx| async move {
                let first = tx.doc_family(&inner, (), "x", 3);
                let second = tx.doc_family(&inner, (), "x", 9);
                let (first, second) = tokio::try_join!(first, second)?;
                first.update(|c| {
                    c.count += 1;
                    Ok(())
                })?;
                assert_eq!(second.read()?.count, 4);
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        session
            .snapshot_family(&token, (), "x", &Context::default())
            .await
            .unwrap()
            .unwrap()
            .count,
        4
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn unfinished_operations_are_drained_and_cannot_initialize_after_settlement() {
    let (storage, session) = open();
    let gate = Arc::new(Gate::default());
    *storage.find_gate.lock() = Some(gate.clone());
    let escaped = Arc::new(Mutex::new(None));
    let capture = escaped.clone();
    let commit = session.commit(
        move |tx| async move {
            *capture.lock() = Some(tx.doc(&counter(), ()));
            Ok(())
        },
        &Context::default(),
    );
    gate.entered().await;
    gate.release();
    assert!(
        commit
            .await
            .unwrap_err()
            .to_string()
            .contains("pending Tx operations")
    );
    let escaped = escaped.lock().take().unwrap();
    assert!(escaped.await.is_err());
    assert_eq!(storage.mints.load(Ordering::SeqCst), 0);
    assert!(storage.writes.lock().is_empty());
    bump(&session).await.unwrap();
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn callback_error_wins_over_pending_operation_rejection() {
    let (storage, session) = open();
    let gate = Arc::new(Gate::default());
    *storage.find_gate.lock() = Some(gate.clone());
    let failure = Error::message("callback error");
    let expected = failure.clone();
    let commit = session.commit(
        move |tx| async move {
            drop(tx.doc(&counter(), ()));
            Err::<(), _>(failure)
        },
        &Context::default(),
    );
    gate.entered().await;
    gate.release();
    assert!(commit.await.unwrap_err().same_error(&expected));
    bump(&session).await.unwrap();
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn changes_stay_invisible_until_adoption_and_publish_after_storage_settles() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    let before = Arc::new(Gate::default());
    *storage.commit_gate.lock() = Some(before.clone());
    let after = Arc::new(Gate::default());
    *storage.after_adopt.lock() = Some(after.clone());
    let published = Arc::new(AtomicUsize::new(0));
    let capture = published.clone();
    let _subscription = session.subscribe_commits(move |_, _| {
        capture.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let commit = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?.update(|c| {
                c.count = 2;
                Ok(())
            })
        },
        &Context::default(),
    );
    before.entered().await;
    assert_eq!(count(&session).await, 1);
    assert_eq!(published.load(Ordering::SeqCst), 0);
    before.release();
    after.entered().await;
    assert_eq!(count(&session).await, 2);
    assert_eq!(published.load(Ordering::SeqCst), 0);
    after.release();
    commit.await.unwrap();
    assert_eq!(published.load(Ordering::SeqCst), 1);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn storage_faults_fail_once_with_original_identity() {
    for after in [false, true] {
        let (storage, session) = open();
        bump(&session).await.unwrap();
        let failure = Error::message("disk vanished");
        *storage.fault.lock() = Some(if after {
            Fault::After(failure.clone())
        } else {
            Fault::Before(failure.clone())
        });
        assert!(bump(&session).await.unwrap_err().same_error(&failure));
        let SessionEnd::Failed(cause) = session.closed().await else {
            panic!("must fail")
        };
        assert!(cause.same_error(&failure));
        assert_eq!(
            session
                .snapshot(&counter(), (), &Context::default())
                .await
                .unwrap_err()
                .name(),
            "SessionFailed"
        );
        assert_eq!(storage.closes.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn missing_adoption_is_fatal() {
    let (storage, session) = open();
    *storage.fault.lock() = Some(Fault::SkipAdopt);
    assert!(
        bump(&session)
            .await
            .unwrap_err()
            .to_string()
            .contains("without adopting")
    );
    assert!(matches!(session.closed().await, SessionEnd::Failed(_)));
}

#[tokio::test]
async fn request_errors_do_not_fail_reads_but_are_fatal_for_id_allocation() {
    let (storage, session) = open();
    *storage.read_error.lock() = Some(Error::request("invalid request"));
    assert_eq!(
        session
            .commit(
                |tx| async move { tx.conversation(ROOT_CONVERSATION_ID).await },
                &Context::default()
            )
            .await
            .unwrap_err()
            .name(),
        "StorageRequestError"
    );
    bump(&session).await.unwrap();
    *storage.mint_error.lock() = Some(Error::request("allocator failed"));
    assert!(
        session
            .commit(
                |tx| async move {
                    tx.create_conversation(ConversationOwnership::Ownerless)
                        .await
                },
                &Context::default()
            )
            .await
            .is_err()
    );
    assert!(matches!(session.closed().await, SessionEnd::Failed(_)));
}

#[tokio::test]
async fn catching_storage_failure_in_callback_cannot_commit() {
    let (storage, session) = open();
    *storage.read_error.lock() = Some(Error::message("read failed"));
    assert_eq!(
        session
            .commit(
                |tx| async move {
                    let _ = tx.conversation(ROOT_CONVERSATION_ID).await;
                    Ok(())
                },
                &Context::default()
            )
            .await
            .unwrap_err()
            .name(),
        "SessionFailed"
    );
    assert!(storage.writes.lock().is_empty());
    assert!(matches!(session.closed().await, SessionEnd::Failed(_)));
}

#[tokio::test]
async fn close_seals_admission_drains_commits_and_ends_watches_immediately() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    let watch = session
        .watch_doc(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let gate = Arc::new(Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let first = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?.update(|c| {
                c.count += 1;
                Ok(())
            })
        },
        &Context::default(),
    );
    gate.entered().await;
    let second = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?.update(|c| {
                c.count += 1;
                Ok(())
            })
        },
        &Context::default(),
    );
    let closes = Arc::new(AtomicUsize::new(0));
    let capture = closes.clone();
    let _subscription = session.subscribe_close(move || {
        capture.fetch_add(1, Ordering::SeqCst);
    });
    let close = session.close(&Context::default());
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(watch.closed().await, WatchEnd::Ended);
    assert!(
        session
            .commit(|_| async { Ok(()) }, &Context::default())
            .await
            .is_err()
    );
    assert_eq!(storage.closes.load(Ordering::SeqCst), 0);
    gate.release();
    first.await.unwrap();
    second.await.unwrap();
    close.await.unwrap();
    assert_eq!(storage.writes.lock().len(), 3);
    assert!(matches!(session.closed().await, SessionEnd::Closed));
}

#[tokio::test]
async fn cancelling_commit_after_admission_does_not_undo_durable_write() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    let gate = Arc::new(Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let (context, cancel) = with_cancel(&Context::default());
    let commit = session.commit(
        |tx| async move {
            tx.doc(&counter(), ()).await?.update(|c| {
                c.count += 1;
                Ok(())
            })
        },
        &context,
    );
    gate.entered().await;
    cancel.abort();
    gate.release();
    commit.await.unwrap();
    assert_eq!(count(&session).await, 2);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn cancellation_before_execution_rejects_without_entering_callback() {
    let (_, session) = open();
    let cancel = AbortController::new();
    cancel.abort();
    let context = with_abort_signal(cancel.signal(), &Context::default());
    assert_eq!(
        session
            .commit(
                |_| async {
                    panic!("must not run");
                    #[allow(unreachable_code)]
                    Ok(())
                },
                &context
            )
            .await
            .unwrap_err()
            .name(),
        "AbortError"
    );
    bump(&session).await.unwrap();
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn cancelled_watch_acquisition_does_not_leave_a_pin() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    session.unload_documents().await.unwrap();
    let gate = Arc::new(Gate::default());
    *storage.find_gate.lock() = Some(gate.clone());
    let (context, cancel) = with_cancel(&Context::default());
    let acquire = session.watch_doc(&counter(), (), &context);
    gate.entered().await;
    cancel.abort();
    assert_eq!(acquire.await.err().unwrap().name(), "AbortError");
    gate.release();
    session
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
    assert_eq!(session.cache_sizes().attached, 0);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn observer_retirement_is_terminal_and_recreation_has_new_identity() {
    let (storage, session) = open();
    bump(&session).await.unwrap();
    let watch = session
        .watch_doc(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let values = Arc::new(Mutex::new(Vec::new()));
    let capture = values.clone();
    watch
        .start(move |f| {
            capture.lock().push(f.value.map(|v| v.count));
            async { Ok(()) }
        })
        .unwrap();
    session
        .commit(
            |tx| async move {
                tx.retire_doc(&counter(), ()).await?;
                tx.doc(&counter(), ()).await?.update(|c| {
                    c.count = 10;
                    Ok(())
                })
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(watch.closed().await, WatchEnd::Retired);
    assert_eq!(*values.lock(), vec![None]);
    assert_eq!(count(&session).await, 10);
    assert_eq!(session.cache_sizes().attached, 0);
    let writes = storage.writes.lock().last().unwrap().clone();
    assert!(matches!(
        &writes[..],
        [
            StorageWrite::DocumentRetire { .. },
            StorageWrite::DocumentCreate { .. }
        ]
    ));
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn read_only_refusal_does_not_fail_session() {
    let storage = Arc::new(ControlledStorage::default());
    let session = Session::new(
        storage.clone(),
        SessionOptions {
            read_only: true,
            report: Arc::new(|_| {}),
            ..Default::default()
        },
    );
    assert_eq!(bump(&session).await.unwrap_err().name(), "ReadOnlySession");
    session
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
    assert!(storage.writes.lock().is_empty());
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn external_listener_errors_are_contained_and_internal_errors_fail_after_commit() {
    let (_, session) = open();
    let called = Arc::new(AtomicUsize::new(0));
    let capture = called.clone();
    let _one = session.subscribe_commits(|_, _| Err(Error::message("external")));
    let _two = session.subscribe_commits(move |_, _| {
        capture.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    bump(&session).await.unwrap();
    assert_eq!(count(&session).await, 1);
    assert_eq!(called.load(Ordering::SeqCst), 1);
    let error = Error::message("internal");
    let cause = error.clone();
    let _internal = session.subscribe_internal(move |_, _| Err(error.clone()));
    bump(&session).await.unwrap();
    assert_eq!(called.load(Ordering::SeqCst), 2);
    let SessionEnd::Failed(failed) = session.closed().await else {
        panic!("must fail")
    };
    assert!(failed.same_error(&cause));
}

#[tokio::test]
async fn task_times_owners_submissions_and_terminal_document_retirement() {
    let (storage, session) = open();
    let conversation = conversation(&session).await;
    let task = task(&session, conversation).await;
    let token = define_doc(DocDefinition::new(
        "scratch",
        1,
        TaskScope,
        || Ok(Counter { count: 0 }),
        |_, _, _| false,
    ))
    .unwrap();
    let t = token.clone();
    session
        .commit(
            move |tx| async move {
                let mut record = tx.task(task).await?.unwrap();
                record.state = TaskState::Running {
                    checkpoint: task_checkpoint(),
                    memos: None,
                };
                tx.set_task(record)?;
                let started = tx.staged_tasks()[0].info.started_at.unwrap();
                assert!(started > 0.0);
                tx.doc(&t, task).await?;
                let submission = tx
                    .create_submission(SubmissionCreate {
                        conversation_id: conversation,
                        trace: None,
                        request_id: None,
                        state: SubmissionState::InputQueued,
                    })
                    .await?;
                let entry = tx
                    .append_entry(conversation, EntryDraft::new("answer"))
                    .await?;
                tx.place_submission(submission.id(), entry.id)?;
                tx.settle_submission(
                    submission.id(),
                    SubmissionSettlement::Done { answer: entry.id },
                )?;
                let mut record = tx.staged_tasks()[0].clone();
                record.state = TaskState::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: JsonValue::Null,
                    },
                };
                tx.set_task(record)?;
                assert_eq!(tx.staged_tasks()[0].info.started_at, Some(started));
                assert!(tx.staged_tasks()[0].info.ended_at.is_some());
                assert!(tx.doc(&t, task).await.is_err());
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert!(
        session
            .snapshot(&token, task, &Context::default())
            .await
            .unwrap()
            .is_none()
    );
    let writes = storage.writes.lock().last().unwrap().clone();
    assert!(
        writes
            .iter()
            .any(|w| matches!(w, StorageWrite::DocumentRetire { .. }))
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn failed_document_watches_preserve_the_session_error_identity() {
    let (_, session) = open();
    bump(&session).await.unwrap();
    let watch = session
        .watch_doc(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let original = Error::host("DiskError", "original failure".into());
    session.fail(original.clone());
    let pi_chord::WatchEnd::Failed { error } = watch.closed().await else {
        panic!("watch did not fail")
    };
    assert!(error.cause::<Error>().unwrap().same_error(&original));
    let _ = session.close(&Context::default()).await;
}

#[tokio::test]
async fn document_listener_failures_retain_native_error_identity() {
    let storage = Arc::new(ControlledStorage::default());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let capture = reports.clone();
    let session = Session::new(
        storage,
        SessionOptions {
            report: Arc::new(move |error| capture.lock().push(error)),
            ..Default::default()
        },
    );
    bump(&session).await.unwrap();
    let watch = session
        .watch_doc(&counter(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let original = Error::host("ListenerError", "original listener failure".into());
    let thrown = original.clone();
    watch
        .start(move |_| {
            let thrown = thrown.clone();
            async move { Err(thrown) }
        })
        .unwrap();
    bump(&session).await.unwrap();
    let pi_chord::WatchEnd::ListenerError { error } = watch.closed().await else {
        panic!("watch listener did not fail")
    };
    assert!(error.cause::<Error>().unwrap().same_error(&original));
    assert!(reports.lock()[0].same_error(&original));
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn close_listener_failures_are_contained_without_joining_pending_listeners() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::new(tokio::sync::Notify::new());
    let capture = reports.clone();
    let notify = reported.clone();
    let session = Session::new(
        Arc::new(ControlledStorage::default()),
        SessionOptions {
            report: Arc::new(move |error| {
                capture.lock().push(error);
                notify.notify_one();
            }),
            ..Default::default()
        },
    );
    let synchronous = Error::message("close listener failed synchronously");
    let cause = synchronous.clone();
    let _first = session
        .subscribe_close_async(move || Err(cause.clone()))
        .unwrap();
    let asynchronous = Error::message("close listener failed asynchronously");
    let cause = asynchronous.clone();
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let _second = session
        .subscribe_close_async(move || {
            let wait = wait.lock().take().unwrap();
            let cause = cause.clone();
            Ok(Box::pin(async move {
                let _ = wait.await;
                Err(cause)
            }))
        })
        .unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let _third = session
        .subscribe_close(move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.close(&Context::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(session.failure().is_none());
    assert!(reports.lock()[0].same_error(&synchronous));
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while reports.lock().len() != 2 {
            reported.notified().await;
        }
    })
    .await
    .unwrap();
    assert!(reports.lock()[1].same_error(&asynchronous));
}
