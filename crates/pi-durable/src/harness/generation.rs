//! Durable preparation, model requests, polling, retries and tool-round handover.
mod prepare;
mod request;
mod settle;
mod tools;
mod types;
use super::live::{LiveRun, LiveState};
use crate::{wire, *};
use pi_chord::delta::Op;
pub use prepare::{Threshold, threshold_compaction};
pub use settle::convert_partial;
use std::sync::Arc;
use std::{any::TypeId, collections::HashMap, sync::LazyLock};
pub use types::*;

pub const GENERATION_KIND: &str = "pi.generation";
pub const GENERATION_VERSION: u64 = 1;
pub fn generation_task<T, S>() -> GenerationTask
where
    T: super::registry::RegisteredTool + super::declaration::DeclaredTool,
    S: super::prompt::PromptSection<GenerationPromptInput<T, S>>,
{
    static TASKS: LazyLock<parking_lot::Mutex<HashMap<TypeId, GenerationTask>>> =
        LazyLock::new(Default::default);
    TASKS
        .lock()
        .entry(TypeId::of::<(T, S)>())
        .or_insert_with(build_generation_task::<T, S>)
        .clone()
}
fn build_generation_task<T, S>() -> GenerationTask
where
    T: super::registry::RegisteredTool + super::declaration::DeclaredTool,
    S: super::prompt::PromptSection<GenerationPromptInput<T, S>>,
{
    define_task(TaskDefinition {
        name: GENERATION_KIND.into(),
        version: GENERATION_VERSION,
        initial: Arc::new(|_| Ok(GenerationCheckpoint::Prepare(Preparation::initial()))),
        phases: indexmap::IndexMap::from_iter([
            (
                "prepare".into(),
                Arc::new(|task, runtime, context| -> storage::StorageFuture<()> {
                    Box::pin(prepare::prepare::<T, S>(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
            (
                "request".into(),
                Arc::new(|task, runtime, context| -> storage::StorageFuture<()> {
                    Box::pin(request::request(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
            (
                "retry".into(),
                Arc::new(|task, runtime, context| -> storage::StorageFuture<()> {
                    Box::pin(request::retry(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
            (
                "poll".into(),
                Arc::new(|task, runtime, context| -> storage::StorageFuture<()> {
                    Box::pin(request::poll(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
            (
                "tools".into(),
                Arc::new(|task, runtime, context| -> storage::StorageFuture<()> {
                    Box::pin(tools::tools(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
        ]),
        abort: Arc::new(|task, runtime, context| Box::pin(settle::abort(task, runtime, context))),
        migrate: None,
        hooks: None,
    })
}
pub async fn create_generation(
    tx: &Transaction,
    conversation: ConversationId,
    submission_ids: Option<Vec<SubmissionId>>,
) -> Result<TaskId> {
    let mut options = TaskOptions::new(TaskOwnership::Conversation);
    options.conversation_id = Some(conversation);
    options.submission_ids = submission_ids;
    tx.create_task_record(
        GENERATION_KIND.into(),
        GENERATION_VERSION,
        JsonValue::Object(Default::default()),
        |_| {
            Ok(pi_chord::json::parse(r#"{"phase":"prepare","attempt":1}"#)
                .expect("valid built-in checkpoint"))
        },
        options,
    )
    .await
}
pub async fn start_run(
    tx: &Transaction,
    conversation: ConversationId,
    live: &DocDraft<LiveState>,
    inputs: Vec<SubmissionId>,
) -> Result<()> {
    let task_id = create_generation(tx, conversation, Some(inputs.clone())).await?;
    live.apply_ops(&[Op::Set(
        vec!["run".into()],
        wire::encode(&LiveRun { task_id, inputs }),
    )])
}
pub fn hand_over(live: &DocDraft<LiveState>, from: TaskId, to: TaskId) -> Result<()> {
    if live.read()?.run.is_some_and(|run| run.task_id == from) {
        live.apply_ops(&[Op::Set(
            vec!["run".into(), "taskId".into()],
            wire::encode(&to),
        )])?;
    }
    Ok(())
}
