#[allow(dead_code)]
#[path = "support/session_storage.rs"]
mod support;
use parking_lot::Mutex;
use pi_chord::{delta::Op, json::parse};
use pi_durable::*;
use std::sync::Arc;
use support::ControlledStorage;

#[tokio::test]
async fn storage_publication_and_snapshots_retain_native_revisions_and_operation_payloads() {
    let storage = Arc::new(ControlledStorage::default());
    let session = Session::new(storage.clone(), SessionOptions::default());
    let definition = Arc::new(
        AnyDocDefinition::new(
            "shared",
            1,
            DefinitionScope::Session,
            false,
            |_| {
                Ok(JsonObject::try_from_value(
                    parse(r#"{"items":[1,2],"other":{"label":"old"}}"#).unwrap(),
                )
                .unwrap())
            },
            |_, _, _| Ok(false),
        )
        .unwrap(),
    );
    let address = DocumentAddress {
        kind: "shared".into(),
        scope: DocumentScope::Session,
        key: None,
    };
    let (def, addr) = (definition.clone(), address.clone());
    let context = Context::default();
    session
        .commit(
            move |tx| async move {
                tx.doc_at(&def, addr, None).await?;
                Ok(())
            },
            &context,
        )
        .await
        .unwrap();
    let before = session
        .snapshot_raw(definition.clone(), address.clone(), &context)
        .await
        .unwrap()
        .unwrap();
    let watch = session
        .watch_raw(definition.clone(), address.clone(), &context)
        .await
        .unwrap()
        .unwrap();
    let (delivered, frame) = tokio::sync::oneshot::channel();
    let delivered = Arc::new(Mutex::new(Some(delivered)));
    watch
        .start(move |frame| {
            if let Some(delivered) = delivered.lock().take() {
                delivered.send(frame).ok();
            }
            async { Ok(()) }
        })
        .unwrap();
    {
        let writes = storage.writes.lock();
        let StorageWrite::DocumentCreate { content, .. } = &writes.last().unwrap()[0] else {
            panic!("expected a base")
        };
        assert!(std::ptr::eq(
            content.value.value().as_object().unwrap(),
            before.as_object().unwrap()
        ));
    }
    let publications = Arc::new(Mutex::new(Vec::new()));
    let received = publications.clone();
    let _subscription = session
        .subscribe_commits(move |publication, _| {
            received.lock().push(publication.clone());
            Ok(())
        })
        .unwrap();
    let (def, addr) = (definition.clone(), address.clone());
    session
        .commit(
            move |tx| async move {
                tx.doc_at(&def, addr, None).await?.apply_ops(&[Op::Set(
                    vec!["other".into()],
                    parse(r#"{"label":"new"}"#).unwrap(),
                )])?;
                Ok(())
            },
            &context,
        )
        .await
        .unwrap();
    let after = session
        .snapshot_raw(definition, address, &context)
        .await
        .unwrap()
        .unwrap();
    let publication = publications.lock().last().unwrap().clone();
    let CommitChange::Document(DocumentCommitChange::Document { value, ops, .. }) =
        &publication.changes[0]
    else {
        panic!("expected a document")
    };
    let frame = frame.await.unwrap();
    assert!(Arc::ptr_eq(&frame.ops, ops));
    {
        let writes = storage.writes.lock();
        let StorageWrite::DocumentChange {
            content: DocumentContent::Delta { ops: persisted, .. },
            ..
        } = &writes.last().unwrap()[0]
        else {
            panic!("expected a delta")
        };
        assert!(Arc::ptr_eq(value, &after));
        assert!(Arc::ptr_eq(persisted, ops));
        let Op::Set(_, payload) = &ops[0] else {
            panic!("expected a Set")
        };
        assert!(std::ptr::eq(
            payload.as_object().unwrap(),
            after["other"].as_object().unwrap()
        ));
        assert!(std::ptr::eq(
            before["items"].as_array().unwrap(),
            after["items"].as_array().unwrap()
        ));
        assert!(before["other"]["label"].string_equals("old"));
    }
    watch.stop();
    session.close(&context).await.unwrap();
}

#[tokio::test]
async fn running_document_watch_owns_its_source_and_stop_releases_it() {
    let marker = Arc::new(());
    let retained = Arc::downgrade(&marker);
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            document_retention_ms: f64::INFINITY,
            report: Arc::new(move |_| {
                let _held = &marker;
            }),
            ..Default::default()
        },
    );
    let doc = define_doc(DocDefinition::new(
        "owned.watch",
        1,
        SessionScope,
        || Ok(JsonObject::try_from_value(parse(r#"{"count":0}"#).unwrap()).unwrap()),
        |_, _, _| false,
    ))
    .unwrap();
    let context = Context::default();
    let token = doc.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?;
                Ok(())
            },
            &context,
        )
        .await
        .unwrap();
    let watch = session
        .watch_doc(&doc, (), &context)
        .await
        .unwrap()
        .unwrap();
    drop(session);
    assert!(
        retained.upgrade().is_some(),
        "running watch lost its source"
    );
    watch.stop();
    assert!(
        retained.upgrade().is_none(),
        "stopped watch retained its source"
    );
    assert_eq!(
        watch.value().unwrap().unwrap().value()["count"].as_u64(),
        Some(0)
    );
}

#[tokio::test]
async fn dropping_the_last_document_watch_releases_its_cache_pin() {
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            document_retention_ms: f64::INFINITY,
            ..Default::default()
        },
    );
    let doc = define_doc(DocDefinition::new(
        "dropped.watch",
        1,
        SessionScope,
        || Ok(JsonObject::try_from_value(parse(r#"{"count":0}"#).unwrap()).unwrap()),
        |_, _, _| false,
    ))
    .unwrap();
    let context = Context::default();
    let token = doc.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?;
                Ok(())
            },
            &context,
        )
        .await
        .unwrap();
    let watch = session
        .watch_doc(&doc, (), &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.cache_sizes().attached, 1);
    let other = watch.clone();
    drop(watch);
    assert_eq!(session.cache_sizes().attached, 1);
    drop(other);
    assert_eq!(session.cache_sizes().attached, 0);
    session.close(&context).await.unwrap();
}

