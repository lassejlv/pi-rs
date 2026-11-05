//! Persistent background subagents. One `subagent` tool lets the main agent start named subagents, message them
//! (steer or follow up), stop them mid-answer, and list them. Subagents keep working while the main agent answers
//! the user, and each answer is delivered back to the main agent as a new message once it arrives. Everything
//! survives a restart: the example closes the Harness while a subagent works and reopens it.
//! Offline: a scripted faux model.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_ai::providers::faux::{FauxResponseStep, RegisterFauxProviderOptions, faux_text_message};
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
};
use support::*;

// ─── Product code ────────────────────────────────────────────────────────────

// Each subagent is its own conversation with its own transcript, so it remembers earlier messages. The main
// conversation keeps a small document that maps subagent names to their conversations. Documents are durable state
// next to a transcript, changed in commits like entries.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subagent {
    conversation_id: ConversationId,
    /// Answers already reported to the main agent: several messages can end in one answer, reported once.
    reported: Vec<EntryId>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct SubagentsState {
    agents: BTreeMap<String, Subagent>,
    reporters: BTreeMap<String, TaskId>,
}
impl DocumentState for SubagentsState {}
// A fork of the main conversation starts without subagents.
static SUBAGENTS: LazyLock<LatestConversationDocToken<SubagentsState>> = LazyLock::new(|| {
    define_doc(DocDefinition::new(
        "app.subagents",
        1,
        LatestConversationScope {
            fork: LatestFork::Initial,
        },
        || Ok(SubagentsState::default()),
        |_, _, _| false,
    ))
    .unwrap()
});

