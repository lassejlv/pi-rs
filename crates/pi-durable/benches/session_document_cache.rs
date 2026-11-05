//! Session document cache benchmark. A Harness on SQLite creates N conversations and runs one short task in each, as
//! a long-running agent with subagents does; then, at a few retention settings, reports the documents left in the
//! cache, the bytes live objects retain (counting allocator, in place of upstream heap snapshots) per conversation,
//! and the cost of reading a document of an old conversation again.
//!
//!   cargo bench -p pi-durable --bench session_document_cache          (PI_DURABLE_BENCH_QUICK=1 for small N)
mod support;
use pi_ai::models::Models;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, LazyLock},
    time::Instant,
};
use support::*;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum WorkState {
    Work,
}
type WorkTask = Task<(), WorkState, ()>;
/// Writes `pi.usage` of its conversation once, then completes.
static WORK: LazyLock<WorkTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "bench.work".into(),
        version: 1,
        initial: Arc::new(|_| Ok(WorkState::Work)),
        phases: indexmap::IndexMap::from_iter([(
            "work".into(),
            Arc::new(
                |_: RunningTask<(), WorkState, ()>,
                 runtime: TaskRuntime<(), WorkState, (), ()>,
                 context| {
                    Box::pin(async move {
                        let conversation = runtime.conversation_id();
                        runtime
                            .commit(
                                move |tx, _| async move {
                                    tx.doc(&USAGE_DOC, conversation).await?.update(|usage| {
                                        usage
                                            .tools
                                            .insert("bench".into(), object(r#"{"input":1}"#));
                                        Ok(())
                                    })?;
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed { result: () },
                                    }))
                                },
                                &context,
                            )
                            .await
                    }) as pi_durable::storage::StorageFuture<()>
                },
            ) as PhaseHandler<_, _, _>,
        )]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Aborted {
                                    reason: None,
                                    result: None,
                                },
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
});

struct Outcome {
    ms: u128,
    heap: isize,
    cached: usize,
    read_us: u128,
}

async fn cycle(harness: &Harness) -> Result<ConversationId> {
    let context = ctx();
    let conversation = harness
        .create_conversation(
            ConversationCreateOptions::new(ConversationOwnership::Ownerless),
            &context,
        )
        .await?;
    let task = conversation
        .commit(
            |tx| async move {
                tx.create_task(&WORK, (), TaskOptions::new(TaskOwnership::Conversation))
                    .await
            },
            &context,
        )
        .await?;
    let settled = harness.wait_for_task(task.untyped(), &context).await?;
    if !matches!(
        settled.state,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { .. }
        }
    ) {
        return Err(bench_err("the work task did not complete"));
    }
    Ok(conversation.id)
}

async fn run(conversations: usize, retention_ms: f64) -> Result<Outcome> {
    let directory = tempfile::tempdir().map_err(bench_err)?;
    let storage = open_native_sqlite_storage(
        directory.path().join("session.sqlite"),
        NativeSqliteStorageOptions::default(),
    )
    .await?;
    let registry = registry();
    let mut extension = Ext::new("bench");
    extension.tasks.push(WORK.erased());
    install(&registry, extension);
    let context = ctx();
    let options = HarnessOptions::new(registry, Models::default()).with_settings(HarnessSettings {
        document_retention_ms: Some(retention_ms),
        ..Default::default()
    });
    let harness = Harness::open(Arc::new(storage), options, &context).await?;
    harness.resume()?;
    // Warm up first: compiled code and storage caches would otherwise count as growth.
    let mut first = None;
    for _ in 0..if quick() { 20 } else { 200 } {
        first.get_or_insert(cycle(&harness).await?);
    }
    harness.read_on_line(|| async { Ok(()) }).await?;
    let before = live_bytes() as isize;
    let started = Instant::now();
    for _ in 0..conversations {
        cycle(&harness).await?;
    }
    let ms = started.elapsed().as_millis();
    // Wait past a short retention without committing, so only the timer can evict; then let its job run.
    if retention_ms > 0.0 && retention_ms < 1000.0 {
        pause((retention_ms * 4.0) as u64).await;
    }
    harness.read_on_line(|| async { Ok(()) }).await?;
    let heap = live_bytes() as isize - before;
    let cached = harness.cache_sizes().cached;
    // One read of an old conversation's document: from the cache, or from storage after eviction.
    let first = first.expect("warmup ran");
    let read_started = Instant::now();
    for _ in 0..100 {
        harness.snapshot(&LIVE_DOC, first, &context).await?;
    }
    let read_us = read_started.elapsed().as_micros() / 100;
    harness.close(&context).await?;
    Ok(Outcome {
        ms,
        heap,
        cached,
        read_us,
    })
}
async fn pause(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[tokio::main]
async fn main() -> Result<()> {
    println!(
        "conversations | retention   | ms     | cached docs | retained bytes        | old-doc read"
    );
    for n in sizes(&[1000, 5000], &[100]) {
        for (label, retention) in [
            ("infinite", f64::INFINITY),
            ("10 min", 600_000.0),
            ("50 ms", 50.0),
            ("0", 0.0),
        ] {
            let Outcome {
                ms,
                heap,
                cached,
                read_us,
            } = run(n, retention).await?;
            println!(
                "{:<13} | {:<11} | {:<6} | {:<11} | {:<21} | {read_us} us",
                n,
                label,
                ms,
                cached,
                format!(
                    "{:.1} MiB, {} B/conv",
                    heap as f64 / 1024.0 / 1024.0,
                    heap / n as isize
                )
            );
        }
    }
    Ok(())
}
