use super::*;
use crate::{
    harness::{
        hooks::HookKey,
        live::{self, LIVE_DOC, NestedToolSlot, SlotProgress, SlotStatus},
        usage::{UsageBucket, record_usage},
    },
    storage::StorageFuture,
    wire, *,
};
use futures::FutureExt;
use parking_lot::Mutex;
use std::sync::{Arc, LazyLock};

pub const MADE_NESTED_CALLS: &str = "pi.tool.madeNestedCalls";
pub type ToolRuntime = TaskRuntime<ToolTaskInput, ToolTaskCheckpoint, ToolTaskResult, ToolHooks>;
pub type ToolTask = Task<ToolTaskInput, ToolTaskCheckpoint, ToolTaskResult, ToolHooks>;
type Running = RunningTask<ToolTaskInput, ToolTaskCheckpoint, ToolTaskResult>;
type Next = NextTaskState<ToolTaskCheckpoint, ToolTaskResult>;
#[derive(Clone)]
pub struct ToolHookCall {
    pub call: ToolCall,
    pub parent: Option<ToolParent>,
}
#[derive(Clone)]
pub struct ToolParent {
    pub task_id: TaskId,
    pub call_id: JsonString,
}
#[derive(Default)]
pub struct ToolDecision {
    pub arguments: Option<JsonValue>,
    pub block: Option<JsonString>,
}
pub type BeforeTool = Arc<
    dyn Fn(ToolHookCall, ToolRuntime, Context) -> StorageFuture<Option<ToolDecision>> + Send + Sync,
>;
pub type AfterTool = Arc<
    dyn Fn(
            ToolHookCall,
            ToolExecutionResult,
            ToolRuntime,
            Context,
        ) -> StorageFuture<Option<ToolExecutionResult>>
        + Send
        + Sync,
