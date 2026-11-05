use pi_durable::{JsonValue, MemoryStorage, testing::replay_case};
use std::sync::Arc;

fn cases() -> &'static Vec<JsonValue> {
    static CASES: std::sync::OnceLock<Vec<JsonValue>> = std::sync::OnceLock::new();
    CASES.get_or_init(|| {
        let fixture: JsonValue =
            serde_json::from_str(include_str!("fixtures/storage.json")).unwrap();
        fixture["cases"].as_array().unwrap().clone()
    })
}
macro_rules! storage_case {
    ($name:ident,$index:expr) => {
        #[tokio::test]
        async fn $name() {
            let case = &cases()[$index];
            replay_case(Arc::new(MemoryStorage::new()), case)
                .await
                .unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
        }
    };
}
include!("fixtures/storage_cases.rs");
