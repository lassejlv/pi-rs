use pi_durable::{storage::sqlite::*, testing::replay_case, *};
use std::sync::Arc;
fn cases() -> &'static Vec<JsonValue> {
    static CASES: std::sync::OnceLock<Vec<JsonValue>> = std::sync::OnceLock::new();
    CASES.get_or_init(|| {
        let fixture: JsonValue =
            serde_json::from_str(include_str!("fixtures/storage-sqlite.json")).unwrap();
        fixture["cases"].as_array().unwrap().clone()
    })
}
macro_rules! storage_case {
    ($name:ident, $index:expr) => {
        #[tokio::test]
        async fn $name() {
            let directory = tempfile::tempdir().unwrap();
            let storage = open_native_sqlite_storage(
                directory.path().join("storage.sqlite"),
                NativeSqliteStorageOptions::default(),
            )
            .await
            .unwrap();
            let case = &cases()[$index];
            let result = replay_case(Arc::new(storage.clone()), case).await;
            storage.close(&Context::default()).await.unwrap();
            result.unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
        }
    };
}
include!("fixtures/storage_sqlite_cases.rs");

fn entry_write(id: i64) -> StorageWrite {
    StorageWrite::Entry {
        value: EntryRecord {
            id: EntryId::from_raw(id),
            conversation_id: ROOT_CONVERSATION_ID,
            kind: "probe".into(),
            model: None,
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        },
    }
}
#[tokio::test]
async fn sql_failure_rolls_back_rows_ids_sequence_and_adoption() {
    use pi_chord::sqlite::{SqliteExecutor, SqliteValue};
    use std::sync::atomic::{AtomicBool, Ordering};
    let db = open_native_sqlite_database(":memory:", Default::default())
        .await
        .unwrap();
    let storage = SqliteStorage::open(Arc::new(db.clone()), Default::default())
        .await
        .unwrap();
    db.exec("CREATE TRIGGER fail_third BEFORE INSERT ON entries WHEN NEW.id = 3 BEGIN SELECT RAISE(ABORT, 'injected row failure'); END").await.unwrap();
    let adopted = Arc::new(AtomicBool::new(false));
    let flag = adopted.clone();
    let error = storage
        .commit(
            &[entry_write(2), entry_write(3)],
            Box::new(move |_| {
                flag.store(true, Ordering::SeqCst);
            }),
            &Context::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected row failure"));
    assert!(!adopted.load(Ordering::SeqCst));
    assert!(
        storage
            .entry(EntryId::from_raw(2), &Context::default())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.get("SELECT count(*) AS count FROM record_ids", &[])
            .await
            .unwrap()
            .unwrap()["count"],
        SqliteValue::Integer(0)
    );
    assert_eq!(
        db.get("SELECT next_id, next_seq FROM durable_metadata", &[])
            .await
            .unwrap()
            .unwrap(),
        [
            ("next_id".into(), SqliteValue::Text("2".into())),
            ("next_seq".into(), SqliteValue::Integer(1))
        ]
        .into_iter()
        .collect::<pi_chord::sqlite::SqliteRow>()
    );
    assert_eq!(storage.mint_id().await.unwrap(), 2);
    assert_eq!(
        storage
            .commit(&[entry_write(4)], Box::new(|_| {}), &Context::default())
            .await
            .unwrap()
            .get(),
        1
    );
    assert_eq!(storage.mint_id().await.unwrap(), 5);
    storage.close(&Context::default()).await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_commit_waiter_keeps_adoption_before_the_next_commit() {
    use parking_lot::Mutex;
    let storage = open_native_sqlite_storage(":memory:", Default::default())
        .await
        .unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = order.clone();
    drop(storage.commit(
        &[entry_write(2)],
        Box::new(move |seq| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            first.lock().push(seq.get());
        }),
        &Context::default(),
    ));
    let second = order.clone();
    storage
        .commit(
            &[entry_write(3)],
            Box::new(move |seq| {
                assert_eq!(*second.lock(), vec![1]);
                second.lock().push(seq.get());
            }),
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(*order.lock(), vec![1, 2]);
    storage.close(&Context::default()).await.unwrap();
}
#[tokio::test]
async fn admitted_native_reads_finish_during_close() {
    let storage = open_native_sqlite_storage(":memory:", Default::default())
        .await
        .unwrap();
    let context = Context::default();
    storage
        .commit(
            &[
                StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: ROOT_CONVERSATION_ID,
                        owner: None,
                        parent: None,
                    },
                },
                entry_write(2),
            ],
            Box::new(|_| {}),
            &context,
        )
        .await
        .unwrap();
    let scan = storage.scan_entries(
        &EntryQuery {
            conversation_id: ROOT_CONVERSATION_ID,
            min_entry_id: None,
            max_entry_id: None,
            order: None,
        },
        10,
        None,
        &context,
    );
    let entry = storage.entry_in(ROOT_CONVERSATION_ID, EntryId::from_raw(2), &context);
    let head = storage.find_latest_head_marker(ROOT_CONVERSATION_ID, None, &context);
    let closing = storage.close(&context);
    let again = storage.close(&context);
    assert_eq!(scan.await.unwrap().items.len(), 1);
    assert_eq!(entry.await.unwrap().unwrap().entry.id.get(), 2);
    assert!(head.await.unwrap().is_none());
    closing.await.unwrap();
    again.await.unwrap();
    assert!(
        storage
            .mint_id()
            .await
            .unwrap_err()
            .to_string()
            .contains("closed")
    );
}
#[tokio::test]
async fn failed_initialization_closes_the_owned_facade_and_preserves_the_error() {
    use pi_chord::sqlite::SqliteExecutor;
    let db = open_native_sqlite_database(":memory:", Default::default())
        .await
        .unwrap();
    let result = SqliteStorage::open(
        Arc::new(db.clone()),
        SqliteStorageOptions {
            prefix: Some("bad-prefix".into()),
        },
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("Invalid SQLite schema prefix")
    );
    assert!(db.get("SELECT 1", &[]).await.is_err());
}

#[tokio::test]
async fn entry_scans_preserve_absent_bounds_in_both_directions() {
    let storage = open_native_sqlite_storage(":memory:", Default::default())
        .await
        .unwrap();
    let context = Context::default();
    storage
        .commit(
            &[
                StorageWrite::Conversation {
                    value: ConversationRecord {
                        id: ROOT_CONVERSATION_ID,
                        parent: None,
                        owner: None,
                    },
                },
                entry_write(-2),
                entry_write(2),
            ],
            Box::new(|_| {}),
            &context,
        )
        .await
        .unwrap();
    for (order, expected) in [
        (ScanOrder::Ascending, vec![-2, 2]),
        (ScanOrder::Descending, vec![2, -2]),
    ] {
        let page = storage
            .scan_entries(
                &EntryQuery {
                    conversation_id: ROOT_CONVERSATION_ID,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: Some(order),
                },
                10,
                None,
                &context,
            )
            .await
            .unwrap();
        assert_eq!(
            page.items
                .iter()
                .map(|entry| entry.id.get())
                .collect::<Vec<_>>(),
            expected
        );
    }
    storage.close(&context).await.unwrap();
    let cursor: Cursor = serde_json::from_str(r#"{"after":false}"#).unwrap();
    assert_eq!(
        storage
            .scan_tasks(&TaskQuery::default(), 10, Some(&cursor), &context)
            .await
            .unwrap_err()
            .to_string(),
        "SqliteStorage is closed"
    );
}
