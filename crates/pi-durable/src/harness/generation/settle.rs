use super::{types::*, *};
use crate::harness::{
    inbox::*,
    live::{self, LIVE_DOC},
    tool::{append_tool_result, harness_error},
    types::{ModelRef, QueueModes},
    usage::{UsageBucket, record_usage},
};

pub(super) async fn append_assistant(
    tx: &Transaction,
    conversation: ConversationId,
    message: GenerationMessage,
) -> Result<EntryRecord> {
    let provider: JsonString = message.required("provider")?;
    let model: JsonString = message.required("model")?;
    let usage: JsonObject = message.required("usage")?;
    record_usage(
        tx,
        conversation,
        UsageBucket::Models,
        joined([provider, "/".into(), model]),
        &usage,
    )
    .await?;
    let mut entry = EntryDraft::new(ASSISTANT_ENTRY.kind().clone());
    entry.model = Some(vec![message.copy_json()?.into_value()]);
    tx.append_entry(conversation, entry).await
}
pub async fn convert_partial(
    tx: &Transaction,
    live: &DocDraft<LiveState>,
    conversation: ConversationId,
) -> Result<()> {
    if let Some(mut message) =
        live.with_json(|raw| JsonObject::try_from_value(raw["generation"]["message"].clone()).ok())?
    {
        message.insert("stopReason".into(), wire::encode(&"aborted"));
        append_assistant(
            tx,
            conversation,
            crate::harness::models::AssistantValue::json(message),
        )
        .await?;
    }
    Ok(())
}
struct Failure {
    policy: JsonString,
    boundary: Option<Boundary>,
}
async fn prepare_failure(tx: &Transaction, runtime: &GenerationRuntime) -> Result<Failure> {
    let settings = runtime.settings()?;
    Ok(Failure {
        boundary: if settings.after_failed_run.equals_str("continue") {
            Some(
                prepare_boundary(tx, runtime.conversation_id(), QueueModes::from(&settings))
                    .await?,
            )
        } else {
            None
        },
        policy: settings.after_failed_run,
    })
}
async fn end_failed_run(
    tx: &Transaction,
    runtime: &GenerationRuntime,
    live: &DocDraft<LiveState>,
    mut failure: Failure,
    settlement: SubmissionSettlement,
) -> Result<()> {
    let conversation = runtime.conversation_id();
    live::end_run(tx, live, runtime.task_id().untyped(), settlement)?;
    if failure.policy.equals_str("withdraw") {
        withdraw_queued_inputs(tx, conversation, "run_failed").await?;
    }
    if let Some(boundary) = &mut failure.boundary {
        let result = apply_boundary(tx, boundary, BoundaryAt::Final, runtime.now()?).await?;
        if !result.users.is_empty() {
            start_run(tx, conversation, live, result.users).await?;
        }
    }
    Ok(())
}
pub(super) async fn fail_model_error(
    runtime: &GenerationRuntime,
    text: JsonString,
    context: &Context,
) -> Result<()> {
    fail(runtime, text, "model_error", true, context).await
}
pub(super) async fn fail_no_model(
    runtime: &GenerationRuntime,
    reference: Option<ModelRef>,
    context: &Context,
) -> Result<()> {
    let message = reference.map_or_else(
        || "No model is configured".into(),
        |reference| {
            joined([
                "Model ".into(),
                reference.provider,
                "/".into(),
                reference.model_id,
                " is not available".into(),
            ])
        },
    );
    fail(runtime, message, "no_model", false, context).await
}
async fn fail(
    runtime: &GenerationRuntime,
    text: JsonString,
    reason: &'static str,
    include_detail: bool,
    context: &Context,
) -> Result<()> {
    let current = runtime.clone();
    let conversation = runtime.conversation_id();
    runtime
        .commit(
            move |tx, _| async move {
                let failure = prepare_failure(&tx, &current).await?;
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                end_failed_run(
                    &tx,
                    &current,
                    &live,
                    failure,
                    SubmissionSettlement::Unanswered {
                        reason: reason.into(),
                        detail: include_detail.then(|| wire::encode(&text)),
                    },
                )
                .await?;
                Ok(Some(failed(text, reason)))
            },
            context,
        )
        .await
}
fn failed(text: JsonString, reason: &str) -> Next {
    let mut detail = wire::object();
    wire::put(&mut detail, "reason", &reason);
    Next::Terminal {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: text,
                detail: Some(detail),
            },
            result: None,
        },
    }
}
pub(super) async fn classify(
    runtime: GenerationRuntime,
    request: ResponseRequest,
    message: GenerationMessage,
    context: Context,
) -> Result<()> {
    runtime.signal().check()?;
    let conversation = runtime.conversation_id();
    let ResponseRequest {
        attempt,
        compacted,
        model: reference,
        cutoff,
        ..
    } = request.clone();
    if message
        .field("stopReason")?
        .is_some_and(|value| value.string_equals("deferred"))
        && let Some(handle) = message.field("deferred")?
    {
        let handle = JsonObject::try_from_value(handle).map_err(Error::message)?;
        let poll_at = (runtime.now()? + handle.value()["pollAfterMs"].as_f64().unwrap_or(5000.0))
            .max(request.poll_at.map_or(f64::NEG_INFINITY, |at| at + 1.0));
        return runtime
            .commit(
                move |tx, _| async move {
                    let value = crate::harness::live::LiveGeneration {
                        attempt,
                        message: None,
                        retry: None,
                        deferred: Some(crate::harness::live::DeferredStatus { poll_at }),
                    };
                    tx.doc(&LIVE_DOC, conversation)
                        .await?
                        .apply_ops(&[Op::Set(vec!["generation".into()], wire::encode(&value))])?;
                    Ok(Some(Next::Sleeping {
                        checkpoint: GenerationCheckpoint::Poll(DeferredPoll {
                            attempt,
                            compacted,
                            model: reference,
                            cutoff,
                            handle,
                            poll_at,
                        }),
                        until: poll_at,
                    }))
                },
                &context,
            )
            .await;
    }
    let shared_message = message;
    let current = runtime.clone();
    let callback_context = context.clone();
    let passed = shared_message.clone();
    runtime
        .hooks()
        .each(&AFTER_RESPONSE, move |hook| {
            hook(passed.clone(), current.clone(), callback_context.clone())
        })
        .await?;
    let message = shared_message.clone();
    let calls = message
        .tool_calls()?
        .iter()
        .map(|block| wire::decode(block).map_err(Error::message))
        .collect::<Result<Vec<crate::harness::tool::ToolCall>>>()?;
    let stop = message.field("stopReason")?.unwrap_or(JsonValue::Null);
    if stop.string_equals("toolUse") && !calls.is_empty() {
        return super::tools::start_tool_round(runtime, request, message, calls, context).await;
    }
    if ["stop", "length", "toolUse"]
        .iter()
        .any(|value| stop.string_equals(value))
    {
        return answer(runtime, shared_message, context).await;
    }
    let settings = runtime.settings()?;
    let error_text: Option<JsonString> = message
        .field("errorMessage")?
        .map(|value| wire::decode(&value).map_err(Error::message))
        .transpose()?
        .flatten();
    let provider = pi_ai::AssistantMessage {
        stop_reason: if stop.string_equals("error") {
            pi_ai::StopReason::Error
        } else {
            pi_ai::StopReason::Pending
        },
        provider: message
            .required::<JsonString>("provider")?
            .to_string_checked()
            .map_err(|error| Error::message(error.to_string()))?,
        error_message: error_text
            .as_ref()
            .map(JsonString::to_string_checked)
            .transpose()
            .map_err(|error| Error::message(error.to_string()))?,
        ..Default::default()
    };
    let overflow = stop.string_equals("error") && pi_ai::is_context_overflow(&provider, None);
    let policy = crate::harness::compaction::compaction_policy(&settings, &reference)?;
    if overflow && compacted.is_none() && policy.enabled {
        let view = runtime
            .context(conversation, Some(cutoff), &context)
            .await?;
        if crate::harness::compaction::select_cut(&view, policy.keep_recent_tokens).is_some() {
            let text = provider
                .error_message
                .unwrap_or_else(|| "Context overflow".into());
            let owner = runtime.task_id().untyped();
            return runtime
                .commit(
                    move |tx, _| async move {
                        let live = tx.doc(&LIVE_DOC, conversation).await?;
                        append_assistant(&tx, conversation, message).await?;
                        live.apply_ops(&[Op::Delete(vec!["generation".into()])])?;
                        let child = crate::harness::compaction::create_compaction(
                            &tx,
                            conversation,
                            crate::harness::compaction::CompactionInput {
                                reason: crate::harness::live::CompactionReason::Overflow,
                                instructions: None,
                            },
                            Some(owner),
                        )
                        .await?;
                        Ok(Some(Next::Waiting {
                            checkpoint: GenerationCheckpoint::Prepare(Preparation {
                                attempt,
                                compacted: Some(child),
                                overflow: Some(text.into()),
                                threshold: None,
                            }),
                            on: vec![child.untyped()],
                            policy: JoinPolicy::AllSettled,
                        }))
                    },
                    &context,
                )
                .await;
        }
    }
    let policy = &settings.retry;
    let retry = stop.string_equals("error")
        && !overflow
        && pi_ai::is_retryable_assistant_error(&provider)
        && policy.value()["enabled"].as_bool() == Some(true)
        && attempt as f64 <= policy.value()["maxRetries"].as_f64().unwrap_or(0.0);
    let until = if retry {
        runtime.now()? + super::request::retry_delay(policy, attempt)
    } else {
        0.0
    };
    let current = runtime.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let failure = if retry {
                    None
                } else {
                    Some(prepare_failure(&tx, &current).await?)
                };
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                append_assistant(&tx, conversation, message).await?;
                let Some(failure) = failure else {
                    let generation = crate::harness::live::LiveGeneration {
                        attempt,
                        message: None,
                        retry: Some(crate::harness::live::RetryStatus {
                            at: until,
                            error: provider.error_message.unwrap_or_default().into(),
                        }),
                        deferred: None,
                    };
                    live.apply_ops(&[Op::Set(
                        vec!["generation".into()],
                        wire::encode(&generation),
                    )])?;
                    return Ok(Some(Next::Sleeping {
                        checkpoint: GenerationCheckpoint::Retry(Backoff {
                            attempt,
                            compacted,
                            until,
                        }),
                        until,
                    }));
                };
                let text: JsonString =
                    provider.error_message.map(Into::into).unwrap_or_else(|| {
                        joined([
                            "Model response ended with stop reason ".into(),
                            stop.as_string().cloned().unwrap_or_default(),
                        ])
                    });
                end_failed_run(
                    &tx,
                    &current,
                    &live,
                    failure,
                    SubmissionSettlement::Unanswered {
                        reason: "model_error".into(),
                        detail: Some(wire::encode(&text)),
                    },
                )
                .await?;
                Ok(Some(failed(text, "model_error")))
            },
            &context,
        )
        .await
}
async fn answer(
    runtime: GenerationRuntime,
    message: GenerationMessage,
    context: Context,
) -> Result<()> {
    let continuation = Arc::new(parking_lot::Mutex::new(None));
    let captured = continuation.clone();
    let current = runtime.clone();
    let callback_context = context.clone();
    let passed = message.clone();
    runtime
        .hooks()
        .each(&ON_YIELD, move |hook| {
            let continuation = captured.clone();
            let message = passed.clone();
            let runtime = current.clone();
            let context = callback_context.clone();
            async move {
                if continuation.lock().is_none() {
                    let next = hook(message, runtime, context).await?;
                    *continuation.lock() = next;
                }
                Ok(())
            }
        })
        .await?;
    let continuation = continuation.lock().clone();
    let current = runtime.clone();
    let conversation = runtime.conversation_id();
    let owner = runtime.task_id().untyped();
    runtime
        .commit(
            move |tx, _| async move {
                let mut boundary =
                    prepare_boundary(&tx, conversation, QueueModes::from(&current.settings()?))
                        .await?;
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                let entry = append_assistant(&tx, conversation, message).await?;
                let next = Next::Terminal {
                    outcome: TaskOutcome::Completed {
                        result: GenerationResult { entry_id: entry.id },
                    },
                };
                let result =
                    apply_boundary(&tx, &mut boundary, BoundaryAt::Final, current.now()?).await?;
                if let Some(continuation) = continuation
                    && result.users.is_empty()
                    && !result.reset
                {
                    append_input(&tx, conversation, continuation, None, current.now()?).await?;
                    hand_over(
                        &live,
                        owner,
                        create_generation(&tx, conversation, None).await?,
                    )?;
                    live.apply_ops(&[Op::Delete(vec!["generation".into()])])?;
                    return Ok(Some(next));
                }
                live::end_run(
                    &tx,
                    &live,
                    owner,
                    SubmissionSettlement::Done { answer: entry.id },
                )?;
                if !result.users.is_empty() {
                    start_run(&tx, conversation, &live, result.users).await?;
                }
                Ok(Some(next))
            },
            &context,
        )
        .await
}
pub(super) async fn abort(
    task: Running,
    runtime: GenerationRuntime,
    context: Context,
) -> Result<()> {
    if let GenerationCheckpoint::Poll(poll) = task.checkpoint()
        && let Some(model) = resolve(&runtime, &poll.model)?
        && let Err(error) = runtime
            .model_collection()?
            .cancel_deferred(
                model,
                poll.handle.clone(),
                pi_chord::context::with_abort_signal(runtime.signal(), &context),
            )
            .await
    {
        runtime.report_host(error, "models")?;
    }
    let unstarted = if let GenerationCheckpoint::Tools(round) = task.checkpoint() {
        super::tools::read_calls(&runtime, round.assistant, &round.pending, &context).await?
    } else {
        Vec::new()
    };
    let current = runtime.clone();
    let conversation = runtime.conversation_id();
    let owner = runtime.task_id().untyped();
    runtime
        .commit(
            move |tx, _| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                convert_partial(&tx, &live, conversation).await?;
                for call in unstarted {
                    let result = harness_error(
                        "aborted",
                        joined(["Tool ".into(), call.name.clone(), " was aborted".into()]),
                    );
                    append_tool_result(&tx, conversation, &call, &result, current.now()?, None)
                        .await?;
                }
                live::end_run(
                    &tx,
                    &live,
                    owner,
                    SubmissionSettlement::Unanswered {
                        reason: "aborted".into(),
                        detail: None,
                    },
                )?;
                Ok(Some(Next::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                }))
            },
            &context,
        )
        .await
}
