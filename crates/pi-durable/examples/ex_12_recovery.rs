//! Close, reopen, and continue where a task stopped, using a SQLite file for storage.
//! Port of upstream test/examples/12-recovery.ts.
#[path = "ex_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::storage::sqlite::open_native_sqlite_storage;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use support::*;
use tokio::sync::Notify;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TickInput {
    to: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum TickState {
    Tick { n: u64 },
}
type TickRuntime = TaskRuntime<TickInput, TickState, String, ()>;

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let directory = tempfile::tempdir().map_err(|error| Error::message(error.to_string()))?;
    let database = directory.path().join("session.sqlite");

    let printed: Arc<Mutex<Vec<u64>>> = Arc::default();
    let reached = Arc::new(Notify::new());
    let watch_for = Arc::new(Mutex::new(0u64));

    let (log, signal, target) = (printed.clone(), reached.clone(), watch_for.clone());
    let tick: PhaseHandler<TickInput, TickState, String, ()> =
        Arc::new(move |task, runtime: TickRuntime, task_context| {
            let (log, signal, target) = (log.clone(), signal.clone(), target.clone());
            Box::pin(async move {
                let TickState::Tick { n } = task.checkpoint().clone();
                // Save the intent before the effect. A memo keeps the first value written under
                // its name, so if the process dies after printing but before the next checkpoint
                // is saved, the rerun sees the memo and does not print the same tick twice.
                let name = format!("printed-{n}");
                if runtime
                    .memo::<bool>(name.clone(), &task_context)
                    .await?
                    .is_none()
                {
                    runtime.store_memo(name, true, &task_context).await?;
                    println!("tick {n}");
                    log.lock().push(n);
                }
                if *target.lock() == n {
                    signal.notify_one();
                }
                if n == task.record().info.input.to {
                    // Save the outcome: the final result...
                    return runtime
                        .commit(
                            move |_, _| async move {
                                Ok(Some(NextTaskState::Terminal {
                                    outcome: TaskOutcome::Completed {
                                        result: format!("counted to {n}"),
                                    },
                                }))
                            },
                            &task_context,
                        )
                        .await;
                }
                // ...or the next tick after a short sleep, stored with the checkpoint, so the task
                // holds no invocation meanwhile and a Harness opened later continues it.
                runtime
                    .sleep_until(
                        runtime.now()? + 50.0,
                        TickState::Tick { n: n + 1 },
                        &task_context,
                    )
                    .await
            })
        });
    let ticker = define_task(TaskDefinition {
        name: "example.ticker".into(),
        version: 1,
        initial: Arc::new(|_| Ok(TickState::Tick { n: 1 })),
        phases: [("tick".into(), tick)].into_iter().collect(),
        abort: Arc::new(|_, runtime: TickRuntime, task_context| {
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
                        &task_context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    });
    let registry = create_registry::<NativeTool, NativeSection>()?;
    let mut extension = define_extension(Ext::new("ticker"));
    extension.tasks.push(ticker.erased());
    registry.install(Arc::new(extension))?;
    let open = || {
        let (registry, database, context) = (registry.clone(), database.clone(), context.clone());
        async move {
            Harness::open(
                Arc::new(open_native_sqlite_storage(&database, Default::default()).await?),
                HarnessOptions::new(registry, pi_ai::models::Models::default()),
                &context,
            )
            .await
        }
    };

    // First run: start counting to 5, and close the Harness right after tick 2 is printed, before
    // its next checkpoint is saved. That is the same situation as a crash between the effect and
    // saving its outcome.
    *watch_for.lock() = 2;
    let first_run = open().await?;
    let task = ticker.clone();
    let root = first_run
        .root(ConversationOptions::default(), &context)
        .await?;
    let ticker_id = root
        .commit(
            move |tx| async move {
                tx.create_task(
                    &task,
                    TickInput { to: 5 },
                    TaskOptions::new(TaskOwnership::Conversation),
                )
                .await
            },
            &context,
        )
        .await?
        .untyped();
    first_run.resume()?;
    reached.notified().await;
    first_run.close(&context).await?;
    *watch_for.lock() = 0;

    // Read the saved record through a Harness that never resumes, so nothing runs.
    let reader = open().await?;
    let saved = reader
        .get_task(ticker_id, &context)
        .await?
        .expect("task is saved");
    reader.close(&context).await?;
    println!(
        "closed; saved checkpoint: {:?} memos: {:?}",
        saved.state,
        saved.state.memos()
    );
    assert!(
        !matches!(saved.state, TaskState::Terminal { .. }),
        "the task was unfinished"
    );
    let memos = saved.state.memos().expect("memos survive closing");
    assert_eq!(
        memos.get(&JsonString::from("printed-2")),
        Some(&JsonValue::Bool(true))
    );

    // Second run: nothing to restart by hand. Waiting for the unfinished task continues it. Tick 2
    // may run again, but its memo says it was already printed.
    let second_run = open().await?;
    let counted = second_run.wait_for_task(ticker_id, &context).await?;
    println!("after reopen: {:?}", counted.state);
    let TaskState::Terminal { outcome } = counted.into_record().state else {
        return Err(Error::message("ticker did not finish"));
    };
    assert_eq!(
        serde_json::to_value(&outcome).expect("outcome serializes"),
        serde_json::json!({"status": "completed", "result": "counted to 5"})
    );
    second_run.close(&context).await?;
    assert_eq!(
        *printed.lock(),
        [1, 2, 3, 4, 5],
        "every tick printed exactly once"
    );
    Ok(())
}
