use pi_chord::{
    AbortController,
    context::with_abort_signal,
    delta::{Op, apply_immutable},
};
use pi_durable::{
    harness::{live::LIVE_DOC, view::*},
    *,
};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;

async fn setup() -> (Session, ConversationViews, ConversationId) {
    let session = Session::new(
        Arc::new(MemoryStorage::new()),
        SessionOptions {
            conversation_created: Some(Arc::new(|tx, record| {
                Box::pin(async move {
                    pi_durable::harness::create_conversation_documents(&tx, record).await
                })
            })),
            ..Default::default()
        },
    );
    let root = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let views = ConversationViews::new(session.clone()).unwrap();
    (session, views, root)
}
fn json(value: &impl serde::Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}

#[tokio::test]
async fn shared_mounts_publish_replayable_frames_and_keep_unmodified_references() {
    let (session, views, root) = setup().await;
    let first = views.watch(root, &Context::default()).await.unwrap();
    let second = views.watch(root, &Context::default()).await.unwrap();
    let initial = first.value();
    assert_eq!(initial.docs.len(), 5);
    assert!(Arc::ptr_eq(&initial, &second.value()));
    second.start(|_| async { Ok(()) }).unwrap();
    let (send, mut frames) = mpsc::unbounded_channel();
    first
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    let entry = session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, root).await?.update(|live| {
                    live.run = Some(pi_durable::harness::live::LiveRun {
                        task_id: TaskId::from_raw(500),
                        inputs: vec![],
                    });
                    Ok(())
                })?;
                tx.append_entry(root, EntryDraft::new("note")).await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    first.delivery_idle().await;
    second.delivery_idle().await;
    let frame = frames.recv().await.unwrap();
    assert_eq!(
        frame.ops.last(),
        Some(&Op::Splice(
            vec!["entries".into()],
            0,
            0,
            vec![json(&entry)]
        ))
    );
    assert_eq!(
        apply_immutable(Some(&json(initial.as_ref())), &frame.ops).unwrap(),
        json(frame.value.as_ref())
    );
    assert!(Arc::ptr_eq(
        &initial.docs[&JsonString::from("pi.agent")],
        &frame.value.docs[&JsonString::from("pi.agent")]
    ));
    assert!(Arc::ptr_eq(&frame.value, &second.value()));
    let prior = first.value();
    session
        .commit(
            move |tx| async move { tx.append_entry(root, EntryDraft::new("another")).await },
            &Context::default(),
        )
        .await
        .unwrap();
    first.delivery_idle().await;
    let frame = frames.recv().await.unwrap();
    assert!(Arc::ptr_eq(&prior.docs, &frame.value.docs));
    assert!(Arc::ptr_eq(&prior.entries[0], &frame.value.entries[0]));
    first.stop();
    second.stop();
    let fresh = views.watch(root, &Context::default()).await.unwrap();
    assert_eq!(json(fresh.value().as_ref()), json(frame.value.as_ref()));
    assert!(!Arc::ptr_eq(&fresh.value(), &frame.value));
    session.close(&Context::default()).await.unwrap();
    assert_eq!(fresh.closed().await, pi_chord::WatchEnd::Ended);
}

