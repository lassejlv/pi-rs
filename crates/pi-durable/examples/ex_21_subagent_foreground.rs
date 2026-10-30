//! A foreground subagent tool: the parent's tool call creates a child conversation it owns, runs one task there,
//! and returns the child's answer. A UI finds the child through the tool's running details and shows its events
//! under the call. Offline: a scripted faux model.
#[path = "ex_b_support/mod.rs"]
mod support;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_durable::harness::tool::ReplayPolicy;
use pi_durable::*;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};
use support::*;

// ─── Product code: the subagent extension ───────────────────────────────────

fn subagent_extension() -> Ext {
    let mut subagent = tool(
        "subagent",
        "Delegate a self-contained task to a subagent and get its answer back.",
        r#"{"type":"object","properties":{"task":{"type":"string","description":"What the subagent should do"}},"required":["task"]}"#,
        |args, api, context| {
            Box::pin(async move {
                let task = sj(args.value())["task"].as_str().unwrap_or("").to_string();
                let task_id = api.task_id().untyped();
                // The child is owned by this tool call's task, so aborting the call aborts the child, and the
                // call finishes only once the child's work is done.
                let child = api
                    .commit(
                        move |tx| async move {
                            // Ownership records the child: a rerun of this call finds it instead of creating
                            // another.
                            let found = tx
                                .scan_conversations(
                                    ConversationQuery {
                                        owner_task_id: Some(task_id),
                                        ..Default::default()
                                    },
                                    1,
                                    None,
                                )
                                .await?;
                            if let Some(existing) = found.items.first() {
                                return Ok(existing.id);
                            }
                            // Starts as a copy of this conversation's agent: model, thinking level, cwd,
                            // extensions, tools.
                            let created = tx
                                .create_conversation(ConversationOwnership::Task(task_id))
                                .await?;
                            // Without this extension, the child is not offered this tool.
                            configure(
                                &tx,
                                created.id,
                                object(r#"{"extensions":{"remove":[{"name":"subagent"}]}}"#),
                            )
                            .await?;
                            Ok(created.id)
                        },
                        &context,
                    )
                    .await?;
                // A UI watching the parent sees this and can attach to the child.
                api.details(from_sj(json!({"conversationId": child.get()})), &context)
                    .await?;

                let handle = api
                    .conversation(child, &context)
                    .await?
                    .ok_or_else(|| Error::message("The child conversation vanished"))?;
                // The request ID makes a rerun get back the submission it made before the crash.
                let mut request = InputSubmissionDraft::new(task.as_str().into());
                request.request_id = Some(format!("subagent:{task_id}").into());
                let settled = handle
                    .submit(request, &context)
                    .await?
                    .wait(&context)
                    .await?;
                let SubmissionState::InputDone { answer, .. } = settled.as_record().state.clone()
                else {
                    return Err(Error::message(format!(
                        "Subagent failed: {:?}",
                        status_of(&settled)
                    )));
                };
                let entry = api
                    .commit(move |tx| async move { tx.entry(answer).await }, &context)
                    .await?
                    .ok_or_else(|| Error::message("The subagent answer is missing"))?;
                let mut result = text_result(&message_text(&entry));
                result.details = Some(from_sj(json!({"conversationId": child.get()})).into());
                Ok(result)
            }) as BoxFuture<'static, Result<ToolExecutionResult>>
        },
    );
    // Safe to rerun after a crash: a rerun finds the child it already created and the submission it already made.
    subagent.replay = ReplayPolicy::Safe;
    let mut extension = Ext::new("subagent");
    extension.tools.push(Arc::new(subagent));
    extension
}

// ─── UI: the parent's events, with each subagent's events indented under its call ───

type Lines = Arc<Mutex<Vec<String>>>;
fn print(lines: &Lines, indent: &str, event: &Value) {
    if event["type"] == "message_end" && event["entry"]["kind"] == "pi.assistant" {
        let text = content_text(&event["entry"]["model"][0]["content"]);
        if !text.is_empty() {
            lines.lock().push(format!("{indent}assistant: {text}"));
        }
    } else if event["type"] == "tool_execution_start" {
        lines.lock().push(format!(
            "{indent}tool {}({})",
            event["toolName"].as_str().unwrap_or(""),
            event["args"]
        ));
    }
}
fn attach(
    harness: Harness,
    id: ConversationId,
    indent: String,
    lines: Lines,
    attached: Arc<Mutex<HashSet<i64>>>,
    streams: Arc<Mutex<Vec<AgentEventStream>>>,
) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        attached.lock().insert(id.get());
        let stream = harness.watch_events(id, &ctx()).await?;
        let (host, indent, lines, attached, collected) = (
            harness.clone(),
            indent.clone(),
            lines.clone(),
            attached.clone(),
            streams.clone(),
        );
        stream.start(move |events, _| {
            let (host, indent, lines, attached, collected) = (
                host.clone(),
                indent.clone(),
                lines.clone(),
                attached.clone(),
                collected.clone(),
            );
            async move {
                for event in events.iter() {
                    let event = serde_json::to_value(event).map_err(|e| e.to_string())?;
                    print(&lines, &indent, &event);
                    if event["type"] != "tool_execution_update" {
                        continue;
                    }
                    if let Some(child) = event["details"]["conversationId"].as_i64()
                        && !attached.lock().contains(&child)
                    {
                        attach(
                            host.clone(),
                            ConversationId::from_raw(child),
                            format!("{indent}  "),
                            lines.clone(),
                            attached.clone(),
                            collected.clone(),
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            }
        })?;
        streams.lock().push(stream);
        Ok(())
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    // The parent delegates, the child answers, and the parent reports.
    let (models, faux) = faux(fast());
    faux.set_responses(vec![
        call(
            "subagent",
            json!({"task": "Name three prime numbers."}),
            Some("call-1"),
        ),
        answer("2, 3, and 5."),
        answer("The subagent says: 2, 3, and 5."),
    ]);
    let registry = registry();
    install(&registry, subagent_extension());
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &context,
    )
    .await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;

    let lines: Lines = Arc::default();
    let streams = Arc::default();
    attach(
        harness.clone(),
        root.id,
        String::new(),
        lines.clone(),
        Arc::default(),
        streams,
    )
    .await?;

    let submission = root
        .submit(
            SubmissionDraft::input(
                "Use the subagent tool to find three prime numbers, then tell me what it said.",
            ),
            &context,
        )
        .await?;
    submission.wait(&context).await?;
    harness.wait_for_idle(&context).await?;
    // Event callbacks run after their commit; let the last ones print.
    pause(100).await;
    let lines = lines.lock().clone();
    for line in &lines {
        println!("{line}");
    }
    check(
        lines
            .iter()
            .any(|l| l.starts_with("tool subagent(") && l.contains("prime numbers")),
        "parent tool start",
    )?;
    check(
        lines.iter().any(|l| l == "  assistant: 2, 3, and 5."),
        "child answer indented under the call",
    )?;
    check(
        lines.last().map(String::as_str) == Some("assistant: The subagent says: 2, 3, and 5."),
        "parent reports the child's answer",
    )?;
    // The child is a conversation the tool call owns.
    let owned = root
        .commit(
            |tx| async move {
                tx.scan_conversations(ConversationQuery::default(), 10, None)
                    .await
            },
            &context,
        )
        .await?;
    check(
        owned.items.iter().filter(|c| c.owner.is_some()).count() == 1,
        "exactly one owned child",
    )?;
    harness.close(&context).await
}
