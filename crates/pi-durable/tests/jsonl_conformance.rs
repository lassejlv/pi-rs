#[path = "support/reopening_jsonl.rs"]
mod support;
use pi_durable::{testing::replay_case, *};
use std::sync::{Arc, OnceLock};
fn cases(reopen: bool) -> &'static Vec<JsonValue> {
    static NORMAL: OnceLock<Vec<JsonValue>> = OnceLock::new();
    static REOPEN: OnceLock<Vec<JsonValue>> = OnceLock::new();
    let (slot, fixture) = if reopen {
        (&REOPEN, include_str!("fixtures/storage-jsonl-reopen.json"))
    } else {
        (&NORMAL, include_str!("fixtures/storage-jsonl.json"))
    };
    slot.get_or_init(|| {
        serde_json::from_str::<JsonValue>(fixture).unwrap()["cases"]
            .as_array()
            .unwrap()
            .clone()
    })
}
async fn run_case(index: usize, reopen: bool) {
    for fsync in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let options = JsonlStorageOptions { fsync };
        let storage = JsonlStorage::open_native(directory.path(), &Context::default(), options)
            .await
            .unwrap();
        let storage: Arc<dyn Storage> = if reopen {
            Arc::new(support::ReopeningJsonl::new(storage, options))
        } else {
            Arc::new(storage)
        };
        replay_case(storage.clone(), &cases(reopen)[index])
            .await
            .unwrap_or_else(|e| panic!("fsync={fsync} reopen={reopen}: {e}"));
        storage.close(&Context::default()).await.unwrap();
    }
}
macro_rules! storage_case {
    ($name:ident,$index:expr) => {
        #[tokio::test]
        async fn $name() {
            super::run_case($index, REOPEN).await;
        }
    };
}
mod normal {
    const REOPEN: bool = false;
    include!("fixtures/storage_jsonl_cases.rs");
}
mod reopening {
    const REOPEN: bool = true;
    include!("fixtures/storage_jsonl_reopen_cases.rs");
}