>;
#[derive(Default)]
pub struct ToolHooks {
    pub before_tool: Option<BeforeTool>,
    pub after_tool: Option<AfterTool>,
    pub provider: Option<Arc<dyn ToolHookProvider>>,
}
/// Host member lookup is fallible and occurs outside the invocation catch.
pub trait ToolHookProvider: Send + Sync {
    fn before_tool(&self) -> Result<Option<BeforeTool>>;
    fn after_tool(&self) -> Result<Option<AfterTool>>;
}
static BEFORE: LazyLock<HookKey<ToolHooks, BeforeTool>> = LazyLock::new(|| {
    HookKey::try_new("beforeTool", |h: Arc<ToolHooks>| match &h.provider {
        Some(provider) => provider.before_tool(),
        None => Ok(h.before_tool.clone()),
    })
});
static AFTER: LazyLock<HookKey<ToolHooks, AfterTool>> = LazyLock::new(|| {
    HookKey::try_new("afterTool", |h: Arc<ToolHooks>| match &h.provider {
        Some(provider) => provider.after_tool(),
        None => Ok(h.after_tool.clone()),
    })
});
pub static TOOL_TASK: LazyLock<ToolTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "pi.tool".into(),
        version: 2,
        initial: Arc::new(|_| Ok(ToolTaskCheckpoint::Call)),
        phases: indexmap::IndexMap::from_iter([
            (
                "call".into(),
                Arc::new(|task, runtime, context| -> StorageFuture<()> {
                    Box::pin(call(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
            (
                "execute".into(),
                Arc::new(|task, runtime, context| -> StorageFuture<()> {
                    Box::pin(recover(task, runtime, context))
                }) as PhaseHandler<_, _, _, _>,
            ),
        ]),
        abort: Arc::new(|task, runtime, context| Box::pin(abort(task, runtime, context))),
        migrate: Some(Arc::new(|mut input, checkpoint, _| {
            // Old model-only inputs had no discriminant; held v1 receipts remain valid.
            if wire::get(&input, "kind").is_none() {
                wire::put(&mut input, "kind", &"model");
            }
            Ok((
                wire::decode(&input).map_err(Error::message)?,
                wire::decode(&checkpoint).map_err(Error::message)?,
            ))
        })),
        hooks: None,
    })
});
pub fn tool_task() -> ToolTask {
    TOOL_TASK.clone()
}
async fn read_call(
    runtime: &ToolRuntime,
    input: &ToolTaskInput,
    context: &Context,
) -> Result<ToolHookCall> {
    let call = match input {
        ToolTaskInput::Nested {
            parent,
            parent_call_id,
            call,
            ..
        } => ToolHookCall {
            call: (**call).clone(),
            parent: Some(ToolParent {
                task_id: *parent,
                call_id: parent_call_id.clone(),
            }),
        },
        ToolTaskInput::Model { assistant, call_id } => {
            let entry = runtime.entry(*assistant, context).await?;
            let call = entry
                .as_ref()
                .filter(|entry| entry.kind == *ASSISTANT_ENTRY.kind())
                .and_then(|entry| entry.model.as_ref())
                .and_then(|messages| messages.first())
                .filter(|message| message["role"].string_equals("assistant"))
                .and_then(|message| message["content"].as_array())
                .and_then(|content| {
                    content.iter().find(|item| {
                        item["type"].string_equals("toolCall")
                            && item["id"].as_string() == Some(call_id)
                    })
                });
            let call = call
                .map(ToolCall::from_json)
                .transpose()
                .map_err(Error::message)?
                .ok_or_else(|| {
                    Error::host(
                        "Error",
                        result::join(
                            [
                                format!("Entry {assistant} has no tool call ").into(),
                                call_id.clone(),
                            ],
                            "",
                        ),
                    )
                })?;
            ToolHookCall { call, parent: None }
        }
    };
    context
        .telemetry()
        .update_attributes(pi_telemetry::attributes! {
            "tool.name" => call.call.name.clone(), "tool.call_id" => call.call.id.clone()
        });
    Ok(call)
}
fn caught<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
}
async fn call(task: Running, runtime: ToolRuntime, context: Context) -> Result<()> {
    let input = task.record().info.input.clone();
    let call = read_call(&runtime, &input, &context).await?;
    let tool = runtime
        .agent(&context)
        .await?
        .tool(&call.call.name, input.is_nested())?;
    let Some(tool) = tool else {
        let result = harness_error(
            "tool_unavailable",
            result::join(
                [
                    "Tool ".into(),
                    call.call.name.clone(),
                    " is not available".into(),
                ],
                "",
            ),
        );
        return settle(
            &runtime,
            input,
            call,
            Ending::Completed,
            move |_| result,
            &context,
            None,
        )
        .await;
    };
    let prepared = caught(|| tool.prepare_arguments(call.call.arguments.clone()))
        .and_then(|args| caught(|| tool.validate_arguments(&call.call, args)));
    let args = match prepared {
        Ok(args) => args.into_value(),
        Err(error) => {
            let result = harness_error("invalid_arguments", error.message_text());
            return settle(
                &runtime,
                input,
                call,
                Ending::Completed,
                move |_| result,
                &context,
                None,
            )
            .await;
        }
    };
    let decision = Arc::new(Mutex::new((args, None::<JsonString>)));
    let capture = decision.clone();
    let hook_call = call.clone();
    let current = runtime.clone();
    let caller = context.clone();
    runtime
        .hooks()
        .each(&BEFORE, move |hook| {
            let capture = capture.clone();
            let mut hook_call = hook_call.clone();
            let runtime = current.clone();
            let context = caller.clone();
            async move {
                {
                    let decision = capture.lock();
                    if decision.1.is_some() {
                        return Ok(());
                    }
                    hook_call.call.arguments = decision.0.clone();
                }
                let result = std::panic::AssertUnwindSafe(async {
                    hook(hook_call, runtime.clone(), context).await
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)));
                match result {
                    Ok(Some(value)) => {
                        let mut decision = capture.lock();
                        if let Some(block) = value.block {
                            decision.1 = Some(block);
                        } else if let Some(args) = value.arguments {
                            decision.0 = args;
                        }
                    }
                    Err(error) if runtime.signal().aborted() => return Err(error),
                    Err(error) => capture.lock().1 = Some(error.message_text()),
                    _ => {}
                }
                Ok(())
            }
        })
        .await?;
    let (args, block) = decision.lock().clone();
    if let Some(block) = block {
        let result = harness_error(
            "blocked",
            result::join(["Tool call blocked: ".into(), block], ""),
        );
        return settle(
            &runtime,
            input,
            call,
            Ending::Completed,
            move |_| result,
            &context,
            None,
        )
        .await;
    }
    let args = match caught(|| tool.validate_arguments(&call.call, args)) {
        Ok(args) => args,
        Err(error) => {
            let result = harness_error("invalid_arguments", error.message_text());
            return settle(
                &runtime,
                input,
                call,
                Ending::Completed,
                move |_| result,
                &context,
                None,
            )
            .await;
        }
    };
    let conversation = runtime.conversation_id();
    let id = runtime.task_id().untyped();
    let replay = tool.replay();
    let checkpoint_args = args.clone();
    let nested_args = input.is_nested().then(|| args.clone());
    runtime
        .commit(
            move |tx, _| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                let ops = live.with_json(|raw| {
                    let mut ops = Vec::new();
                    if let Some(at) = live::locate_slot(raw, id) {
                        live::set_slot_field(
                            raw,
                            at,
                            "status",
                            wire::encode(&SlotStatus::Running),
                            &mut ops,
                        );
                        if let (Some(args), "nestedTools") = (nested_args, at.field) {
                            live::set_slot_field(raw, at, "arguments", args.into_value(), &mut ops);
                        }
                    }
                    ops
                })?;
                if !ops.is_empty() {
                    live.apply_ops(&ops)?;
                }
                Ok(Some(Next::Running {
                    checkpoint: ToolTaskCheckpoint::Execute {
                        arguments: checkpoint_args,
                        replay,
                    },
                }))
            },
            &context,
        )
        .await?;
    run(runtime, input, call, tool, args, context).await
}
async fn recover(task: Running, runtime: ToolRuntime, context: Context) -> Result<()> {
    let input = task.record().info.input.clone();
    let call = read_call(&runtime, &input, &context).await?;
    let ToolTaskCheckpoint::Execute { arguments, replay } = task.checkpoint().clone() else {
        return Err(Error::message("Expected tool execution checkpoint"));
    };
    let tool = runtime
        .agent(&context)
        .await?
        .tool(&call.call.name, input.is_nested())?;
    if replay == ReplayPolicy::Safe
        && tool
            .as_ref()
            .is_some_and(|tool| tool.replay() == ReplayPolicy::Safe)
    {
        let conversation = runtime.conversation_id();
        let id = runtime.task_id().untyped();
        runtime
            .commit(
                move |tx, _| async move {
                    let live = tx.doc(&LIVE_DOC, conversation).await?;
                    let ops = live.with_json(|raw| {
                        live::locate_slot(raw, id)
                            .map(|at| live::clear_progress_ops(raw, at))
                            .unwrap_or_default()
                    })?;
                    if !ops.is_empty() {
                        live.apply_ops(&ops)?;
                    }
                    Ok(None)
                },
                &context,
            )
            .await?;
        return run(
            runtime,
            input,
            call,
            tool.expect("safe tool"),
            arguments,
            context,
        )
        .await;
    }
    let message = result::join(
        [
            "Tool ".into(),
            call.call.name.clone(),
            " was interrupted and may have partially run".into(),
        ],
        "",
    );
    settle(
        &runtime,
        input,
        call,
        Ending::Failed(message.clone()),
        move |slot| from_slot(slot, "interrupted", message),
        &context,
        None,
    )
    .await
}
async fn abort(task: Running, runtime: ToolRuntime, context: Context) -> Result<()> {
    let input = task.record().info.input.clone();
    let call = read_call(&runtime, &input, &context).await?;
    let (code, suffix) = if task.record().info.abort_reason != Some(AbortReason::Restart) {
        ("aborted", " was aborted")
    } else if matches!(task.checkpoint(), ToolTaskCheckpoint::Execute { .. }) {
        (
            "interrupted",
            " was interrupted by a restart and may have partially run",
        )
    } else {
        (
            "abandoned",
            " was not started: its caller ended with a restart",
        )
    };
    let message = result::join(["Tool ".into(), call.call.name.clone(), suffix.into()], "");
    settle(
        &runtime,
        input,
        call,
        Ending::Aborted,
        move |slot| from_slot(slot, code, message),
        &context,
        None,
    )
    .await
}
async fn run(
    runtime: ToolRuntime,
    input: ToolTaskInput,
    call: ToolHookCall,
    tool: Arc<dyn ExecutableTool>,
    args: JsonObject,
    context: Context,
) -> Result<()> {
    let limits = tool.output_limits();
    let enabled = !matches!(
        &input,
        ToolTaskInput::Nested {
            progress: Some(false),
            ..
        }
    );
    let scope = super::api::ToolScope::new(&runtime, limits, enabled, &context)?;
    let mut api = ToolExecutionApi::new(
        runtime.clone(),
        call.call.id.clone(),
        None,
        scope.clone(),
        limits,
        tool.replay() == ReplayPolicy::Safe,
    )?;
    let mut duration = None;
    let execution = std::panic::AssertUnwindSafe(async {
        api.env = runtime.env(&context).await?;
        let started = web_time::Instant::now();
        let result =
            std::panic::AssertUnwindSafe(async { tool.execute(args, api, context.clone()).await })
                .catch_unwind()
                .await;
        duration = Some((started.elapsed().as_secs_f64() * 1000.0).round());
        result.unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)));
    scope.end().await;
    let (result, ending) = match execution {
        Ok(result) => (result, Ending::Completed),
        Err(error) if runtime.signal().aborted() => {
            for waiter in scope.progress.stop().await {
                let _ = waiter.send(Err(error.clone()));
            }
            return Err(error);
        }
        Err(error) => (
            ToolExecutionResult {
                is_error: Some(true),
                diagnostics: Some(vec![result::tool_diagnostic(
                    "tool_error".into(),
                    error.message_text(),
                )]),
                ..Default::default()
            },
            Ending::Failed(result::join(
                ["Tool ".into(), call.call.name.clone(), " threw".into()],
                "",
            )),
        ),
    };
    scope.drain_admissions().await;
    scope.reported.lock().output.end();
    let pending = scope.progress.stop().await;
    let final_result = async {
        let result = final_result(
            &runtime,
            &input,
            &call,
            tool.as_ref(),
            result,
            &scope,
            &context,
        )
        .await?;
        settle(
            &runtime,
            input,
            call,
            ending,
            move |_| result,
            &context,
            duration,
        )
        .await
    }
    .await;
    for waiter in pending {
        let _ = waiter.send(final_result.clone());
    }
    final_result
}