#[tokio::test]
async fn head_markers_keep_the_non_head_suffix_and_replay_exactly() {
    let (session, views, root) = setup().await;
    let entries = session
        .commit(
            move |tx| async move {
                let a = tx.append_entry(root, EntryDraft::new("a")).await?;
                let b = tx.append_entry(root, EntryDraft::new("b")).await?;
                let c = tx.append_entry(root, EntryDraft::new("c")).await?;
                Ok([a, b, c])
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let watch = views.watch(root, &Context::default()).await.unwrap();
    let initial = watch.value();
    let (send, mut frames) = mpsc::unbounded_channel();
    watch
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    let target = entries[1].id;
    let head = session
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("head");
                draft.head = Some(EntryHead::Entry(target));
                tx.append_entry(root, draft).await
            },
            &Context::default(),
        )
        .await
        .unwrap();
    watch.delivery_idle().await;
    let frame = frames.recv().await.unwrap();
    assert_eq!(
        frame
            .value
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [head.id, entries[1].id, entries[2].id]
    );
    assert!(Arc::ptr_eq(&initial.entries[1], &frame.value.entries[1]));
    assert_eq!(
        apply_immutable(Some(&json(initial.as_ref())), &frame.ops).unwrap(),
        json(frame.value.as_ref())
    );
    let fresh = views.watch(root, &Context::default()).await.unwrap();
    assert_eq!(json(fresh.value().as_ref()), json(frame.value.as_ref()));
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn cancelled_factory_registers_nothing_and_missing_conversations_reject() {
    let (session, views, root) = setup().await;
    let controller = AbortController::new();
    let context = with_abort_signal(controller.signal(), &Context::default());
    let (ready, entered) = oneshot::channel();
    let (send, receive) = oneshot::channel();
    let acquisition = views.attach(
        root,
        move |value, release, _| async move {
            ready.send(()).unwrap();
            receive.await.unwrap();
            Ok(Arc::new(pi_chord::Watch::with_frames(
                value,
                move || release(),
                |_| {},
                |_| Vec::new(),
            )))
        },
        &context,
    );
    entered.await.unwrap();
    controller.abort();
    send.send(()).unwrap();
    assert!(acquisition.await.is_err());
    let a = views.watch(root, &Context::default()).await.unwrap();
    a.stop();
    let b = views.watch(root, &Context::default()).await.unwrap();
    assert!(!Arc::ptr_eq(&a.value(), &b.value()));
    assert!(
        views
            .watch(ConversationId::from_raw(9999), &Context::default())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("does not exist")
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn retired_documents_disappear_and_the_next_incarnation_mounts_whole() {
    let (session, views, root) = setup().await;
    let watch = views.watch(root, &Context::default()).await.unwrap();
    let (send, mut frames) = mpsc::unbounded_channel();
    watch
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    session
        .commit(
            move |tx| async move { tx.retire_doc(&LIVE_DOC, root).await },
            &Context::default(),
        )
        .await
        .unwrap();
    session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, root).await?.update(|live| {
                    live.tools = Some(vec![]);
                    Ok(())
                })
            },
            &Context::default(),
        )
        .await
        .unwrap();
    watch.delivery_idle().await;
    let retired = frames.recv().await.unwrap();
    let recreated = frames.recv().await.unwrap();
    assert_eq!(
        retired.ops.as_slice(),
        [Op::Delete(vec!["docs".into(), "pi.live".into()])]
    );
    assert!(
        !retired
            .value
            .docs
            .contains_key(&JsonString::from("pi.live"))
    );
    assert_eq!(
        recreated.ops.as_slice(),
        [Op::Set(
            vec!["docs".into(), "pi.live".into()],
            pi_chord::json::parse(r#"{"tools":[]}"#).unwrap()
        )]
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn adoption_is_the_visibility_boundary_and_a_dropped_waiter_does_not_discard_the_frame() {
    let storage = Arc::new(support::ControlledStorage::default());
    let session = Session::new(storage.clone(), Default::default());
    let root = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let views = ConversationViews::new(session.clone()).unwrap();
    let watch = views.watch(root, &Context::default()).await.unwrap();
    let initial = watch.value();
    let (send, mut frames) = mpsc::unbounded_channel();
    watch
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    let gate = Arc::new(support::Gate::default());
    *storage.commit_gate.lock() = Some(gate.clone());
    let committing = session.commit(
        move |tx| async move { tx.append_entry(root, EntryDraft::new("durable")).await },
        &Context::default(),
    );
    gate.entered().await;
    assert!(frames.try_recv().is_err());
    assert!(Arc::ptr_eq(&initial, &watch.value()));
    drop(committing);
    gate.release();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.value.entries.len(), 1);
    assert_eq!(
        storage
            .memory
            .scan_entries(
                &EntryQuery {
                    conversation_id: root,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None
                },
                10,
                None,
                &Context::default()
            )
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn observer_failures_are_contained_and_detachment_is_reentrant() {
    use parking_lot::Mutex;
    struct Stopper {
        other: Mutex<Option<ConversationViewWatch>>,
        error: Error,
    }
    impl ViewObserver for Stopper {
        fn advance(&self, _: Arc<ConversationView>, _: &[Op], context: &Context) -> Result<()> {
            assert!(context.abort_signal().is_none());
            if let Some(watch) = self.other.lock().as_ref() {
                watch.stop();
            }
            Err(self.error.clone())
        }
        fn end(&self, _: Option<Error>) -> Result<()> {
            Ok(())
        }
    }
    let reports = Arc::new(Mutex::new(Vec::new()));
    let record = reports.clone();
    let session = Session::new(
        Arc::new(MemoryStorage::new()),
        SessionOptions {
            report: Arc::new(move |error| record.lock().push(error)),
            ..Default::default()
        },
    );
    let root = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let views = ConversationViews::new(session.clone()).unwrap();
    let error = Error::message("observer failed");
    let retained = error.clone();
    let attachment = views
        .attach(
            root,
            move |_, _, _| async move {
                Ok(Arc::new(Stopper {
                    other: Mutex::new(None),
                    error: retained,
                }))
            },
            &Context::default(),
        )
        .await
        .unwrap();
    let other = views.watch(root, &Context::default()).await.unwrap();
    *attachment.observer.other.lock() = Some(other.clone());
    session
        .commit(
            move |tx| async move { tx.append_entry(root, EntryDraft::new("note")).await },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(other.closed().await, pi_chord::WatchEnd::Stopped);
    assert!(reports.lock()[0].same_error(&error));
    assert!(session.failure().is_none());
    (attachment.detach)();
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn factory_finishing_during_close_cannot_register_behind_a_blocked_close_listener() {
    let session = Session::new(Arc::new(MemoryStorage::new()), Default::default());
    let root = session
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let (closing, close_started) = oneshot::channel();
    let closing = parking_lot::Mutex::new(Some(closing));
    let (release_close, wait_close) = std::sync::mpsc::channel();
    let wait_close = parking_lot::Mutex::new(wait_close);
    let _listener = session
        .subscribe_close(move || {
            closing.lock().take().unwrap().send(()).unwrap();
            wait_close.lock().recv().unwrap();
        })
        .unwrap();
    let views = ConversationViews::new(session.clone()).unwrap();
    let (ready, entered) = oneshot::channel();
    let (release_factory, wait_factory) = oneshot::channel();
    let acquire = views.attach(
        root,
        move |value, release, _| async move {
            ready.send(()).unwrap();
            wait_factory.await.unwrap();
            Ok(Arc::new(pi_chord::Watch::with_frames(
                value,
                move || release(),
                |_| {},
                |_| Vec::new(),
            )))
        },
        &Context::default(),
    );
    entered.await.unwrap();
    let runtime = tokio::runtime::Handle::current();
    let close = std::thread::spawn(move || {
        let _entered = runtime.enter();
        session.close(&Context::default())
    });
    close_started.await.unwrap();
    release_factory.send(()).unwrap();
    let result = acquire.await;
    release_close.send(()).unwrap();
    close.join().unwrap().await.unwrap();
    assert!(
        result.is_err(),
        "a sealed Session accepted a new view observer"
    );
}
