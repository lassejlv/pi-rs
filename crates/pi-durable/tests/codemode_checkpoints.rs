use pi_durable::{codemode::*, *};
use std::sync::Arc;

#[test]
fn builtin_checkpoint_policies_keep_task_deltas_and_bound_the_store() {
    for definition in [
        CODEMODE_RUN_DOC.definition(),
        CODEMODE_JOURNAL_DOC.definition(),
    ] {
        assert!(
            !definition
                .checkpoint_when(
                    &definition.initial(None).unwrap(),
                    &[],
                    CheckpointInfo {
                        deltas_since_base: 1000
                    },
                )
                .unwrap()
        );
    }
    let nested = harness::tool::NESTED_CALL_DOC.definition();
    assert!(
        !nested
            .checkpoint_when(
                &nested.initial(Some(&JsonValue::Null)).unwrap(),
                &[],
                CheckpointInfo {
                    deltas_since_base: 1000
                },
            )
            .unwrap()
    );
    let definition = CODEMODE_STORE_DOC.definition();
    for count in [0, 63, 64, 65, 1000] {
        assert_eq!(
            definition
                .checkpoint_when(
                    &definition.initial(None).unwrap(),
                    &[],
                    CheckpointInfo {
                        deltas_since_base: count
                    },
                )
                .unwrap(),
            count >= 64
        );
    }
}

async fn set_counter(session: &Session, owner: ConversationId, counter: usize) -> EntryId {
    session
        .commit(
            move |tx| async move {
                tx.doc(&CODEMODE_STORE_DOC, owner).await?.update(|state| {
                    state
                        .values
                        .insert("counter".into(), JsonValue::from(counter));
                    Ok(())
                })?;
                Ok(tx
                    .append_entry(owner, EntryDraft::new("test.counter"))
                    .await?
                    .id)
            },
            &Context::default(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn store_checkpoint_preserves_history_and_fork_state_in_both_retention_modes() {
    for retention in [600000.0, 0.0] {
        let storage = Arc::new(MemoryStorage::new());
        let session = Session::new(
            storage.clone(),
            SessionOptions {
                document_retention_ms: retention,
                ..Default::default()
            },
        );
        let context = Context::default();
        let owner = session
            .commit(
                |tx| async move {
                    Ok(tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?
                        .id)
                },
                &context,
            )
            .await
            .unwrap();
        let mut cutoff = set_counter(&session, owner, 0).await;
        let record = storage
            .find_document(
                &CODEMODE_STORE_DOC.address(owner),
                DocumentPoint::Current,
                &context,
            )
            .await
            .unwrap()
            .unwrap();
        for counter in 1..=64 {
            let entry = set_counter(&session, owner, counter).await;
            if counter == 32 {
                cutoff = entry;
            }
        }
        assert_eq!(
            storage
                .document(record.id(), DocumentPoint::Current, &context)
                .await
                .unwrap()
                .unwrap()
                .deltas_since_base,
            64
        );
        set_counter(&session, owner, 65).await;
        let stored = storage
            .document(record.id(), DocumentPoint::Current, &context)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.deltas_since_base, 0);
        assert_eq!(
            stored.value.value()["values"]["counter"],
            JsonValue::from(65usize)
        );
        assert_eq!(
            session
                .snapshot_as_of(&CODEMODE_STORE_DOC, owner, cutoff, &context)
                .await
                .unwrap()
                .unwrap()
                .values
                .value()["counter"],
            JsonValue::from(32usize)
        );
        let fork = session
            .commit(
                move |tx| async move {
                    tx.fork_conversation(owner, cutoff, ConversationOwnership::Ownerless)
                        .await
                },
                &context,
            )
            .await
            .unwrap()
            .id;
        assert_eq!(
            session
                .snapshot(&CODEMODE_STORE_DOC, fork, &context)
                .await
                .unwrap()
                .unwrap()
                .values
                .value()["counter"],
            JsonValue::from(32usize)
        );
        set_counter(&session, owner, 66).await;
        assert_eq!(
            storage
                .document(record.id(), DocumentPoint::Current, &context)
                .await
                .unwrap()
                .unwrap()
                .deltas_since_base,
            1
        );
        session.close(&context).await.unwrap();
    }
}