async fn final_result(
    runtime: &ToolRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    tool: &dyn ExecutableTool,
    mut result: ToolExecutionResult,
    scope: &super::api::ToolScope,
    context: &Context,
) -> Result<ToolExecutionResult> {
    let limits = scope.limits;
    let (retained, output) = {
        let mut reported = scope.reported.lock();
        let retained = result.output.is_none().then(|| reported.output.snapshot());
        let output = result.output.clone().unwrap_or_else(|| {
            Arc::new(
                retained
                    .as_ref()
                    .filter(|value| !value.text.units().is_empty())
                    .map(|value| vec![text_content(value.text.clone())])
                    .unwrap_or_default(),
            )
        });
        if result.details.is_none() {
            result.details = reported.details.as_deref().cloned().map(Into::into);
        }
        result.diagnostics = Some(
            reported
                .diagnostics
                .iter()
                .cloned()
                .chain(result.diagnostics.take().unwrap_or_default())
                .collect(),
        );
        result.output = Some(output.clone());
        (retained, output)
    };
    let final_value = Arc::new(Mutex::new(result));
    let capture = final_value.clone();
    let current = runtime.clone();
    let call = call.clone();
    let caller = context.clone();
    runtime
        .hooks()
        .each(&AFTER, move |hook| {
            let capture = capture.clone();
            let current = current.clone();
            let call = call.clone();
            let context = caller.clone();
            let result = capture.lock().clone();
            async move {
                if let Some(result) = hook(call, result, current, context).await? {
                    *capture.lock() = result;
                }
                Ok(())
            }
        })
        .await?;
    let mut result = final_value.lock().clone();
    let mut harness = Vec::new();
    if let Some(retained) = retained
        && retained.dropped_bytes > 0
        && result
            .output
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &output))
    {
        harness.push(result::truncated(
            retained.dropped_bytes,
            retained.dropped_lines,
            Some(limits.retain),
        ));
    }
    let bounded = bound_content(&result.output.clone().unwrap_or_default(), limits);
    let (bytes, lines) = (bounded.dropped_bytes, bounded.dropped_lines);
    let bounded = bounded.content;
    if bytes > 0 {
        harness.push(result::truncated(bytes, lines, Some(limits.retain)));
    }
    if let Some(broken) = tool.structured_output_error(&result)? {
        result.structured_output = None;
        if input.is_nested() {
            result.is_error = Some(true);
            harness.push(result::tool_diagnostic(
                "invalid_structured_output".into(),
                broken,
            ));
        } else {
            let extension = runtime
                .agent(context)
                .await
                .ok()
                .and_then(|agent| agent.tool_extension(&tool.declaration().name));
            runtime.report_extension(Error::host("Error", broken), extension)?;
        }
    }
    let schema = tool.structured_output_schema();
    if input.is_nested() && schema.is_none() {
        result.structured_output = Some(output_value(&bounded)?.into());
    }
    if input.is_nested()
        && schema.is_some()
        && result.is_error == Some(true)
        && result.structured_output.is_none()
        && !result
            .diagnostics
            .iter()
            .flatten()
            .chain(harness.iter())
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
    {
        let text = result::join(
            bounded
                .iter()
                .filter(|item| item.value()["type"].string_equals("text"))
                .filter_map(|item| item.value()["text"].as_string().cloned()),
            "\n",
        );
        if !text.units().is_empty() {
            harness.push(result::tool_diagnostic("tool_error".into(), text));
        }
    }
    result.output = Some(bounded);
    result.diagnostics.get_or_insert_default().extend(harness);
    Ok(result)
}
#[derive(Clone)]
enum Ending {
    Completed,
    Aborted,
    Failed(JsonString),
}
async fn settle(
    runtime: &ToolRuntime,
    input: ToolTaskInput,
    call: ToolHookCall,
    ending: Ending,
    build: impl FnOnce(Option<&SlotProgress>) -> ToolExecutionResult + Send + 'static,
    context: &Context,
    duration: Option<f64>,
) -> Result<()> {
    let children = runtime
        .owned_tasks(context)
        .await?
        .into_iter()
        .filter(|record| {
            record.info.kind.equals_str("pi.tool")
                && record.info.input["kind"].string_equals("nested")
        })
        .map(|record| record.id())
        .collect::<Vec<_>>();
    let made_nested = runtime
        .memo::<bool>(MADE_NESTED_CALLS, context)
        .await?
        .unwrap_or(false);
    let waits = children
        .into_iter()
        .map(|id| runtime.abort_owned(TypedTaskId::<ToolTaskResult>::from_untyped(id), context))
        .collect::<Vec<_>>();
    for result in futures::future::join_all(waits).await {
        result?;
    }
    let conversation = runtime.conversation_id();
    let id = runtime.task_id().untyped();
    let current = runtime.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                let progress = live.with_json(|raw| {
                    live::locate_slot(raw, id)
                        .map(|at| live::slot_progress(raw, at))
                        .transpose()
                })?;
                let progress = progress.map_err(Error::message)?;
                let result = build(progress.as_ref());
                let receipt = if let ToolTaskInput::Nested { parent, key, .. } = input {
                    let nested = nested_result(id, &result, duration)?;
                    if let Some(usage) = &nested.usage {
                        record_usage(
                            &tx,
                            conversation,
                            UsageBucket::Tools,
                            call.call.name.clone(),
                            usage,
                        )
                        .await?;
                    }
                    tx.doc_family(&NESTED_CALL_DOC, parent, key, ())
                        .await?
                        .update(|state| {
                            if state.task_id != Some(TypedTaskId::from_untyped(id)) {
                                return Err(Error::host(
                                    "Error",
                                    result::join(
                                        [
                                            "Nested call ".into(),
                                            call.call.id.clone(),
                                            " is not in its caller's index".into(),
                                        ],
                                        "",
                                    ),
                                ));
                            }
                            state.result = Some(nested.clone());
                            Ok(())
                        })?;
                    let summary = wire::encode(&summary_of(
                        &nested,
                        &result.output.clone().unwrap_or_default(),
                    ));
                    let ops = live.with_json(|raw| {
                        let mut ops = Vec::new();
                        if let Some(at) =
                            live::locate_slot(raw, id).filter(|at| at.field == "nestedTools")
                        {
                            ops.extend(live::finish_slot_ops(raw, at));
                            live::set_slot_field(raw, at, "summary", summary, &mut ops);
                        }
                        ops
                    })?;
                    if !ops.is_empty() {
                        live.apply_ops(&ops)?;
                    }
                    ToolTaskResult::Nested
                } else {
                    let entry = append_tool_result(
                        &tx,
                        conversation,
                        &call.call,
                        &result,
                        current.now()?,
                        duration,
                    )
                    .await?;
                    let ops = live.with_json(|raw| {
                        let mut ops = Vec::new();
                        if let Some(at) =
                            live::locate_slot(raw, id).filter(|at| at.field == "tools")
                        {
                            ops.extend(live::finish_slot_ops(raw, at));
                            live::set_slot_field(
                                raw,
                                at,
                                "entry",
                                wire::encode(&entry.id),
                                &mut ops,
                            );
                        }
                        ops
                    })?;
                    if !ops.is_empty() {
                        live.apply_ops(&ops)?;
                    }
                    ToolTaskResult::Model {
                        entry_id: entry.id,
                        control: if matches!(ending, Ending::Completed) {
                            result.control
                        } else {
                            None
                        },
                    }
                };
                if made_nested {
                    live::update_fields(&live, &["nestedTools"], |live| {
                        live::remove_nested_slots(live, id);
                    })?;
                }
                let outcome = match ending {
                    Ending::Completed => TaskOutcome::Completed { result: receipt },
                    Ending::Aborted => TaskOutcome::Aborted {
                        reason: None,
                        result: Some(receipt),
                    },
                    Ending::Failed(message) => TaskOutcome::Failed {
                        error: TaskOutcomeError {
                            message,
                            detail: None,
                        },
                        result: Some(receipt),
                    },
                };
                Ok(Some(Next::Terminal { outcome }))
            },
            context,
        )
        .await
}

