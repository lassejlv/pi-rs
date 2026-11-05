//! `HookApi` / `DocumentReader`: one checked surface over the runtime objects of every task kind.
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_durable::{
    harness::{
        catalog::*, compaction::CompactionRuntime, generation::GenerationRuntime, scheduler::*,
        state::NextTaskState, tool::ToolRuntime,
    },
    *,
};
use std::{sync::Arc, time::Duration};

fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
// Compile-time: the runtime of every built-in task kind, and the erased one, is a HookApi.
fn is_hook_api<A: HookApi>() {}
#[test]
fn every_task_runtime_implements_the_hook_api() {
    is_hook_api::<GenerationRuntime>();
    is_hook_api::<ToolRuntime>();
    is_hook_api::<CompactionRuntime>();
    is_hook_api::<harness::runtime::TaskRuntime>();
}

/// Hook code written once over the trait.
async fn exercise<A: HookApi>(
    api: &A,
    seen: Arc<Mutex<Vec<String>>>,
    context: &Context,
) -> Result<()> {
    seen.lock().push(format!(
        "task {} conversation {}",
        api.task_id(),
        api.conversation_id()
    ));
    assert!(api.memo::<String>("greeting", context).await?.is_none());
    assert_eq!(
        api.store_memo("greeting", "first".to_owned(), context)
            .await?,
        "first"
    );
    // The memo that stands wins over a later candidate.
    assert_eq!(
        api.store_memo("greeting", "second".to_owned(), context)
            .await?,
        "first"
    );
    assert_eq!(
        api.memo::<String>("greeting", context).await?.as_deref(),
        Some("first")
    );
    // The built-in agent document is readable through the reader surface.
    let agent = api
        .snapshot(&AGENT_DOC, api.conversation_id(), context)
        .await?;
    seen.lock().push(format!("agent {}", agent.is_some()));
    Ok(())
}

#[tokio::test]
async fn hook_code_over_the_trait_reads_ids_memos_and_documents() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let task: Task<JsonValue, JsonValue, JsonValue> = define_task(TaskDefinition {
        name: "hook-api".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([(
            "run".into(),
            Arc::new(
                move |_, runtime: harness::runtime::TaskRuntime, context: Context| {
                    let log = log.clone();
                    Box::pin(async move {
                        exercise(&runtime, log, &context).await?;
                        runtime
                            .commit(
                                |_, _| async {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: JsonValue::Null,
                                        },
                                    }))
                                },
                                &context,
                            )
                            .await
                    }) as storage::StorageFuture<()>
                },
            ) as PhaseHandler<JsonValue, JsonValue, JsonValue>,
        )]),
        abort: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        migrate: None,
        hooks: None,
    });
    let catalog = TaskCatalog::default();
    catalog
        .replace([InstalledTask {
            task: task.erased(),
            extension: Some("tests".into()),
        }])
        .unwrap();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog,
        SchedulerOptions::default(),
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &Context::default(),
        )
        .await
        .unwrap();
    let started = task.clone();
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
                Ok(tx
                    .create_task(&started, JsonValue::Null, options)
                    .await?
                    .untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    let settled = tokio::time::timeout(
        Duration::from_secs(5),
        scheduler.wait_for_task(id, &Context::default()),
    )
    .await
    .expect("task did not settle")
    .unwrap();
    assert!(
        matches!(
            settled.state,
            TaskState::Terminal {
                outcome: TaskOutcome::Completed { .. }
            }
        ),
        "{:?}",
        settled.state
    );
    assert_eq!(
        *seen.lock(),
        [
            format!("task {id} conversation {root}"),
            "agent false".to_owned()
        ]
    );
}
