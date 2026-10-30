use super::{settle::append_assistant, types::*, *};
use crate::harness::{
    declaration::current_tool_declarations,
    inbox::*,
    live::{self, LIVE_DOC, SlotProgress, SlotStatus, ToolSlot},
    tool::*,
    types::QueueModes,
};
use indexmap::{IndexMap, IndexSet};

pub(super) async fn read_calls(
    runtime: &GenerationRuntime,
    assistant: EntryId,
    ids: &[JsonString],
    context: &Context,
) -> Result<Vec<ToolCall>> {
    let entry = runtime.entry(assistant, context).await?;
    let message = entry
        .as_ref()
        .filter(|entry| entry.kind == *ASSISTANT_ENTRY.kind())
        .and_then(|entry| entry.model.as_ref())
        .and_then(|model| model.first());
    let calls = message
        .filter(|message| message["role"].string_equals("assistant"))
        .and_then(|message| message["content"].as_array())
        .into_iter()
        .flatten()
        .filter(|content| content["type"].string_equals("toolCall"))
        .map(|content| wire::decode(content).map_err(Error::message))
        .collect::<Result<Vec<ToolCall>>>()?;
    Ok(ids
        .iter()
        .filter_map(|id| calls.iter().find(|call| call.id == *id).cloned())
        .collect())
}
async fn create_tool_task(
    tx: &Transaction,
    runtime: &GenerationRuntime,
    assistant: EntryId,
    id: JsonString,
) -> Result<TypedTaskId<ToolTaskResult>> {
    tx.create_task(
        &tool_task(),
        ToolTaskInput::model(assistant, id),
        TaskOptions::new(TaskOwnership::Task(runtime.task_id().untyped())),
    )
    .await
}
pub(super) async fn tools(
    task: Running,
    runtime: GenerationRuntime,
    context: Context,
) -> Result<()> {
    let GenerationCheckpoint::Tools(mut round) = task.checkpoint().clone() else {
        return Err(Error::message("Expected tools checkpoint"));
    };
    if round.pending.is_empty() {
        return finish_tool_round(runtime, round, context).await;
    }
    let next = round.pending.remove(0);
    let conversation = runtime.conversation_id();
    let current = runtime.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                let task = create_tool_task(&tx, &current, round.assistant, next.clone()).await?;
                live::update_fields(&live, &["tools"], |live| {
                    if let Some(slot) = live.tools.as_mut().and_then(|slots| {
                        slots
                            .iter_mut()
                            .find(|slot| slot.progress.call_id == next && slot.task_id.is_none())
                    }) {
                        slot.task_id = Some(task.untyped());
                    }
                })?;
                round.tools.push(task);
                Ok(Some(Next::Waiting {
                    checkpoint: GenerationCheckpoint::Tools(round),
                    on: vec![task.untyped()],
                    policy: JoinPolicy::AllSettled,
                }))
            },
            &context,
        )
        .await
}
pub(super) async fn start_tool_round(
    runtime: GenerationRuntime,
    request: ResponseRequest,
    message: GenerationMessage,
    calls: Vec<ToolCall>,
    context: Context,
) -> Result<()> {
    let conversation = runtime.conversation_id();
    let messages = match request.messages {
        Some(messages) => messages,
        None => {
            runtime
                .context(conversation, Some(request.cutoff), &context)
                .await?
                .messages
        }
    };
    let offered = current_tool_declarations(&messages)?
        .into_iter()
        .map(|tool| tool.name)
        .collect::<IndexSet<_>>();
    let agent = runtime.agent(&context).await?;
    let mut sequential = runtime.settings()?.tool_execution.equals_str("sequential");
    for call in &calls {
        if offered.contains(&call.name)
            && agent
                .tool(&call.name, false)?
                .is_some_and(|tool| tool.execution_mode() == Some(ToolExecutionMode::Sequential))
        {
            sequential = true;
        }
    }
    let current = runtime.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                let entry = append_assistant(&tx, conversation, message).await?;
                let mut slots = Vec::new();
                let mut tools = Vec::new();
                let mut pending = Vec::new();
                for call in calls {
                    let mut progress = SlotProgress::new(call.id.clone(), call.name.clone());
                    if !offered.contains(&call.name) {
                        let unavailable = harness_error(
                            "tool_unavailable",
                            joined([
                                "Tool ".into(),
                                call.name.clone(),
                                " is not available".into(),
                            ]),
                        );
                        let result = append_tool_result(
                            &tx,
                            conversation,
                            &call,
                            &unavailable,
                            current.now()?,
                            None,
                        )
                        .await?;
                        progress.status = SlotStatus::Done;
                        slots.push(ToolSlot {
                            progress,
                            task_id: None,
                            entry: Some(result.id),
                        });
                        continue;
                    }
                    if sequential && !tools.is_empty() {
                        pending.push(call.id);
                        slots.push(ToolSlot {
                            progress,
                            task_id: None,
                            entry: None,
                        });
                        continue;
                    }
                    let task = create_tool_task(&tx, &current, entry.id, call.id).await?;
                    tools.push(task);
                    slots.push(ToolSlot {
                        progress,
                        task_id: Some(task.untyped()),
                        entry: None,
                    });
                }
                live.apply_ops(&[
                    Op::Delete(vec!["generation".into()]),
                    Op::Set(vec!["tools".into()], wire::encode(&slots)),
                ])?;
                let on = tools.iter().map(|id| id.untyped()).collect();
                Ok(Some(Next::Waiting {
                    checkpoint: GenerationCheckpoint::Tools(ToolRound {
                        assistant: entry.id,
                        tools,
                        pending,
                    }),
                    on,
                    policy: JoinPolicy::AllSettled,
                }))
            },
            &context,
        )
        .await
}
async fn finish_tool_round(
    runtime: GenerationRuntime,
    round: ToolRound,
    context: Context,
) -> Result<()> {
    let conversation = runtime.conversation_id();
    let mut controls = IndexMap::new();
    for (id, outcome) in round
        .tools
        .iter()
        .zip(runtime.outcomes(&round.tools, &context).await?)
    {
        let control = match outcome {
            TaskOutcome::Completed {
                result:
                    ToolTaskResult::Model { control, .. } | ToolTaskResult::LegacyModel { control, .. },
            } => control,
            _ => None,
        };
        controls.insert(id.untyped(), control);
    }
    let slots = runtime
        .snapshot(&LIVE_DOC, conversation, &context)
        .await?
        .and_then(|live| live.tools.clone())
        .unwrap_or_default();
    let results = slots
        .iter()
        .filter_map(|slot| slot.entry)
        .collect::<Vec<_>>();
    let current = runtime.clone();
    let callback_context = context.clone();
    let assistant = round.assistant;
    runtime
        .hooks()
        .each(&AFTER_TOOLS, move |hook| {
            hook(
                assistant,
                results.clone(),
                current.clone(),
                callback_context.clone(),
            )
        })
        .await?;
    let terminate = !slots.is_empty()
        && slots.iter().all(|slot| {
            slot.task_id
                .and_then(|id| controls.get(&id))
                .and_then(Option::as_ref)
                .is_some_and(|control| control.terminate == Some(true))
        });
    let added = controls
        .values()
        .filter_map(Option::as_ref)
        .flat_map(|control| control.add_tools.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    let handoff = controls
        .values()
        .rev()
        .filter_map(Option::as_ref)
        .find_map(|control| control.handoff.clone());
    let owner = runtime.task_id().untyped();
    let current = runtime.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let mut boundary =
                    prepare_boundary(&tx, conversation, QueueModes::from(&current.settings()?))
                        .await?;
                if !added.is_empty() {
                    crate::harness::agent::add_tools(&tx, conversation, added).await?;
                }
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                let now = current.now()?;
                if terminate || handoff.is_some() {
                    if let Some(handoff) = handoff {
                        let mut entry = EntryDraft::new(RESET_ENTRY.kind().clone());
                        entry.head = Some(EntryHead::ThisEntry);
                        let mut message = wire::object();
                        wire::put(&mut message, "role", &"user");
                        wire::put(&mut message, "content", &handoff);
                        wire::put(&mut message, "timestamp", &now);
                        entry.model = Some(vec![message]);
                        boundary.head = Some(tx.append_entry(conversation, entry).await?.id);
                    }
                    let result = apply_boundary(&tx, &mut boundary, BoundaryAt::Final, now).await?;
                    live::end_run(
                        &tx,
                        &live,
                        owner,
                        SubmissionSettlement::Done { answer: assistant },
                    )?;
                    if !result.users.is_empty() {
                        start_run(&tx, conversation, &live, result.users).await?;
                    }
                } else {
                    let result =
                        apply_boundary(&tx, &mut boundary, BoundaryAt::PostTools, now).await?;
                    if result.reset {
                        live::end_run(
                            &tx,
                            &live,
                            owner,
                            SubmissionSettlement::Unanswered {
                                reason: "reset".into(),
                                detail: None,
                            },
                        )?;
                        if !result.users.is_empty() {
                            start_run(&tx, conversation, &live, result.users).await?;
                        }
                    } else {
                        live::update_fields(&live, &["tools", "nestedTools", "run"], |live| {
                            live.tools = None;
                            live.nested_tools = None;
                            if let Some(run) = live.run.as_mut().filter(|run| run.task_id == owner)
                            {
                                run.inputs.extend(result.users);
                            }
                        })?;
                        hand_over(
                            &live,
                            owner,
                            create_generation(&tx, conversation, None).await?,
                        )?;
                    }
                }
                Ok(Some(Next::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: GenerationResult {
                            entry_id: assistant,
                        },
                    },
                }))
            },
            &context,
        )
        .await
}
