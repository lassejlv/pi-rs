use pi_chord::sqlite::{SqliteExecutor, cloudflare::*};
use pi_durable::{storage::sqlite::*, testing::replay_case, *};
use std::sync::Arc;
#[path = "../../pi-chord/tests/support/sqlite_host.rs"]
mod sqlite_host;
use sqlite_host::Host;
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
            let host = Arc::new(Host::new());
            let adapter = Arc::new(DurableObjectSqliteStorage::new(host));
            let storage = open_durable_object_sqlite_storage(
                adapter,
                SqliteStorageOptions {
                    prefix: Some("pi_".into()),
                },
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
#[tokio::test]
async fn concurrent_prefixes_share_the_host_queue_and_preserve_host_tables() {
    let host = Arc::new(Host::new());
    let adapter = Arc::new(DurableObjectSqliteStorage::new(host.clone()));
    let database = DurableObjectSqliteDatabase::new(adapter.clone());
    database
        .exec("CREATE TABLE tasks (text TEXT)")
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        open_durable_object_sqlite_storage(
            adapter.clone(),
            SqliteStorageOptions {
                prefix: Some("first_".into())
            }
        ),
        open_durable_object_sqlite_storage(
            adapter,
            SqliteStorageOptions {
                prefix: Some("second_".into())
            }
        ),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    let context = Context::default();
    let writes = [StorageWrite::Conversation {
        value: ConversationRecord {
            id: ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        },
    }];
    let (a, b) = tokio::join!(
        first.commit(&writes, Box::new(|_| {}), &context),
        second.commit(&writes, Box::new(|_| {}), &context)
    );
    assert_eq!(a.unwrap().get(), 1);
    assert_eq!(b.unwrap().get(), 1);
    assert!(
        first
            .conversation(ROOT_CONVERSATION_ID, &context)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        second
            .conversation(ROOT_CONVERSATION_ID, &context)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        host.connection
            .lock()
            .query_row("SELECT count(*) FROM tasks", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    first.close(&context).await.unwrap();
    second.close(&context).await.unwrap();
}
