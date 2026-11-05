//! Scheduler memory benchmark. Under one live parent task, N subagent cycles run and end: a tool task owned by the
//! parent, a conversation owned by the tool task, and a task in that conversation. Reports the live heap bytes (a
//! counting allocator, in place of upstream heap snapshots) once all cycles ended, then the storage reads a later
//! message into an old subagent conversation costs. On SQLite, so durable history stays out of the heap.
//!
//! The native scheduler does not expose its ownership index sizes, so the upstream assertion that those indexes do
//! not grow with ended history has no counterpart here; the heap growth per cycle is the observable.
//!
//!   cargo bench -p pi-durable --bench scheduler_memory          (PI_DURABLE_BENCH_QUICK=1 for few cycles)
mod support;
use parking_lot::Mutex;
use pi_ai::models::Models;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, atomic::Ordering},
    time::Instant,
};
use support::*;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static WAITING: LazyLock<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>> =
    LazyLock::new(Mutex::default);

#[derive(Clone, Serialize, Deserialize)]
struct HoldInput {
    name: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum HoldState {
    Hold,
}
type HoldTask = Task<HoldInput, HoldState, ()>;
/// Holds until released by name, then completes.
static HOLD: LazyLock<HoldTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "bench.hold".into(),
        version: 1,
        initial: Arc::new(|_| Ok(HoldState::Hold)),
        phases: indexmap::IndexMap::from_iter([(
            "hold".into(),
            Arc::new(
                |task: RunningTask<HoldInput, HoldState, ()>,
                 runtime: TaskRuntime<HoldInput, HoldState, (), ()>,
                 context| {
                    Box::pin(async move {
                        let (send, receive) = tokio::sync::oneshot::channel();
                        WAITING
                            .lock()
                            .insert(task.record().info.input.name.clone(), send);
                        let _ = receive.await;
                        runtime
                            .commit(
                                |_, _| async {
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

async fn release(name: &str) {
    loop {
        let sender = WAITING.lock().remove(name);
        if let Some(sender) = sender {
            let _ = sender.send(());
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(0)).await;
    }
}
fn hold(name: impl Into<String>) -> HoldInput {
    HoldInput { name: name.into() }
}

async fn run(cycles: usize) -> Result<(u128, isize, u64)> {
    let directory = tempfile::tempdir().map_err(bench_err)?;
    let storage = open_native_sqlite_storage(
        directory.path().join("session.sqlite"),
        NativeSqliteStorageOptions::default(),
    )
    .await?;
    let (storage, tally) = Counted::wrap(Arc::new(storage));
    let registry = registry();
    let mut extension = Ext::new("bench");
    extension.tasks.push(HOLD.erased());
    install(&registry, extension);
    let context = ctx();
    let harness = Harness::open(
        storage,
        HarnessOptions::new(registry, Models::default()),
        &context,
    )
    .await?;
    let root = harness.root(Default::default(), &context).await?;
    harness.resume()?;
    let parent = root
        .commit(
            |tx| async move {
                tx.create_task(
                    &HOLD,
                    hold("parent"),
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &context,
        )
        .await?;
    let before = live_bytes() as isize;
    let mut first = None;
    let started = Instant::now();
    for n in 0..cycles {
        let parent_id = parent.untyped();
        let (tool, inner, child) = root
            .commit(
                move |tx| async move {
                    let tool = tx
                        .create_task(
                            &HOLD,
                            hold(format!("tool{n}")),
                            TaskOptions::new(TaskOwnership::Task(parent_id)),
                        )
                        .await?;
                    let child = tx
                        .create_conversation(ConversationOwnership::Task(tool.untyped()))
                        .await?;
                    let mut options = TaskOptions::new(TaskOwnership::Conversation);
                    options.conversation_id = Some(child.id);
                    let inner = tx
                        .create_task(&HOLD, hold(format!("inner{n}")), options)
                        .await?;
                    Ok((tool, inner, child.id))
                },
                &context,
            )
            .await?;
        release(&format!("inner{n}")).await;
        release(&format!("tool{n}")).await;
        harness.wait_for_task(inner.untyped(), &context).await?;
        harness.wait_for_task(tool.untyped(), &context).await?;
        first.get_or_insert(child);
    }
    let ms = started.elapsed().as_millis();
    let heap = live_bytes() as isize - before;
    // A later message into an old subagent conversation: a task created there, run, and ended.
    let reads_before = tally.reads.load(Ordering::SeqCst);
    let old = harness
        .conversation(first.expect("at least one cycle"), &context)
        .await?
        .ok_or_else(|| bench_err("the old conversation is gone"))?;
    let late = old
        .commit(
            |tx| async move {
                tx.create_task(
                    &HOLD,
                    hold("late"),
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &context,
        )
        .await?;
    release("late").await;
    harness.wait_for_task(late.untyped(), &context).await?;
    let late_reads = tally.reads.load(Ordering::SeqCst) - reads_before;
    release("parent").await;
    harness.wait_for_task(parent.untyped(), &context).await?;
    harness.close(&context).await?;
    Ok((ms, heap, late_reads))
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("cycles | ms     | heap growth             | reads for a later message");
    for n in sizes(&[1000, 10000], &[100]) {
        let (ms, heap, reads) = run(n).await?;
        println!(
            "{:<6} | {:<6} | {:<23} | {reads}",
            n,
            ms,
            format!(
                "{:.1} MiB, {} B/cycle",
                heap as f64 / 1024.0 / 1024.0,
                heap / n as isize
            )
        );
    }
    Ok(())
}
