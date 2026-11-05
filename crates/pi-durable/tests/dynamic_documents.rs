use parking_lot::Mutex;
use pi_chord::{delta::Op, json::parse};
use pi_durable::*;
use std::sync::Arc;

fn value(text: &str) -> JsonValue {
    parse(text).unwrap()
}
fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(value(text)).unwrap()
}

#[tokio::test]
async fn dynamic_drafts_preserve_structural_noops_through_persistence_and_publication() {
    let storage = Arc::new(MemoryStorage::default());
    let session = Session::new(storage.clone(), SessionOptions::default());
    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    let received = checkpoints.clone();
    let definition = Arc::new(
        AnyDocDefinition::new(
            "dynamic",
            1,
            DefinitionScope::Session,
            false,
            |_| Ok(object(r#"{"items":[1,2]}"#)),
            move |_, ops, info| {
                received.lock().push((ops.to_vec(), info.deltas_since_base));
                Ok(false)
            },
        )
        .unwrap(),
    );
    let address = DocumentAddress {
        kind: "dynamic".into(),
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
    let before = session
        .snapshot_raw(definition.clone(), address.clone(), &context)
        .await
        .unwrap()
        .unwrap();
    let publications = Arc::new(Mutex::new(Vec::new()));
    let received = publications.clone();
    let _subscription = session
        .subscribe_commits(move |publication, _| {
            received.lock().push(publication.clone());
            Ok(())
        })
        .unwrap();
    let watch = session
        .watch_raw(definition.clone(), address.clone(), &context)
        .await
        .unwrap()
        .unwrap();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let received = frames.clone();
    watch
        .start(move |frame| {
            received.lock().push((frame.value, frame.ops));
            async { Ok(()) }
        })
        .unwrap();
    let ops: Vec<Op> =
        serde_json::from_str(r#"[["p",["items"],0,1,[]],["p",["items"],0,0,[1]]]"#).unwrap();
    let edits = ops.clone();
    let (def, addr) = (definition.clone(), address.clone());
    let (retained, transaction) = session
        .commit(
            move |tx| async move {
                let draft = tx.doc_at(&def, addr.clone(), None).await?;
                let again = tx.doc_at(&def, addr, None).await?;
                assert!(draft.same_draft(&again));
                assert!(draft.apply_ops(&[Op::Replace(JsonValue::Null)]).is_err());
                assert!(
                    draft
                        .apply_ops(&[Op::Append(vec!["items".into()], "invalid".into())])
                        .is_err()
                );
                assert_eq!(draft.read()?, object(r#"{"items":[1,2]}"#));
                draft.apply_ops(&edits)?;
                Ok((draft, tx))
            },
            &context,
        )
        .await
        .unwrap();
    assert!(retained.read().is_err());
    assert!(retained.apply_ops(&[]).is_err());
    let mut invalid = address.clone();
    invalid.kind = "wrong".into();
    assert!(
        transaction
            .doc_at(&definition, invalid, None)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("settled")
    );
    let after = session
        .snapshot_raw(definition.clone(), address.clone(), &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
    assert!(!Arc::ptr_eq(&before, &after));
    assert_eq!(*checkpoints.lock(), vec![(ops.clone(), 0)]);
    let publication = publications.lock().last().unwrap().clone();
    assert!(
        matches!(&publication.changes[..], [CommitChange::Document(DocumentCommitChange::Document { ops: published, value, .. })] if published.as_ref() == &ops && Arc::ptr_eq(value, &after))
    );
    watch.delivery_idle().await;
    assert!(
        matches!(&frames.lock()[..], [(value, published)] if published.as_ref() == ops.as_slice() && Arc::ptr_eq(value, &after))
    );
    watch.stop();
    let stored = storage
        .find_document(&address, DocumentPoint::Current, &context)
        .await
        .unwrap()
        .unwrap();
    let stored = storage
        .document(stored.id(), DocumentPoint::Current, &context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.value, object(r#"{"items":[1,2]}"#));
    session.close(&context).await.unwrap();
}

#[tokio::test]
async fn dynamic_document_boundaries_validate_scope_family_and_version() {
    assert!(
        AnyDocDefinition::new(
            "bad",
            0,
            DefinitionScope::Session,
            false,
            |_| Ok(JsonObject::default()),
            |_, _, _| Ok(false)
        )
        .is_err()
    );
    let definition = Arc::new(
        AnyDocDefinition::new(
            "family",
            1,
            DefinitionScope::Task,
            true,
            |_| Ok(JsonObject::default()),
            |_, _, _| Ok(false),
        )
        .unwrap(),
    );
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions::default(),
    );
    let address = DocumentAddress {
        kind: "family".into(),
        scope: DocumentScope::Session,
        key: Some("key".into()),
    };
    let context = Context::default();
    assert!(
        session
            .snapshot_raw(definition.clone(), address.clone(), &context)
            .await
            .is_err()
    );
    assert!(
        session
            .watch_raw(definition.clone(), address.clone(), &context)
            .await
            .is_err()
    );
    assert!(
        session
            .snapshot_as_of_raw(
                definition.clone(),
                address.clone(),
                EntryId::from_raw(1),
                &context
            )
            .await
            .is_err()
    );
    assert!(
        session
            .commit(
                move |tx| async move { tx.doc_at(&definition, address, None).await },
                &context
            )
            .await
            .is_err()
    );
    session.close(&context).await.unwrap();
}