pub(crate) struct AdmissionOptions {
    pub key: JsonString,
    pub progress: Option<bool>,
    pub abandon: bool,
}
pub(crate) async fn start_nested_call(
    runtime: &ToolRuntime,
    parent_call_id: JsonString,
    name: JsonString,
    arguments: JsonValue,
    options: AdmissionOptions,
    context: &Context,
) -> Result<TypedTaskId<ToolTaskResult>> {
    let AdmissionOptions {
        key,
        progress,
        abandon,
    } = options;
    let parent = runtime.task_id().untyped();
    let conversation = runtime.conversation_id();
    let call = ToolCall::new(
        result::join([parent_call_id.clone(), "/".into(), key.clone()], ""),
        name.clone(),
        arguments.clone(),
    );
    let id = Arc::new(Mutex::new(None));
    let capture = id.clone();
    let replayed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let replay_capture = replayed.clone();
    let replay_name = name.clone();
    runtime.commit(move |tx,_| async move {
        let state = tx.doc_family(&NESTED_CALL_DOC,parent,key.clone(),()).await?;
        if let Some(existing) = state.read()?.task_id {
            let same = tx.task(existing.untyped()).await?.and_then(|record| wire::decode::<ToolTaskInput>(&record.info.input).ok()).is_some_and(|input| matches!(input,ToolTaskInput::Nested {call:prior,..} if prior.name == name && prior.arguments == call.arguments));
            if !same { return Err(Error::host("Error",result::join(["Nested call ".into(),call.id," was already made with another tool or other arguments".into()],""))); }
            replay_capture.store(state.read()?.result.is_some(), std::sync::atomic::Ordering::Release);
            *capture.lock() = Some(existing); return Ok(None);
        }
        let input = ToolTaskInput::Nested {parent,parent_call_id:parent_call_id.clone(),key,call:Box::new(call.clone()),progress:progress.filter(|enabled| !enabled)};
        let mut options = TaskOptions::new(TaskOwnership::Task(parent)); options.abandon_on_restart = abandon;
        let created = tx.create_task(&TOOL_TASK,input,options).await?;
        state.update(|state| { state.task_id = Some(created); Ok(()) })?;
        let live = tx.doc(&LIVE_DOC,conversation).await?;
        let slot = NestedToolSlot {progress:SlotProgress::new(call.id,name),parent_call_id,parent_task_id:parent,task_id:created.untyped(),arguments,summary:None};
        let op = live.with_json(|raw| live::push_nested_slot_op(raw, &slot))?;
        live.apply_ops(&[op])?;
        *capture.lock() = Some(created); Ok(None)
    },context).await?;
    if replayed.load(std::sync::atomic::Ordering::Acquire) {
        context.telemetry().record_event(
            "pi.durable.replay",
            pi_telemetry::attributes! {"tool.name" => replay_name},
        );
    }
    Ok(id.lock().take().expect("nested admission captures task"))
}