// Every task and conversation has an owner, and that decides what an abort or an idle wait reaches: aborting a task
// aborts what it owns, and waiting for a conversation to be idle waits for its work. A subagent must outlive the
// main agent's turns, so its conversation is owned by an anchor: a background task that finishes at once. A
// background task is a boundary: the main agent's Esc and idle waits stop there, but a background abort still
// reaches past it, so a host can stop everything.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum AnchorState {
    Done,
}
type AnchorTask = Task<(), AnchorState, ()>;
static ANCHOR: LazyLock<AnchorTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "app.subagent-anchor".into(),
        version: 1,
        initial: Arc::new(|_| Ok(AnchorState::Done)),
        phases: indexmap::IndexMap::from_iter([(
            "done".into(),
            Arc::new(
                |_: RunningTask<(), AnchorState, ()>,
                 runtime: TaskRuntime<(), AnchorState, (), ()>,
                 context| {
                    Box::pin(async move {
                        runtime
                            .commit(
                                |_, _| async {
                                    Ok(terminal(TaskOutcome::Completed { result: () }))
                                },
                                &context,
                            )
                            .await
                    }) as storage::StorageFuture<()>
                },
            ) as PhaseHandler<_, _, _>,
        )]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(terminal(TaskOutcome::Aborted {
                                reason: None,
                                result: None,
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
});

// A reporter delivers one message to a subagent and reports the answer to the main agent. It is a background task
// too, so the main agent's Esc and idle waits leave it alone. Tasks are durable: each phase ends with a saved
// checkpoint, and after a restart the task continues from the last one. Request IDs make a repeated submission
// return the first one, so the subagent gets the message once and the main agent gets the report once.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReporterInput {
    name: String,
    conversation_id: ConversationId,
    message: String,
    follow_up: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum ReporterState {
    Deliver,
    Report {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<String>,
    },
}
type ReporterTask = Task<ReporterInput, ReporterState, ()>;
type ReporterRunning = RunningTask<ReporterInput, ReporterState, ()>;
type ReporterRuntime = TaskRuntime<ReporterInput, ReporterState, (), ()>;
static REPORTER: LazyLock<ReporterTask> = LazyLock::new(|| {
    define_task(TaskDefinition {
        name: "app.subagent-reporter".into(),
        version: 1,
        initial: Arc::new(|_| Ok(ReporterState::Deliver)),
        phases: indexmap::IndexMap::from_iter([
            (
                // Send the message, wait for its answer, and decide what to report.
                "deliver".into(),
                Arc::new(
                    |reporter: ReporterRunning, runtime: ReporterRuntime, context| {
                        Box::pin(async move {
                            let input = reporter.record().info.input.clone();
                            // While the subagent is busy, a steer reaches it at its next step and a follow-up after
                            // its current answer.
                            let subagent = runtime
                                .conversation(input.conversation_id, &context)
                                .await?
                                .ok_or_else(|| {
                                    Error::message("The subagent conversation is missing")
                                })?;
                            let mut request =
                                InputSubmissionDraft::new(input.message.as_str().into());
                            request.when_busy = Some(if input.follow_up {
                                WhenBusy::FollowUp
                            } else {
                                WhenBusy::Steer
                            });
                            request.request_id =
                                Some(format!("subagent:{}", reporter.record().id()).into());
                            let settled = subagent
                                .submit(request, &context)
                                .await?
                                .wait(&context)
                                .await?;
                            let conversation = runtime.conversation_id();
                            // One commit decides the report and records the answer as delivered, so a restart does
                            // not decide again.
                            runtime
                            .commit(
                                move |tx, _| async move {
                                    let next = |report: Option<String>| {
                                        Ok(Some(NextTaskState::Running {
                                            checkpoint: ReporterState::Report { report },
                                        }))
                                    };
                                    match settled.as_record().state.clone() {
                                        // `aborted`: stopped, or withdrawn while queued. Nothing to report.
                                        SubmissionState::InputUnanswered { reason, .. } => next(
                                            if reason.equals_str("aborted") {
                                                None
                                            } else {
                                                Some(format!("[subagent {} failed: {reason}]", input.name))
                                            },
                                        ),
                                        SubmissionState::InputDone { answer, .. } => {
                                            let doc = tx.doc(&SUBAGENTS, conversation).await?;
                                            let name = input.name.clone();
                                            let first = doc.update(|state| {
                                                let agent = state
                                                    .agents
                                                    .get_mut(&name)
                                                    .ok_or_else(|| Error::message("Unknown subagent"))?;
                                                if agent.reported.contains(&answer) {
                                                    return Ok(false);
                                                }
                                                agent.reported.push(answer);
                                                Ok(true)
                                            })?;
                                            if !first {
                                                return next(None);
                                            }
                                            let entry = tx.entry(answer).await?;
                                            let text = entry.as_ref().map(message_text).unwrap_or_default();
                                            next(Some(format!(
                                                "[subagent {} answered, no reply needed] {text}",
                                                input.name
                                            )))
                                        }
                                        _ => next(None),
                                    }
                                },
                                &context,
                            )
                            .await
                        }) as storage::StorageFuture<()>
                    },
                ) as PhaseHandler<_, _, _>,
            ),
            (
                // Post the report as a follow-up input: it starts a turn when the main agent is idle, or waits
                // for its current answer. If the user presses Esc while it waits in the main agent's queue, it is
                // dropped with the other input; a report that arrives after Esc starts a new turn.
                "report".into(),
                Arc::new(
                    |reporter: ReporterRunning, runtime: ReporterRuntime, context| {
                        Box::pin(async move {
                            let report = match reporter.checkpoint() {
                                ReporterState::Report { report } => report.clone(),
                                ReporterState::Deliver => None,
                            };
                            if let Some(report) = report {
                                let main = runtime
                                    .conversation(runtime.conversation_id(), &context)
                                    .await?
                                    .ok_or_else(|| {
                                        Error::message("The main conversation is missing")
                                    })?;
                                let mut input = InputSubmissionDraft::new(report.as_str().into());
                                input.when_busy = Some(WhenBusy::FollowUp);
                                input.request_id = Some(
                                    format!("subagent-report:{}", reporter.record().id()).into(),
                                );
                                main.submit(input, &context).await?;
                            }
                            runtime
                                .commit(
                                    |_, _| async {
                                        Ok(terminal(TaskOutcome::Completed { result: () }))
                                    },
                                    &context,
                                )
                                .await
                        }) as storage::StorageFuture<()>
                    },
                ) as PhaseHandler<_, _, _>,
            ),
        ]),
        abort: Arc::new(|_, runtime, context| {
            Box::pin(async move {
                runtime
                    .commit(
                        |_, _| async {
                            Ok(terminal(TaskOutcome::Aborted {
                                reason: None,
                                result: None,
                            }))
                        },
                        &context,
                    )
                    .await
            })
        }),
        migrate: None,
        hooks: None,
    })
});

fn subagent_tool() -> NativeTool {
    // A call interrupted by a crash is not rerun: repeating `stop` could stop newer work. The model sees that the
    // call was interrupted and can check with `status`. (Unsafe replay is the default.)
    tool(
        "subagent",
        "Manage persistent subagents that work in the background. Actions: spawn (name, message), send (name, \
         message; followUp: true queues it after the current answer instead of steering), stop (name: aborts its \
         current work), status (name, or all subagents without one). Answers are reported back to you when they \
         arrive.",
        r#"{"type":"object","properties":{"action":{"type":"string","enum":["spawn","send","stop","status"]},"name":{"type":"string"},"message":{"type":"string"},"followUp":{"type":"boolean"}},"required":["action"]}"#,
        |args, api, context| {
            Box::pin(async move {
                let args = sj(args.value());
                let action = args["action"].as_str().unwrap_or("").to_string();
                let name = args["name"].as_str().map(str::to_string);
                let message = args["message"].as_str().map(str::to_string);
                let follow_up = args["followUp"].as_bool().unwrap_or(false);
                let reply = |text: &str, conversation: Option<ConversationId>| {
                    let mut result = text_result(text);
                    // A UI can attach to the subagent's conversation through the call's details.
                    if let (Some(conversation), Some(name)) = (conversation, &name) {
                        result.details = Some(
                            from_sj(json!({"name": name, "conversationId": conversation.get()}))
                                .into(),
                        );
                    }
                    Ok::<_, Error>(result)
                };
                let main = api.conversation_id();
                let registry = api
                    .snapshot(&SUBAGENTS, main, &context)
                    .await?
                    .map(|state| (*state).clone())
                    .unwrap_or_default();

                if action == "status" {
                    let names = match &name {
                        Some(name) => vec![name.clone()],
                        None => registry.agents.keys().cloned().collect(),
                    };
                    let mut lines = Vec::new();
                    for each in names {
                        let Some(found) = registry.agents.get(&each) else {
                            continue;
                        };
                        // A conversation is busy while it has a run: from an input until its final answer.
                        let busy = api
                            .snapshot(&LIVE_DOC, found.conversation_id, &context)
                            .await?
                            .is_some_and(|live| live.run.is_some());
                        lines.push(format!("{each}: {}", if busy { "working" } else { "idle" }));
                    }
                    return reply(
                        &if lines.is_empty() {
                            "No subagents.".into()
                        } else {
                            lines.join("\n")
                        },
                        None,
                    );
                }
                let Some(name) = name.clone() else {
                    return reply(&format!("{action} needs a name."), None);
                };
                let agent = registry.agents.get(&name).cloned();
                if action != "spawn" && agent.is_none() {
                    return reply(&format!("No subagent named {name}."), None);
                }

                if action == "stop" {
                    // Aborts the subagent's current answer and tools and drops its queued messages. It stays
                    // usable.
                    let agent = agent.expect("checked above");
                    api.conversation(agent.conversation_id, &context)
                        .await?
                        .ok_or_else(|| Error::message("The subagent conversation is missing"))?
                        .abort(&context, false)
                        .await?;
                    return reply(&format!("Stopped {name}."), Some(agent.conversation_id));
                }
                let Some(message) = message else {
                    return reply(&format!("{action} needs a message."), None);
                };

                // spawn and send: one commit starts a reporter for the message.
                let call_task = api.task_id().untyped();
                let spawn = action == "spawn";
                let who = name.clone();
                let result = api
                    .commit(
                        move |tx| async move {
                            let doc = tx.doc(&SUBAGENTS, main).await?;
                            let state = doc.read()?;
                            // Both tasks belong to the main conversation and are background: its Esc and idle
                            // waits skip them.
                            let background = || {
                                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                                options.background = true;
                                options
                            };
                            if spawn {
                                if state.agents.contains_key(&who) {
                                    return Ok(format!("{who} already exists; use send."));
                                }
                                let anchor = tx.create_task(&ANCHOR, (), background()).await?;
                                // Owned by a task of the main conversation: starts as a copy of the main agent.
                                let child = tx.create_conversation(ConversationOwnership::Task(anchor.untyped())).await?;
                                // Subagents cannot start subagents, and know who they are.
                                configure(
                                    &tx,
                                    child.id,
                                    change(json!({
                                        "extensions": {"remove": [{"name": "subagent-tools"}]},
                                        "instructions": format!("You are the subagent \"{who}\". Answer the main agent's requests."),
                                    })),
                                )
                                .await?;
                                let key = who.clone();
                                doc.update(|state| {
                                    state.agents.insert(key, Subagent { conversation_id: child.id, reported: Vec::new() });
                                    Ok(())
                                })?;
                            }
                            let conversation_id = doc
                                .read()?
                                .agents
                                .get(&who)
                                .map(|agent| agent.conversation_id)
                                .ok_or_else(|| Error::message("Unknown subagent"))?;
                            let input = ReporterInput {
                                name: who.clone(),
                                conversation_id,
                                message,
                                follow_up: !spawn && follow_up,
                            };
                            let reporter = tx.create_task(&REPORTER, input, background()).await?;
                            doc.update(|state| {
                                state.reporters.insert(call_task.to_string(), reporter.untyped());
                                Ok(())
                            })?;
                            Ok(if spawn { format!("Started {who}.") } else { format!("Sent to {who}.") })
                        },
                        &context,
                    )
                    .await?;
                let current = api
                    .snapshot(&SUBAGENTS, main, &context)
                    .await?
                    .and_then(|state| state.agents.get(&name).map(|agent| agent.conversation_id));
                reply(&result, current)
            })
        },
    )
}

// The task definitions come with the extension, so pending reporters resume after a restart once the host installs
// it again.
fn subagent_tools() -> Ext {
    let mut extension = Ext::new("subagent-tools");
    extension.tasks.push(ANCHOR.erased());
    extension.tasks.push(REPORTER.erased());
    extension.tools.push(Arc::new(subagent_tool()));
    extension
}

// ─── Host setup ─────────────────────────────────────────────────────────────

/// Route each request by its last message: the main agent calls the tool and repeats what it said, while the
/// subagent gives short answers and a long chapter walk-through that is stopped halfway.
fn route() -> FauxResponseStep {
    FauxResponseStep::Factory(Arc::new(|transcript, _, _, _| {
        let transcript = serde_json::to_value(transcript).expect("transcript JSON");
        Box::pin(async move {
            let say = |text: &str| Ok(faux_text_message(text));
            let tool_call = |input: Value| {
                let step = call("subagent", input, None);
                match step {
                    FauxResponseStep::Message(message) => Ok(message),
                    FauxResponseStep::Factory(_) => unreachable!(),
                }
            };
            // System messages carry prompt changes; the request is about the message before them.
            let last = transcript["messages"]
                .as_array()
                .and_then(|messages| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| message["role"] != "system")
                })
                .cloned()
                .unwrap_or(Value::Null);
            let text = content_text(&last["content"]);
            // The main agent repeats what the tool said.
            if last["role"] == "toolResult" {
                return say(&format!("OK. {text}"));
            }
            // The main agent.
            if text.contains("Start a subagent") {
                return tool_call(
                    json!({"action": "spawn", "name": "reader", "message": "Summarize the plot of Moby Dick."}),
                );
            }
            if text.contains("whale's name") {
                return tool_call(
                    json!({"action": "send", "name": "reader", "message": "What is the whale called?"}),
                );
            }
            if text.contains("every chapter") {
                return tool_call(
                    json!({"action": "send", "name": "reader", "message": "Now go through all chapters in detail."}),
                );
            }
            if text.contains("Stop reader") {
                return tool_call(json!({"action": "stop", "name": "reader"}));
            }
            if text.contains("my subagents") {
                return tool_call(json!({"action": "status"}));
            }
            if text.contains("[subagent") {
                return say("Noted.");
            }
            // The subagent: short answers, and a long chapter walk-through that is stopped halfway.
            if text.contains("Summarize the plot") {
                return say("A whale, a captain, an obsession.");
            }
            if text.contains("whale called") {
                return say("Moby Dick.");
            }
            let chapters = (1..=135)
                .map(|n| format!("Chapter {n}: more whaling."))
                .collect::<Vec<_>>();
            say(&chapters.join("\n"))
        })
    }))
}

