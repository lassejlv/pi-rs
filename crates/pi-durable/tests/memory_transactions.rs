use pi_chord::context::BACKGROUND_CONTEXT;
use pi_durable::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn writes(source: &str) -> Vec<StorageWrite> {
    serde_json::from_str(source).unwrap()
}
fn object(source: &str) -> JsonObject {
    serde_json::from_str(source).unwrap()
}
fn adoption() -> Box<dyn FnOnce(Seq) + Send> {
    Box::new(|_| {})
}

#[tokio::test]
async fn prepared_batch_stays_invisible_until_apply_and_applies_once() {
    let storage = MemoryStorage::new();
    let mut input = writes(
        r#"[{"type":"conversation","value":{"id":1}},{"type":"entry","value":{"id":100,"conversationId":1,"kind":"note","data":{"count":1}}}]"#,
    );
    let prepared = storage
        .prepare_commit(&input, Some(Seq::from_raw(9)))
        .unwrap();
    assert_eq!(prepared.seq(), Seq::from_raw(9));
    assert!(
        storage
            .conversation(ROOT_CONVERSATION_ID, &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        storage
            .entry(EntryId::from_raw(100), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(storage.mint_id().await.unwrap(), 2);
    if let StorageWrite::Entry { value } = &mut input[1] {
        value.data = Some(JsonValue::Null);
    }
    assert_eq!(prepared.apply().unwrap(), Seq::from_raw(9));
    assert_eq!(prepared.apply().unwrap(), Seq::from_raw(9));
    let entry = storage
        .entry(EntryId::from_raw(100), &BACKGROUND_CONTEXT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.commit_seq, Seq::from_raw(9));
    assert_eq!(
        entry.entry.data,
        Some(object(r#"{"count":1}"#).into_value())
    );
    assert_eq!(storage.mint_id().await.unwrap(), 101);
    assert_eq!(
        storage
            .commit(&[], adoption(), &BACKGROUND_CONTEXT)
            .await
            .unwrap(),
        Seq::from_raw(10)
    );
}

#[tokio::test]
async fn failed_preparation_changes_neither_tables_sequence_ids_nor_adoption() {
    let storage = MemoryStorage::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let input = writes(
        r#"[{"type":"conversation","value":{"id":1}},{"type":"entry","value":{"id":100,"conversationId":1,"kind":"note"}},{"type":"document.change","id":200,"content":{"kind":"delta","version":1,"ops":[]}}]"#,
    );
    let error = storage
        .commit(
            &input,
            {
                let calls = calls.clone();
                Box::new(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                })
            },
            &BACKGROUND_CONTEXT,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Unknown document: 200");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        storage
            .conversation(ROOT_CONVERSATION_ID, &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        storage
            .entry(EntryId::from_raw(100), &BACKGROUND_CONTEXT)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(storage.mint_id().await.unwrap(), 2);
    assert_eq!(
        storage
            .commit(&[], adoption(), &BACKGROUND_CONTEXT)
            .await
            .unwrap(),
        Seq::from_raw(1)
    );
}

#[tokio::test]
async fn concurrent_commits_serialize_adoption_and_reads_can_run_inside_it() {
    let storage = Arc::new(MemoryStorage::new());
    let adopted = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for index in 0..32 {
        let storage = storage.clone();
        let adopted = adopted.clone();
        workers.push(std::thread::spawn(move || {
            let id = ConversationId::from_raw(index + 1);
            let value = ConversationRecord {
                id,
                parent: None,
                owner: None,
            };
            let observed = storage.clone();
            futures::executor::block_on(storage.commit(
                &[StorageWrite::Conversation { value }],
                Box::new(move |seq| {
                    let read =
                        futures::executor::block_on(observed.conversation(id, &BACKGROUND_CONTEXT))
                            .unwrap();
                    assert!(read.is_some());
                    adopted.lock().push(seq);
                }),
                &BACKGROUND_CONTEXT,
            ))
            .unwrap();
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        *adopted.lock(),
        (1..=32).map(Seq::from_raw).collect::<Vec<_>>()
    );
}

#[test]
fn competing_preparations_cannot_revert_a_later_committed_prefix() {
    let storage = MemoryStorage::new();
    let first = storage
        .prepare_commit(
            &writes(r#"[{"type":"conversation","value":{"id":1}}]"#),
            None,
        )
        .unwrap();
    let competing = storage.prepare_commit(&[], None).unwrap();
    first.apply().unwrap();
    assert_eq!(
        competing.apply().unwrap_err().to_string(),
        "Prepared memory commit is stale"
    );
    assert!(
        storage
            .prepare_commit(&[], Some(Seq::from_raw(1)))
            .err()
            .expect("expected a rejected sequence")
            .to_string()
            .contains("strictly increase")
    );
}
