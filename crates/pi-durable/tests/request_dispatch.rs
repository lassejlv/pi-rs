use pi_durable::{testing::storage_request, *};
use std::sync::Arc;
#[tokio::test]
async fn requests_start_in_dispatch_order_even_if_their_waiters_are_observed_later() {
    let directory = tempfile::tempdir().unwrap();
    for storage in [
        Arc::new(MemoryStorage::new()) as Arc<dyn Storage>,
        Arc::new(
            JsonlStorage::open_native(
                directory.path(),
                &Context::default(),
                JsonlStorageOptions::default(),
            )
            .await
            .unwrap(),
        ),
    ] {
        let conversation = ConversationRecord {
            id: ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        };
        storage
            .commit(
                &[StorageWrite::Conversation {
                    value: conversation.clone(),
                }],
                Box::new(|_| {}),
                &Context::default(),
            )
            .await
            .unwrap();
        let read = storage_request(
            storage.as_ref(),
            "conversation",
            &[1.into()],
            Box::new(|_| {}),
        );
        let close = storage_request(storage.as_ref(), "close", &[], Box::new(|_| {}));
        close.await.unwrap();
        assert_eq!(
            read.await.unwrap(),
            Some(pi_chord::json::to_value(&conversation).unwrap())
        );
        assert!(
            storage_request(
                storage.as_ref(),
                "conversation",
                &[1.into()],
                Box::new(|_| {})
            )
            .await
            .is_err()
        );
    }
}