type Lines = Arc<Mutex<Vec<String>>>;

/// Print the main conversation's messages as they are committed. Subagents work in their own conversations.
async fn follow(
    harness: &Harness,
    id: ConversationId,
    lines: Lines,
    streams: &mut Vec<AgentEventStream>,
) -> Result<()> {
    let stream = harness.watch_events(id, &ctx()).await?;
    stream.start(move |events, _| {
        let lines = lines.clone();
        async move {
            for event in events.iter() {
                let event = serde_json::to_value(event).map_err(|e| e.to_string())?;
                if event["type"] != "message_end" {
                    continue;
                }
                let message = &event["entry"]["model"][0];
                match message["role"].as_str() {
                    Some("user") => {
                        let text = content_text(&message["content"]);
                        // Input is either the user or a subagent's report, which arrives whenever the subagent
                        // is done.
                        let line = match text.strip_prefix("[subagent ") {
                            Some(rest) => {
                                let (name, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                                let body = tail
                                    .split_once(']')
                                    .map_or(tail, |(_, body)| body.trim_start());
                                let body = if body.is_empty() {
                                    tail.trim_end_matches(']')
                                } else {
                                    body
                                };
                                format!("> {name}: {body}")
                            }
                            None => format!("> {text}"),
                        };
                        println!("\n{line}");
                        lines.lock().push(line);
                    }
                    Some("assistant") => {
                        for part in message["content"].as_array().into_iter().flatten() {
                            if part["type"] != "toolCall" {
                                continue;
                            }
                            let args = &part["arguments"];
                            let sent = args["message"]
                                .as_str()
                                .map(|m| format!(" {m:?}"))
                                .unwrap_or_default();
                            let line = format!(
                                "  {} {}{}{sent}",
                                part["name"].as_str().unwrap_or(""),
                                args["action"].as_str().unwrap_or(""),
                                args["name"]
                                    .as_str()
                                    .map(|n| format!(" {n}"))
                                    .unwrap_or_default(),
                            );
                            println!("{line}");
                            lines.lock().push(line);
                        }
                        let text = content_text(&message["content"]);
                        if !text.is_empty() {
                            println!("{text}");
                            lines.lock().push(text);
                        }
                    }
                    Some("toolResult") => {
                        let line = format!("  → {}", content_text(&message["content"]));
                        println!("{line}");
                        lines.lock().push(line);
                    }
                    _ => {}
                }
            }
            Ok(())
        }
    })?;
    streams.push(stream);
    Ok(())
}

async fn open(
    registry: &Reg,
    path: &std::path::Path,
    models: pi_ai::models::Models,
) -> Result<(Harness, Conversation)> {
    let storage = open_native_sqlite_storage(path, NativeSqliteStorageOptions::default()).await?;
    let harness = Harness::open(
        Arc::new(storage),
        HarnessOptions::new(registry.clone(), models),
        &ctx(),
    )
    .await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &ctx())
        .await?;
    Ok((harness, root))
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    // The main agent and the subagent share one scripted model, which answers each request by its last message.
    // It streams its answers at 50 tokens per second, like a slow real model.
    let (models, faux) = faux(RegisterFauxProviderOptions {
        tokens_per_second: Some(50.0),
        ..Default::default()
    });
    // More responses than the script needs; each request takes the next one.
    faux.set_responses((0..40).map(|_| route()).collect());
    let registry = registry();
    install(&registry, subagent_tools());
    let directory = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    let path = directory.path().join("session.sqlite");

    let lines: Lines = Arc::default();
    let mut streams = Vec::new();
    let (mut harness, mut root) = open(&registry, &path, models.clone()).await?;
    follow(&harness, root.id, lines.clone(), &mut streams).await?;

    // Say something to the main agent and wait for its answer.
    let say = |root: Conversation, text: &'static str| async move {
        root.submit(SubmissionDraft::input(text), &ctx())
            .await?
            .wait(&ctx())
            .await?;
        Ok::<_, Error>(())
    };
    // Wait until every message to a subagent was answered and reported, and the main agent has reacted.
    let settle = |harness: Harness, root: Conversation| async move {
        let ctx = ctx();
        let reporters = harness
            .snapshot(&SUBAGENTS, root.id, &ctx)
            .await?
            .map(|state| state.reporters.values().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for id in reporters {
            harness.wait_for_task(id, &ctx).await?;
        }
        root.wait_for_idle(&ctx).await?;
        // Event callbacks run after their commit; let the last ones print. A slow machine may need longer.
        pause(100).await;
        Ok::<_, Error>(())
    };
    // Poll `check` for up to 10 seconds.
    let working = |harness: Harness, root: Conversation, name: &'static str| async move {
        let ctx = ctx();
        let Some(state) = harness.snapshot(&SUBAGENTS, root.id, &ctx).await? else {
            return Ok::<_, Error>(false);
        };
        let Some(agent) = state.agents.get(name) else {
            return Ok(false);
        };
        Ok(harness
            .snapshot(&LIVE_DOC, agent.conversation_id, &ctx)
            .await?
            .is_some_and(|live| live.run.is_some()))
    };

    // The main agent answers at once; the subagent's answer is reported back when it arrives.
    say(
        root.clone(),
        "Start a subagent named reader that summarizes Moby Dick.",
    )
    .await?;
    settle(harness.clone(), root.clone()).await?;
    let snapshot = lines.lock().clone();
    check(
        snapshot
            .iter()
            .any(|l| l.starts_with("  subagent spawn reader")),
        "the main agent spawned the subagent",
    )?;
    check(
        snapshot.iter().any(|l| l == "  → Started reader."),
        "the tool reported the start",
    )?;
    check(
        snapshot
            .iter()
            .any(|l| l == "> reader: A whale, a captain, an obsession."),
        "the subagent's answer was reported back",
    )?;
    check(
        snapshot.iter().any(|l| l == "Noted."),
        "the main agent reacted to the report",
    )?;

    // A long request, stopped while the subagent is still answering.
    say(root.clone(), "Ask reader to summarize every chapter.").await?;
    for _ in 0..1000 {
        if working(harness.clone(), root.clone(), "reader").await? {
            break;
        }
        pause(10).await;
    }
    check(
        working(harness.clone(), root.clone(), "reader").await?,
        "the subagent is working on the long request",
    )?;
    say(root.clone(), "Stop reader.").await?;
    settle(harness.clone(), root.clone()).await?;
    let snapshot = lines.lock().clone();
    check(
        snapshot.iter().any(|l| l == "  → Stopped reader."),
        "the subagent was stopped",
    )?;
    check(
        !snapshot.iter().any(|l| l.contains("Chapter 1:")),
        "a stopped answer is not reported",
    )?;

    say(root.clone(), "What are my subagents doing?").await?;
    settle(harness.clone(), root.clone()).await?;
    let snapshot = lines.lock().clone();
    check(
        snapshot.iter().any(|l| l == "  → reader: idle"),
        "the stopped subagent is idle and still usable",
    )?;

    // The process stops while the subagent works on a message; after the restart its answer still arrives.
    let reporters = |harness: Harness, root: Conversation| async move {
        Ok::<_, Error>(
            harness
                .snapshot(&SUBAGENTS, root.id, &ctx())
                .await?
                .map_or(0, |state| state.reporters.len()),
        )
    };
    let before = reporters(harness.clone(), root.clone()).await?;
    root.submit(
        SubmissionDraft::input("Ask reader for the whale's name."),
        &context,
    )
    .await?;
    for _ in 0..1000 {
        if reporters(harness.clone(), root.clone()).await? > before {
            break;
        }
        pause(10).await;
    }
    check(
        reporters(harness.clone(), root.clone()).await? > before,
        "a reporter was started for the message",
    )?;
    harness.close(&context).await?;
    println!("\n  (process restarts)");
    streams.clear();
    (harness, root) = open(&registry, &path, models).await?;
    follow(&harness, root.id, lines.clone(), &mut streams).await?;
    settle(harness.clone(), root.clone()).await?;
    let snapshot = lines.lock().clone();
    check(
        snapshot
            .iter()
            .filter(|l| l.as_str() == "> reader: Moby Dick.")
            .count()
            == 1,
        "the answer arrived exactly once after the restart",
    )?;

    harness.close(&context).await
}
