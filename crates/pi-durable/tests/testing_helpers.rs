//! The native testing helpers executed against the native environment and the
//! memory, JSONL and SQLite storage backends.
use pi_durable::{
    Context, JsonlStorage, JsonlStorageOptions, MemoryStorage,
    storage::{Storage, sqlite::open_native_sqlite_storage},
    testing::*,
};
use std::sync::Arc;

const SMALL: StorageBenchmarkScale = StorageBenchmarkScale {
    name: "test",
    entry_count: 120,
    task_count: 30,
    document_count: 30,
};

type Made = (Arc<dyn Storage>, Option<tempfile::TempDir>);
async fn memory() -> Made {
    (Arc::new(MemoryStorage::new()), None)
}
async fn jsonl() -> Made {
    let directory = tempfile::tempdir().unwrap();
    let storage = JsonlStorage::open_native(
        directory.path().join("log"),
        &Context::default(),
        JsonlStorageOptions::default(),
    )
    .await
    .unwrap();
    (Arc::new(storage), Some(directory))
}
async fn sqlite() -> Made {
    let directory = tempfile::tempdir().unwrap();
    let storage = open_native_sqlite_storage(directory.path().join("db"), Default::default())
        .await
        .unwrap();
    (Arc::new(storage), Some(directory))
}

fn provider<F, Fut>(make: F) -> StorageConformanceProvider
where
    F: Fn() -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Made> + Send + 'static,
{
    Arc::new(move |body| {
        let make = make.clone();
        Box::pin(async move {
            let (storage, directory) = make().await;
            let result = body(storage.clone()).await;
            let _ = storage.close(&Context::default()).await;
            drop(directory);
            result
        })
    })
}

#[tokio::test]
async fn native_env_passes_the_env_conformance_suite() {
    run_env_conformance(
        "native env",
        &native_env_provider(),
        EnvConformanceOptions {
            list_files: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn the_env_suite_has_every_reference_case() {
    let provider = native_env_provider();
    let count = |list_files| {
        create_env_conformance(
            provider.clone(),
            EnvConformanceOptions {
                list_files,
                ..Default::default()
            },
        )
        .len()
    };
    assert_eq!((count(false), count(true)), (24, 29));
}

#[tokio::test]
async fn memory_passes_storage_conformance() {
    run_storage_conformance("memory", StorageBackend::Memory, &provider(memory))
        .await
        .unwrap();
}
#[tokio::test]
async fn jsonl_passes_storage_conformance() {
    run_storage_conformance("jsonl", StorageBackend::Jsonl, &provider(jsonl))
        .await
        .unwrap();
}
#[tokio::test]
async fn sqlite_passes_storage_conformance() {
    run_storage_conformance("sqlite", StorageBackend::Sqlite, &provider(sqlite))
        .await
        .unwrap();
}

async fn benchmark(
    make: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Made> + Send>>,
) {
    let (read, _read_dir) = make().await;
    let (write, _write_dir) = make().await;
    let executed = verify_storage_benchmarks(read, write, &SMALL)
        .await
        .unwrap();
    assert_eq!(
        executed,
        storage_read_benchmarks().len() + storage_write_benchmarks().len()
    );
}
#[tokio::test]
async fn memory_storage_benchmark_returns_expected_values() {
    benchmark(|| Box::pin(memory())).await;
}
#[tokio::test]
async fn jsonl_storage_benchmark_returns_expected_values() {
    benchmark(|| Box::pin(jsonl())).await;
}
#[tokio::test]
async fn sqlite_storage_benchmark_returns_expected_values() {
    benchmark(|| Box::pin(sqlite())).await;
}

#[test]
fn benchmark_constants_match_the_reference() {
    assert_eq!(TIMING_SCALE.entry_count, 1_000);
    assert_eq!(STORAGE_MEMORY_SCALES.map(|scale| scale.name), ["1k", "10k"]);
    assert_eq!(
        storage_benchmark_primary_record_count(&TIMING_SCALE),
        1 + 1000 + 300 + 300 + 4 + 1 + 8 * 33
    );
}

#[tokio::test]
async fn assertions_report_failures_and_rejections() {
    let a = create_expect_assertions();
    assert!(a.strict_equal(&1, &1).is_ok() && a.strict_equal(&1, &2).is_err());
    assert!(a.greater_than(2.0, 1.0).is_ok() && a.greater_than(1.0, 1.0).is_err());
    assert!(a.ok(false, "x").is_err());
    let failing = async { Err::<(), _>(pi_durable::Error::message("boom here")) };
    assert!(a.rejects(failing, "boom").await.is_ok());
    assert!(a.rejects(async { Ok(()) }, "boom").await.is_err());
    let actual: pi_durable::JsonValue =
        serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
    let want: pi_durable::JsonValue = serde_json::from_str(r#"{"b":{"c":2}}"#).unwrap();
    let wrong: pi_durable::JsonValue = serde_json::from_str(r#"{"b":{"c":9}}"#).unwrap();
    assert!(a.partial_deep_equal(&actual, &want).is_ok());
    assert!(a.partial_deep_equal(&actual, &wrong).is_err());
}
