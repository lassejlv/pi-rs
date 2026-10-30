use super::{settle::*, types::*, *};
use crate::harness::{
    agent::section_extension, compaction::*, declaration::DeclaredTool, live::LIVE_DOC, prompt::*,
    registry::RegisteredTool,
};

pub(super) async fn prepare<T, S>(
    task: Running,
    runtime: GenerationRuntime,
    context: Context,
) -> Result<()>
where
    T: RegisteredTool + DeclaredTool,
    S: PromptSection<GenerationPromptInput<T, S>>,
{
    let agent = runtime.agent(&context).await?.typed::<T, S>()?;
    let settings = runtime.settings()?;
    let reference: Option<crate::harness::types::ModelRef> = agent
        .model
        .as_ref()
        .map(wire::decode)
        .transpose()
        .map_err(Error::message)?;
    let Some((reference, model)) = reference.as_ref().zip(match &reference {
        Some(reference) => resolve(&runtime, reference)?,
        None => None,
    }) else {
        return fail_no_model(&runtime, reference, &context).await;
    };
    let GenerationCheckpoint::Prepare(preparation) = task.checkpoint().clone() else {
        return Err(Error::message("Expected prepare checkpoint"));
    };
    let Preparation {
        attempt,
        compacted,
        overflow,
        threshold: blocked,
    } = preparation;
    if let Some(child) = compacted {
        if let Some(overflow) = overflow {
            let outcome = runtime.outcomes(&[child], &context).await?.remove(0);
            if !matches!(
                outcome,
                TaskOutcome::Completed {
                    result: CompactionResult {
                        entry_id: Some(_),
                        ..
                    }
                }
            ) {
                return fail_model_error(&runtime, overflow, &context).await;
            }
        }
        if blocked == Some(true) {
            let outcome = runtime.outcomes(&[child], &context).await?.remove(0);
            if !matches!(outcome, TaskOutcome::Completed { .. }) {
                return fail_model_error(&runtime, compaction_failure(outcome), &context).await;
            }
        }
    }
    let conversation = runtime.conversation_id();
    let view = runtime.context(conversation, None, &context).await?;
    let shown = replay_sections(&view.messages)?;
    let env = match runtime.env(&context).await {
        Ok(env) => env,
        Err(error) => {
            if runtime.signal().aborted() {
                return Err(error);
            }
            runtime.report_host(error, "env")?;
            None
        }
    };
    let input = Arc::new(PromptInput {
        conversation_id: conversation,
        agent: agent.clone(),
        env,
        shown: shown.clone(),
        read: runtime.clone(),
    });
    let desired = render_sections(
        &agent.sections,
        input,
        &shown,
        |error, section| runtime.report_extension(error, section_extension(&agent, section.key())),
        &context,
    )
    .await?;
    let tools = agent
        .tools
        .iter()
        .map(|tool| tool.declaration())
        .collect::<Vec<_>>();
    let entries = plan_system_entries(&view, &desired, &tools, runtime.now()?)?;
    let threshold = if compacted.is_none() {
        threshold_compaction(
            &view,
            &entries,
            model.context_window()?,
            &compaction_policy(&settings, reference)?,
        )
    } else {
        None
    };
    let owner = runtime.task_id().untyped();
    if threshold == Some(Threshold::Blocking) {
        return runtime
            .commit(
                move |tx, _| async move {
                    let child = create_compaction(
                        &tx,
                        conversation,
                        CompactionInput {
                            reason: crate::harness::live::CompactionReason::Threshold,
                            instructions: None,
                        },
                        Some(owner),
                    )
                    .await?;
                    Ok(Some(Next::Waiting {
                        checkpoint: GenerationCheckpoint::Prepare(Preparation {
                            attempt,
                            compacted: Some(child),
                            overflow: None,
                            threshold: Some(true),
                        }),
                        on: vec![child.untyped()],
                        policy: JoinPolicy::AllSettled,
                    }))
                },
                &context,
            )
            .await;
    }
    let reference = reference.clone();
    let thinking_level = agent.thinking_level.clone();
    runtime
        .commit(
            move |tx, _| async move {
                let mut cutoff = tx
                    .scan_entries(
                        EntryQuery {
                            conversation_id: conversation,
                            min_entry_id: None,
                            max_entry_id: None,
                            order: None,
                        },
                        1,
                        None,
                    )
                    .await?
                    .items
                    .first()
                    .map(|entry| entry.id);
                for entry in entries {
                    cutoff = Some(tx.append_entry(conversation, entry).await?.id);
                }
                let cutoff = cutoff.ok_or_else(|| {
                    Error::message(format!(
                        "Conversation {conversation} has no entries to send"
                    ))
                })?;
                if threshold == Some(Threshold::Background)
                    && tx
                        .doc(&LIVE_DOC, conversation)
                        .await?
                        .with_json(|raw| raw["compactions"].is_null())?
                {
                    create_compaction(
                        &tx,
                        conversation,
                        CompactionInput {
                            reason: crate::harness::live::CompactionReason::Threshold,
                            instructions: None,
                        },
                        None,
                    )
                    .await?;
                }
                Ok(Some(Next::Running {
                    checkpoint: GenerationCheckpoint::Request(GenerationRequest {
                        attempt,
                        compacted,
                        model: reference,
                        thinking_level,
                        stream_options: settings.stream,
                        cutoff,
                    }),
                }))
            },
            &context,
        )
        .await
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threshold {
    Blocking,
    Background,
}
pub fn threshold_compaction(
    view: &ContextView,
    planned: &[EntryDraft],
    context_window: f64,
    policy: &CompactionPolicy,
) -> Option<Threshold> {
    if !policy.enabled || context_window <= 0.0 {
        return None;
    }
    let extra = planned
        .iter()
        .flat_map(|entry| entry.model.iter().flatten().cloned().map(Arc::new))
        .collect::<Vec<_>>();
    let tokens = estimate_context(view, &extra);
    let blocking = context_window - policy.reserve_tokens;
    let background = blocking - policy.background_tokens;
    let over = if tokens > blocking {
        Some(Threshold::Blocking)
    } else if policy.background_tokens > 0.0 && tokens > background {
        Some(Threshold::Background)
    } else {
        None
    };
    over.filter(|_| select_cut(view, policy.keep_recent_tokens).is_some())
}
fn compaction_failure(outcome: TaskOutcome<CompactionResult>) -> JsonString {
    match outcome {
        TaskOutcome::Failed { error, .. } => joined(["Compaction failed: ".into(), error.message]),
        TaskOutcome::Faulted { error } => joined(["Compaction failed: ".into(), error.message]),
        TaskOutcome::Orphaned { reason } => joined(["Compaction failed: ".into(), reason]),
        TaskOutcome::Aborted { .. } => "Compaction was aborted".into(),
        TaskOutcome::Completed { .. } => unreachable!("caller checked unsuccessful compaction"),
    }
}
