//! `SettledTask`: a task record known to be terminal, the receipt `Harness::wait_for_task` returns.
use pi_durable::{harness::settled::SettledTask, *};

fn record(state: &str) -> TaskRecord {
    let text = format!(
        r#"{{"id":7,"conversationId":1,"kind":"k","version":1,"input":null,"background":false,"abortRequested":false,"state":{state}}}"#
    );
    serde_json::from_str(&text).unwrap()
}

// Compile-time: the Harness wait API narrows to the receipt.
#[allow(dead_code)]
fn wait_narrows(
    harness: &Harness,
    id: TaskId,
    context: &Context,
) -> storage::StorageFuture<SettledTask> {
    harness.wait_for_task(id, context)
}

#[test]
fn only_terminal_records_become_receipts() {
    for state in [
        r#"{"status":"pending","checkpoint":{}}"#,
        r#"{"status":"running","checkpoint":{}}"#,
        r#"{"status":"waiting","checkpoint":{},"on":[],"policy":"allSettled"}"#,
        r#"{"status":"sleeping","checkpoint":{},"until":5}"#,
    ] {
        let error = SettledTask::try_from(record(state)).unwrap_err();
        assert_eq!(error.to_string(), "Task 7 has not settled", "{state}");
    }
    let completed = SettledTask::try_from(record(
        r#"{"status":"terminal","outcome":{"status":"completed","result":41}}"#,
    ))
    .unwrap();
    // Record fields read through the receipt; the outcome is always present.
    assert_eq!(
        (completed.id().get(), completed.info.kind.to_string_lossy()),
        (7, "k".to_owned())
    );
    assert!(
        matches!(completed.outcome(), TaskOutcome::Completed { result } if result.as_f64() == Some(41.0))
    );
    assert_eq!(completed.as_record().state.status(), TaskStatus::Terminal);
    let back: TaskRecord = completed.into();
    assert!(matches!(back.state, TaskState::Terminal { .. }));
    let aborted = SettledTask::try_from(record(
        r#"{"status":"terminal","outcome":{"status":"aborted"}}"#,
    ))
    .unwrap();
    assert!(matches!(aborted.outcome(), TaskOutcome::Aborted { .. }));
}