#[tokio::test]
async fn checkpoint_retains_the_prepared_value_and_batch_for_both_storage_decisions() {
    for base in [false, true] {
        let storage = Arc::new(ControlledStorage::default());
        let session = Session::new(storage.clone(), SessionOptions::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let received = calls.clone();
        let definition = Arc::new(
            AnyDocDefinition::new_with_shared_checkpoint(
                "policy.shared",
                1,
                DefinitionScope::Session,
                false,
                |_| Ok(JsonObject::try_from_value(parse(r#"{"count":0}"#).unwrap()).unwrap()),
                move |value, ops, info| {
                    received
                        .lock()
                        .push((value.clone(), ops.clone(), info.deltas_since_base));
                    Ok(base)
                },
            )
            .unwrap(),
        );
        let address = DocumentAddress {
            kind: "policy.shared".into(),
            scope: DocumentScope::Session,
            key: None,
        };
        let context = Context::default();
        let (def, addr) = (definition.clone(), address.clone());
        session
            .commit(
                move |tx| async move {
                    tx.doc_at(&def, addr, None).await?;
                    Ok(())
                },
                &context,
            )
            .await
            .unwrap();
        assert!(
            calls.lock().is_empty(),
            "creation does not consult the policy"
        );
        let publications = Arc::new(Mutex::new(Vec::new()));
        let received = publications.clone();
        let _subscription = session
            .subscribe_commits(move |publication, _| {
                received.lock().push(publication.clone());
                Ok(())
            })
            .unwrap();
        let (def, addr) = (definition.clone(), address.clone());
        session
            .commit(
                move |tx| async move {
                    tx.doc_at(&def, addr, None)
                        .await?
                        .apply_ops(&[Op::Set(vec!["count".into()], 1.into())])?;
                    Ok(())
                },
                &context,
            )
            .await
            .unwrap();
        let snapshot = session
            .snapshot_raw(definition, address, &context)
            .await
            .unwrap()
            .unwrap();
        {
            let calls = calls.lock();
            assert_eq!(calls.len(), 1);
            let (value, ops, count) = &calls[0];
            assert_eq!(*count, 0);
            assert!(std::ptr::eq(
                value.value().as_object().unwrap(),
                snapshot.as_object().unwrap()
            ));
            let publications = publications.lock();
            let CommitChange::Document(DocumentCommitChange::Document {
                value: published,
                ops: published_ops,
                ..
            }) = &publications[0].changes[0]
            else {
                panic!("expected a document publication")
            };
            assert!(Arc::ptr_eq(&snapshot, published));
            assert!(Arc::ptr_eq(ops, published_ops));
            let writes = storage.writes.lock();
            let StorageWrite::DocumentChange { content, .. } = &writes.last().unwrap()[0] else {
                panic!("expected a document change")
            };
            match (base, content) {
                (false, DocumentContent::Delta { ops: stored, .. }) => {
                    assert!(Arc::ptr_eq(ops, stored));
                }
                (true, DocumentContent::Base { value: stored, .. }) => {
                    assert!(std::ptr::eq(
                        value.value().as_object().unwrap(),
                        stored.value().as_object().unwrap()
                    ));
                }
                _ => panic!("checkpoint decision was not respected"),
            }
        }
        session.close(&context).await.unwrap();
    }
}
