use indexmap::IndexMap;
use pi_durable::{
    harness::{catalog::*, scheduler::*, state::*},
    *,
};
use pi_telemetry::{
    InMemoryTelemetryContext, SpanOptions, SpanOutcome, SpanStatus, TelemetryContext,
};
use std::sync::Arc;

fn json(value: &str) -> JsonValue {
    pi_chord::json::parse(value).unwrap()
}
type JsonTask = Task<JsonValue, JsonValue, JsonValue>;
fn task(phases: IndexMap<JsonString, PhaseHandler<JsonValue, JsonValue, JsonValue>>) -> JsonTask {
    define_task(TaskDefinition {
        name: "test.telemetry".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases,
        abort: Arc::new(
            |_: RunningTask,
             runtime: pi_durable::harness::runtime::TaskRuntime,
             context: Context|
             -> pi_durable::storage::StorageFuture<()> {
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
            },
        ),
        migrate: None,
        hooks: None,
    })
}
async fn open(task: &JsonTask, telemetry: Arc<InMemoryTelemetryContext>) -> TaskScheduler {
    let catalog = TaskCatalog::default();
    catalog
        .replace([InstalledTask {
            task: task.erased(),
            extension: Some("tests".into()),
        }])
        .unwrap();
    TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog,
        SchedulerOptions {
            telemetry,
            ..Default::default()
        },
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap()
}
async fn start(scheduler: &TaskScheduler, task: JsonTask, context: &Context) -> TaskId {
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let root = tx.create_root_conversation().await?;
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root.id);
                Ok(tx
                    .create_task(&task, JsonValue::Null, options)
                    .await?
                    .untyped())
            },
            context,
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    id
}
async fn settled(scheduler: &TaskScheduler, telemetry: &InMemoryTelemetryContext, id: TaskId) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        scheduler
            .wait_for_task(id, &Context::default())
            .await
            .unwrap();
        while telemetry.get_spans().iter().any(|span| !span.settled) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn phase_parent_progress_checkpoint_and_metrics_match_source() {
    let telemetry = InMemoryTelemetryContext::new();
    let creator = telemetry.begin_span(SpanOptions::new("creator"));
    let context = pi_chord::context::with_telemetry(creator.clone(), &Context::default());
    let task = task(IndexMap::from([
        (
            "run".into(),
            Arc::new(
                |_: RunningTask,
                 runtime: pi_durable::harness::runtime::TaskRuntime,
                 context: Context|
                 -> pi_durable::storage::StorageFuture<()> {
                    Box::pin(async move {
                        for checkpoint in [
                            r#"{"phase":"run","count":1}"#,
                            r#"{"phase":"finish","attempt":2}"#,
                        ] {
                            runtime
                                .commit(
                                    move |_, _| async move {
                                        Ok(Some(NextTaskState::Running {
                                            checkpoint: json(checkpoint),
                                        }))
                                    },
                                    &context,
                                )
                                .await?;
                        }
                        Ok(())
                    })
                },
            ) as PhaseHandler<_, _, _>,
        ),
        (
            "finish".into(),
            Arc::new(
                |_: RunningTask,
                 runtime: pi_durable::harness::runtime::TaskRuntime,
                 context: Context|
                 -> pi_durable::storage::StorageFuture<()> {
                    Box::pin(async move {
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
                    })
                },
            ) as PhaseHandler<_, _, _>,
        ),
    ]));
    let scheduler = open(&task, telemetry.clone()).await;
    let id = start(&scheduler, task, &context).await;
    creator.end(SpanOutcome::Ok);
    settled(&scheduler, &telemetry, id).await;
    let spans = telemetry.get_spans();
    let phases = spans
        .iter()
        .filter(|span| span.name == "pi.durable.phase")
        .collect::<Vec<_>>();
    assert_eq!(phases.len(), 2);
    assert_eq!(phases[0].parent_context, Some(spans[0].context.clone()));
    assert_eq!(phases[1].parent_context, phases[0].parent_context);
    assert_eq!(
        phases[0].attributes["outcome"].as_str(),
        Some("next=finish")
    );
    assert_eq!(phases[1].attributes["outcome"].as_str(), Some("done"));
    assert_eq!(phases[1].attributes["attempt"], 2f64.into());
    assert_eq!(phases[0].events.len(), 1);
    assert_eq!(phases[0].events[0].name, "pi.durable.checkpoint");
    assert_eq!(
        phases[0].events[0].attributes,
        pi_telemetry::attributes! {"phase"=>"finish"}
    );
    assert!(
        phases
            .iter()
            .all(|span| span.attributes["resumed"] == false.into())
    );
    let metrics = telemetry.get_metrics();
    let active = metrics
        .iter()
        .filter(|metric| metric.name == "pi.durable.tasks.active")
        .collect::<Vec<_>>();
    assert_eq!(
        active.iter().map(|metric| metric.value).collect::<Vec<_>>(),
        [1.0, -1.0]
    );
    let durations = metrics
        .iter()
        .filter(|metric| metric.name == "pi.durable.phase.duration")
        .collect::<Vec<_>>();
    assert_eq!(durations.len(), 2);
    assert!(durations.iter().all(|metric| metric.value >= 0.0
        && metric.unit.as_deref() == Some("ms")
        && metric.attributes.len() == 3));
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn thrown_phase_records_bounded_utf16_diagnostic_without_error_detail() {
    let telemetry = InMemoryTelemetryContext::new();
    let mut units = vec![b'a' as u16; 499];
    units.extend([0xd83d, 0xde00]);
    let message = JsonString::from_utf16(units);
    let task = task(IndexMap::from([(
        "run".into(),
        Arc::new(
            move |_: RunningTask,
                  _: pi_durable::harness::runtime::TaskRuntime,
                  _: Context|
                  -> pi_durable::storage::StorageFuture<()> {
                let message = message.clone();
                Box::pin(async move { Err(Error::host("PrivateErrorName", message)) })
            },
        ) as PhaseHandler<_, _, _>,
    )]));
    let scheduler = open(&task, telemetry.clone()).await;
    let id = start(&scheduler, task, &Context::default()).await;
    settled(&scheduler, &telemetry, id).await;
    let spans = telemetry.get_spans();
    assert_eq!(spans.len(), 1);
    let SpanStatus::Error(Some(error)) = &spans[0].status else {
        panic!("missing fault diagnostic")
    };
    assert_eq!(error.name, "task_faulted");
    assert_eq!(error.message.units().len(), 500);
    assert_eq!(error.message.units()[499], 0xd83d);
    assert_eq!(spans[0].attributes["outcome"].as_str(), Some("faulted"));
    assert!(
        !serde_json::to_string(&spans)
            .unwrap()
            .contains("PrivateErrorName")
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
