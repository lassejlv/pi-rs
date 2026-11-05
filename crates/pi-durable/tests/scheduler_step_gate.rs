//! The scheduler's step gate fixes upstream's order around a caller's commit: the commit that creates
//! a task publishes, reservation runs at once, the caller's next commit lands, and only then does the
//! task's first step start. A host across a process boundary holds the gate until it acknowledges the
//! reply, so a step can never overtake the continuation it raced in the stock retention case.
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_durable::{
    harness::{catalog::*, scheduler::*},
    *,
};
use std::{sync::Arc, time::Duration};

type JsonTask = Task<JsonValue, JsonValue, JsonValue>;

fn json(value: &str) -> JsonValue {
    pi_chord::json::parse(value).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_step_waits_for_the_gate_after_reservation_and_sees_the_callers_next_commit() {
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = events.clone();
    let phase: PhaseHandler<JsonValue, JsonValue, JsonValue> =
        Arc::new(move |task, runtime, context| {
            log.lock()
                .push(format!("step input={}", task.record().info.input));
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
        });
    let task: JsonTask = define_task(TaskDefinition {
        name: "test.gate.spawner".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([("run".into(), phase)]),
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
    // The host's acknowledgement: closed until the caller's continuation has committed.
    let (acknowledge, acknowledged) = tokio::sync::watch::channel(false);
    let gated = events.clone();
    let gate: StepGate = Arc::new(move |_| {
        let mut acknowledged = acknowledged.clone();
        let gated = gated.clone();
        Box::pin(async move {
            gated.lock().push("gate".into());
            let _ = acknowledged.wait_for(|open| *open).await;
        })
    });
    let context = Context::default();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog,
        SchedulerOptions {
            step_gate: Some(gate),
            ..Default::default()
        },
        SessionOptions::default(),
        &context,
    )
    .await
    .unwrap();
    let root = scheduler
        .session()
        .commit(
            |tx| async move { Ok(tx.create_root_conversation().await?.id) },
            &context,
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    let created = task.clone();
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root);
                Ok(tx
                    .create_task(&created, json(r#"{"into":0}"#), options)
                    .await?
                    .untyped())
            },
            &context,
        )
        .await
        .unwrap();
    // Reservation is not gated: the invocation reaches the gate on its own.
    tokio::time::timeout(Duration::from_secs(3), async {
        while !events.lock().iter().any(|event| event == "gate") {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the first step never reached the gate");
    let log = events.clone();
    scheduler
        .session()
        .commit(
            move |tx| async move {
                let mut record = tx.task(id).await?.unwrap();
                log.lock()
                    .push(format!("continuation sees {:?}", record.state.status()));
                record.info.input = json(r#"{"into":7}"#);
                tx.set_task(record)?;
                Ok(())
            },
            &context,
        )
        .await
        .unwrap();
    acknowledge.send(true).unwrap();
    let ended = tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.wait_for_task(id, &context),
    )
    .await
    .expect("task did not settle")
    .unwrap();
    assert_eq!(ended.state.status(), TaskStatus::Terminal);
    assert_eq!(
        *events.lock(),
        [
            "gate",
            "continuation sees Running",
            "step input={\"into\":7}",
            "gate",
        ]
    );
    scheduler.session().close(&context).await.unwrap();
}
