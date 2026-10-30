//! Deterministic storage benchmark datasets and measured operations, written
//! only against the public `Storage` contract.

use crate::{
    ConversationId, DocumentAddress, DocumentId, DocumentPoint, DocumentScope, EntryId, EntryQuery,
    Error, ROOT_CONVERSATION_ID, Result, Seq, StorageWrite, TaskQuery, TaskStatus,
    storage::{Adopt, Storage},
    wire,
};
use pi_chord::context::BACKGROUND_CONTEXT;
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};

/// Storage-level commits here have no in-memory owner to adopt them.
fn adopt() -> Adopt {
    Box::new(|_| {})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageBenchmarkScale {
    pub name: &'static str,
    pub entry_count: usize,
    pub task_count: usize,
    pub document_count: usize,
}

pub const STORAGE_MEMORY_SCALES: [StorageBenchmarkScale; 2] = [
    StorageBenchmarkScale {
        name: "1k",
        entry_count: 1_000,
        task_count: 200,
        document_count: 200,
    },
    StorageBenchmarkScale {
        name: "10k",
        entry_count: 10_000,
        task_count: 2_000,
        document_count: 2_000,
    },
];

pub const TIMING_SCALE: StorageBenchmarkScale = StorageBenchmarkScale {
    name: "timing",
    entry_count: 1_000,
    task_count: 300,
    document_count: 300,
};

const REPLAY_TAILS: [usize; 4] = [0, 16, 128, 1_024];
const HISTORY_SEGMENT_LENGTH: usize = 128;
const FORK_DEPTH: usize = 8;
const ENTRIES_PER_FORK: usize = 32;
const BATCH_SIZE: usize = 100;

pub fn storage_benchmark_primary_record_count(scale: &StorageBenchmarkScale) -> usize {
    1 + scale.entry_count
        + scale.task_count
        + scale.document_count
        + REPLAY_TAILS.len()
        + 1
        + FORK_DEPTH * (1 + ENTRIES_PER_FORK)
}

#[derive(Clone, Debug)]
pub struct StorageBenchmarkDataset {
    pub first_entry_id: EntryId,
    pub filtered_task_count: usize,
    pub exact_document_id: DocumentId,
    pub exact_document_key: String,
    /// Replay document per tail length in `REPLAY_TAILS` order.
    pub replay_document_ids: Vec<(usize, DocumentId)>,
    pub historical_document_id: DocumentId,
    pub ancient_at: Seq,
    pub recent_at: Seq,
    pub deepest_conversation_id: ConversationId,
    pub ancestor_head_entry_id: EntryId,
}

fn decoded(value: Value) -> Result<StorageWrite> {
    serde_json::from_value(value).map_err(|error| Error::message(error.to_string()))
}
async fn commit(storage: &dyn Storage, writes: Vec<Value>) -> Result<Seq> {
    let writes = writes
        .into_iter()
        .map(decoded)
        .collect::<Result<Vec<_>>>()?;
    storage.commit(&writes, adopt(), &BACKGROUND_CONTEXT).await
}
async fn mint(storage: &dyn Storage) -> Result<i64> {
    storage.mint_id().await
}

fn task(id: i64, index: usize) -> Value {
    let common = |state: Value| {
        json!({
            "id": id,
            "conversationId": ROOT_CONVERSATION_ID.get(),
            "kind": if index.is_multiple_of(4) { "benchmark.filtered" } else { "benchmark.other" },
            "version": 1,
            "input": { "index": index },
            "background": index.is_multiple_of(5),
            "abortRequested": index.is_multiple_of(7),
            "state": state,
        })
    };
    match index % 3 {
        0 => common(
            json!({ "status": "pending", "checkpoint": { "index": index, "payload": "x".repeat(64) } }),
        ),
        1 => common(
            json!({ "status": "running", "checkpoint": { "index": index, "payload": "x".repeat(64) } }),
        ),
        _ => common(json!({
            "status": "terminal",
            "outcome": { "status": "completed", "result": { "index": index } },
        })),
    }
}

fn root_conversation() -> Value {
    json!({ "type": "conversation", "value": { "id": ROOT_CONVERSATION_ID.get() } })
}

/// Seed deterministic representative data through only the public `Storage` contract.
pub async fn seed_storage_benchmark(
    storage: &dyn Storage,
    scale: &StorageBenchmarkScale,
) -> Result<StorageBenchmarkDataset> {
    commit(storage, vec![root_conversation()]).await?;

    let mut first_entry_id = None;
    for start in (0..scale.entry_count).step_by(BATCH_SIZE) {
        let mut writes = Vec::new();
        for index in start..(start + BATCH_SIZE).min(scale.entry_count) {
            let id = mint(storage).await?;
            let mut value = json!({
                "id": id,
                "conversationId": ROOT_CONVERSATION_ID.get(),
                "kind": "benchmark.entry",
                "data": { "index": index, "text": format!("entry-{index}-{}", "x".repeat(96)) },
            });
            if index == 0 {
                first_entry_id = Some(id);
                value["head"] = json!(id);
            }
            writes.push(json!({ "type": "entry", "value": value }));
        }
        commit(storage, writes).await?;
    }

    for start in (0..scale.task_count).step_by(BATCH_SIZE) {
        let mut writes = Vec::new();
        for index in start..(start + BATCH_SIZE).min(scale.task_count) {
            writes.push(json!({ "type": "task", "value": task(mint(storage).await?, index) }));
        }
        commit(storage, writes).await?;
    }

    let mut exact_document_id = None;
    for start in (0..scale.document_count).step_by(BATCH_SIZE) {
        let mut writes = Vec::new();
        for index in start..(start + BATCH_SIZE).min(scale.document_count) {
            let id = mint(storage).await?;
            exact_document_id = Some(id);
            writes.push(json!({
                "type": "document.create",
                "record": { "id": id, "kind": "benchmark.family", "key": format!("key-{index}"), "scope": { "kind": "session" } },
                "content": { "kind": "base", "version": 1, "value": { "index": index, "text": "x".repeat(128) } },
            }));
        }
        commit(storage, writes).await?;
    }

    let mut replay = Vec::new();
    for tail in REPLAY_TAILS {
        replay.push((tail, mint(storage).await?));
    }
    commit(
        storage,
        replay
            .iter()
            .map(|(tail, id)| {
                json!({
                    "type": "document.create",
                    "record": {
                        "id": id,
                        "kind": "benchmark.replay",
                        "key": tail.to_string(),
                        "scope": { "kind": "conversation", "conversationId": ROOT_CONVERSATION_ID.get() },
                        "history": "rewindable",
                        "fork": "asOf",
                    },
                    "content": { "kind": "base", "version": 1, "value": { "count": 0, "text": "x".repeat(64) } },
                })
            })
            .collect(),
    )
    .await?;
    for count in 1..=REPLAY_TAILS[REPLAY_TAILS.len() - 1] {
        let writes: Vec<Value> = replay
            .iter()
            .filter(|(tail, _)| count <= *tail)
            .map(|(_, id)| {
                json!({
                    "type": "document.change",
                    "id": id,
                    "content": { "kind": "delta", "version": 1, "ops": [["s", ["count"], count]] },
                })
            })
            .collect();
        commit(storage, writes).await?;
    }

    let historical = mint(storage).await?;
    commit(
        storage,
        vec![json!({
            "type": "document.create",
            "record": {
                "id": historical,
                "kind": "benchmark.history",
                "scope": { "kind": "conversation", "conversationId": ROOT_CONVERSATION_ID.get() },
                "history": "rewindable",
                "fork": "asOf",
            },
            "content": { "kind": "base", "version": 1, "value": { "count": 0 } },
        })],
    )
    .await?;
    let delta = |count: usize| {
        json!({
            "type": "document.change",
            "id": historical,
            "content": { "kind": "delta", "version": 1, "ops": [["s", ["count"], count]] },
        })
    };
    let mut ancient_at = None;
    for count in 1..=HISTORY_SEGMENT_LENGTH {
        ancient_at = Some(commit(storage, vec![delta(count)]).await?);
    }
    commit(
        storage,
        vec![json!({
            "type": "document.change",
            "id": historical,
            "content": { "kind": "base", "version": 1, "value": { "count": HISTORY_SEGMENT_LENGTH } },
        })],
    )
    .await?;
    let ancient_at =
        ancient_at.ok_or_else(|| Error::message("Benchmark history seed produced no commits"))?;
    let mut recent_at = ancient_at;
    for count in HISTORY_SEGMENT_LENGTH + 1..=HISTORY_SEGMENT_LENGTH * 2 {
        recent_at = commit(storage, vec![delta(count)]).await?;
    }

    let first_entry_id =
        first_entry_id.ok_or_else(|| Error::message("Benchmark scale must create entries"))?;
    let mut parent_conversation = ROOT_CONVERSATION_ID.get();
    let mut parent_at = first_entry_id;
    let mut deepest = ROOT_CONVERSATION_ID.get();
    for depth in 0..FORK_DEPTH {
        let conversation = mint(storage).await?;
        commit(
            storage,
            vec![json!({
                "type": "conversation",
                "value": { "id": conversation, "parent": { "conversationId": parent_conversation, "at": parent_at } },
            })],
        )
        .await?;
        let mut ids = Vec::new();
        for _ in 0..ENTRIES_PER_FORK {
            ids.push(mint(storage).await?);
        }
        commit(
            storage,
            ids.iter()
                .enumerate()
                .map(|(index, id)| {
                    json!({
                        "type": "entry",
                        "value": {
                            "id": id,
                            "conversationId": conversation,
                            "kind": "benchmark.fork",
                            "data": { "depth": depth, "index": index },
                        },
                    })
                })
                .collect(),
        )
        .await?;
        parent_conversation = conversation;
        parent_at = ids[ids.len() - 1];
        deepest = conversation;
    }

    let exact_document_id =
        exact_document_id.ok_or_else(|| Error::message("Benchmark scale must create documents"))?;
    Ok(StorageBenchmarkDataset {
        first_entry_id: EntryId::from_raw(first_entry_id),
        filtered_task_count: 50.min(scale.task_count.div_ceil(60)),
        exact_document_id: DocumentId::from_raw(exact_document_id),
        exact_document_key: format!("key-{}", scale.document_count - 1),
        replay_document_ids: replay
            .into_iter()
            .map(|(tail, id)| (tail, DocumentId::from_raw(id)))
            .collect(),
        historical_document_id: DocumentId::from_raw(historical),
        ancient_at,
        recent_at,
        deepest_conversation_id: ConversationId::from_raw(deepest),
        ancestor_head_entry_id: EntryId::from_raw(first_entry_id),
    })
}

pub type BenchmarkFuture = Pin<Box<dyn Future<Output = Result<i64>> + Send + 'static>>;

pub struct StorageReadBenchmark {
    pub name: String,
    run: Box<
        dyn Fn(Arc<dyn Storage>, Arc<StorageBenchmarkDataset>) -> BenchmarkFuture + Send + Sync,
    >,
    expected: Box<dyn Fn(&StorageBenchmarkDataset) -> i64 + Send + Sync>,
}
impl StorageReadBenchmark {
    pub fn run(
        &self,
        storage: Arc<dyn Storage>,
        dataset: Arc<StorageBenchmarkDataset>,
    ) -> BenchmarkFuture {
        (self.run)(storage, dataset)
    }
    pub fn expected(&self, dataset: &StorageBenchmarkDataset) -> i64 {
        (self.expected)(dataset)
    }
}
fn read_benchmark<F, Fut>(
    name: impl Into<String>,
    run: F,
    expected: impl Fn(&StorageBenchmarkDataset) -> i64 + Send + Sync + 'static,
) -> StorageReadBenchmark
where
    F: Fn(Arc<dyn Storage>, Arc<StorageBenchmarkDataset>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<i64>> + Send + 'static,
{
    StorageReadBenchmark {
        name: name.into(),
        run: Box::new(move |storage, dataset| Box::pin(run(storage, dataset))),
        expected: Box::new(expected),
    }
}
fn count_of(document: Option<crate::StoredDocument>) -> i64 {
    document
        .and_then(|document| wire::field::<i64>(document.value.value(), "count").ok())
        .unwrap_or(-1)
}

pub fn storage_read_benchmarks() -> Vec<StorageReadBenchmark> {
    let mut benchmarks = vec![
        read_benchmark(
            "exact entry lookup",
            |storage, dataset| async move {
                Ok(storage
                    .entry(dataset.first_entry_id, &BACKGROUND_CONTEXT)
                    .await?
                    .map_or(-1, |stored| stored.entry.id.get()))
            },
            |dataset| dataset.first_entry_id.get(),
        ),
        read_benchmark(
            "entry page scan (100)",
            |storage, _| async move {
                let query = EntryQuery {
                    conversation_id: ROOT_CONVERSATION_ID,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                };
                Ok(storage
                    .scan_entries(&query, 100, None, &BACKGROUND_CONTEXT)
                    .await?
                    .items
                    .len() as i64)
            },
            |_| 100,
        ),
        read_benchmark(
            "filtered task scan (50)",
            |storage, _| async move {
                let query = TaskQuery {
                    kind: Some("benchmark.filtered".into()),
                    status: Some(TaskStatus::Pending),
                    background: Some(true),
                    ..Default::default()
                };
                Ok(storage
                    .scan_tasks(&query, 50, None, &BACKGROUND_CONTEXT)
                    .await?
                    .items
                    .len() as i64)
            },
            |dataset| dataset.filtered_task_count as i64,
        ),
        read_benchmark(
            "exact document address among many",
            |storage, dataset| async move {
                let address = DocumentAddress {
                    kind: "benchmark.family".into(),
                    scope: DocumentScope::Session,
                    key: Some(dataset.exact_document_key.as_str().into()),
                };
                Ok(storage
                    .find_document(&address, DocumentPoint::Current, &BACKGROUND_CONTEXT)
                    .await?
                    .map_or(-1, |record| record.id().get()))
            },
            |dataset| dataset.exact_document_id.get(),
        ),
    ];
    for tail in REPLAY_TAILS {
        benchmarks.push(read_benchmark(
            format!("document replay tail ({tail})"),
            move |storage, dataset| async move {
                let id = dataset
                    .replay_document_ids
                    .iter()
                    .find(|(candidate, _)| *candidate == tail)
                    .map(|(_, id)| *id)
                    .ok_or_else(|| Error::message("Missing replay document"))?;
                Ok(count_of(
                    storage
                        .document(id, DocumentPoint::Current, &BACKGROUND_CONTEXT)
                        .await?,
                ))
            },
            move |_| tail as i64,
        ));
    }
    benchmarks.extend([
        read_benchmark(
            "ancient historical read before newer base",
            |storage, dataset| async move {
                Ok(count_of(
                    storage
                        .document(
                            dataset.historical_document_id,
                            DocumentPoint::At(dataset.ancient_at),
                            &BACKGROUND_CONTEXT,
                        )
                        .await?,
                ))
            },
            |_| HISTORY_SEGMENT_LENGTH as i64,
        ),
        read_benchmark(
            "recent historical read after newer base",
            |storage, dataset| async move {
                Ok(count_of(
                    storage
                        .document(
                            dataset.historical_document_id,
                            DocumentPoint::At(dataset.recent_at),
                            &BACKGROUND_CONTEXT,
                        )
                        .await?,
                ))
            },
            |_| (HISTORY_SEGMENT_LENGTH * 2) as i64,
        ),
        read_benchmark(
            "fork-depth history scan (100)",
            |storage, dataset| async move {
                let query = EntryQuery {
                    conversation_id: dataset.deepest_conversation_id,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                };
                Ok(storage
                    .scan_entries(&query, 100, None, &BACKGROUND_CONTEXT)
                    .await?
                    .items
                    .len() as i64)
            },
            |_| 100,
        ),
        read_benchmark(
            "fork-depth head lookup",
            |storage, dataset| async move {
                Ok(storage
                    .find_latest_head_marker(
                        dataset.deepest_conversation_id,
                        None,
                        &BACKGROUND_CONTEXT,
                    )
                    .await?
                    .map_or(-1, |record| record.id.get()))
            },
            |dataset| dataset.ancestor_head_entry_id.get(),
        ),
    ]);
    benchmarks
}

pub struct StorageWriteBenchmark {
    pub name: &'static str,
    pub expected: i64,
    run: Box<dyn Fn(Arc<dyn Storage>) -> BenchmarkFuture + Send + Sync>,
}
impl StorageWriteBenchmark {
    pub fn run(&self, storage: Arc<dyn Storage>) -> BenchmarkFuture {
        (self.run)(storage)
    }
}

/// Seed the common state expected by every write benchmark sample.
pub async fn seed_storage_write_benchmark(storage: &dyn Storage) -> Result<()> {
    commit(storage, vec![root_conversation()]).await?;
    let mut writes = Vec::new();
    for index in 0..100 {
        writes.push(json!({
            "type": "entry",
            "value": {
                "id": mint(storage).await?,
                "conversationId": ROOT_CONVERSATION_ID.get(),
                "kind": "benchmark.baseline",
                "data": { "index": index },
            },
        }));
    }
    commit(storage, writes).await?;
    Ok(())
}

fn write_benchmark<Fut>(
    name: &'static str,
    expected: i64,
    run: impl Fn(Arc<dyn Storage>) -> Fut + Send + Sync + 'static,
) -> StorageWriteBenchmark
where
    Fut: Future<Output = Result<i64>> + Send + 'static,
{
    StorageWriteBenchmark {
        name,
        expected,
        run: Box::new(move |storage| Box::pin(run(storage))),
    }
}

pub fn storage_write_benchmarks() -> Vec<StorageWriteBenchmark> {
    vec![
        write_benchmark("commit one entry", 1, |storage| async move {
            let id = mint(&*storage).await?;
            commit(
                &*storage,
                vec![json!({
                    "type": "entry",
                    "value": {
                        "id": id,
                        "conversationId": ROOT_CONVERSATION_ID.get(),
                        "kind": "benchmark.write",
                        "data": { "text": "x".repeat(128) },
                    },
                })],
            )
            .await?;
            Ok(1)
        }),
        write_benchmark("commit 100 entries", 100, |storage| async move {
            let mut writes = Vec::new();
            for index in 0..100 {
                writes.push(json!({
                    "type": "entry",
                    "value": {
                        "id": mint(&*storage).await?,
                        "conversationId": ROOT_CONVERSATION_ID.get(),
                        "kind": "benchmark.write",
                        "data": { "index": index, "text": "x".repeat(128) },
                    },
                }));
            }
            let count = writes.len() as i64;
            commit(&*storage, writes).await?;
            Ok(count)
        }),
        write_benchmark(
            "commit mixed entry/task/submission/document",
            4,
            |storage| async move {
                let entry = mint(&*storage).await?;
                let task_id = mint(&*storage).await?;
                let submission = mint(&*storage).await?;
                let document = mint(&*storage).await?;
                let writes = vec![
                    json!({
                        "type": "entry",
                        "value": { "id": entry, "conversationId": ROOT_CONVERSATION_ID.get(), "kind": "benchmark.mixed" },
                    }),
                    json!({ "type": "task", "value": task(task_id, task_id as usize) }),
                    json!({
                        "type": "submission",
                        "value": {
                            "id": submission,
                            "conversationId": ROOT_CONVERSATION_ID.get(),
                            "requestId": format!("benchmark-{submission}"),
                            "type": "write",
                            "status": "done",
                            "entry": entry,
                        },
                    }),
                    json!({
                        "type": "document.create",
                        "record": { "id": document, "kind": "benchmark.mixed", "key": document.to_string(), "scope": { "kind": "session" } },
                        "content": { "kind": "base", "version": 1, "value": { "entryId": entry, "taskId": task_id } },
                    }),
                ];
                let count = writes.len() as i64;
                commit(&*storage, writes).await?;
                Ok(count)
            },
        ),
    ]
}

/// Seed `scale` into `read_storage` and the common write state into the empty
/// `write_storage`, then check every benchmark returns its expected value.
/// Returns the number of benchmarks executed.
pub async fn verify_storage_benchmarks(
    read_storage: Arc<dyn Storage>,
    write_storage: Arc<dyn Storage>,
    scale: &StorageBenchmarkScale,
) -> Result<usize> {
    let dataset = Arc::new(seed_storage_benchmark(&*read_storage, scale).await?);
    let mut executed = 0;
    for benchmark in storage_read_benchmarks() {
        let got = benchmark.run(read_storage.clone(), dataset.clone()).await?;
        let want = benchmark.expected(&dataset);
        if got != want {
            return Err(Error::message(format!(
                "{}: got {got}, expected {want}",
                benchmark.name
            )));
        }
        executed += 1;
    }
    seed_storage_write_benchmark(&*write_storage).await?;
    for benchmark in storage_write_benchmarks() {
        let got = benchmark.run(write_storage.clone()).await?;
        if got != benchmark.expected {
            return Err(Error::message(format!(
                "{}: got {got}, expected {}",
                benchmark.name, benchmark.expected
            )));
        }
        executed += 1;
    }
    Ok(executed)
}
