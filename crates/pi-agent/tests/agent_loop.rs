//! Port of `packages/agent/test/agent-loop.test.ts`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::*;
use parking_lot::Mutex;
use pi_agent::hooks;
use pi_agent::{
    AfterToolCallResult, AgentContext, AgentEvent, AgentEventStream, AgentLoopConfig,
    AgentLoopTurnUpdate, AgentMessage, AgentRequestUpdate, AgentToolResult, AgentTurnDecision,
    BeforeToolCallResult, CustomMessage, FnAgentTool, RunToolCallOptions, ThinkingLevel,
    ToolExecutionMode, agent_loop, agent_loop_continue, event_sink, run_agent_loop, run_tool_call,
    set_default_stream_fn,
};
use pi_ai::{
    Message, StopReason, SystemContent, SystemMessage, TextContent, ThinkingLevel as Reasoning,
    ToolResultMessage, Usage, UsageCost, UserContent, UserContentBlock, UserMessage,
};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

fn identity_config() -> AgentLoopConfig {
    AgentLoopConfig::new(create_model())
}

async fn drain(mut stream: AgentEventStream) -> (Vec<AgentEvent>, Vec<AgentMessage>) {
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await {
        events.push(event);
    }
    let messages = stream.try_result().await.expect("loop result");
    (events, messages)
}

fn types(events: &[AgentEvent]) -> Vec<&'static str> {
    events.iter().map(AgentEvent::type_name).collect()
}

fn tool_results(messages: &[AgentMessage]) -> Vec<ToolResultMessage> {
    messages
        .iter()
        .filter_map(|m| m.as_tool_result().cloned())
        .collect()
}

#[tokio::test]
async fn uses_the_configured_default_when_stream_fn_is_omitted() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    set_default_stream_fn(Some(mock_stream_fn(move |_, _, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        respond(assistant_text("fallback"))
    })));
    let stream = agent_loop(
        vec![user("Hello")],
        AgentContext::new(vec![], vec![]),
        identity_config(),
        None,
        None,
    );
    let _ = drain(stream).await;
    set_default_stream_fn(None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn emits_events_with_agent_message_types() {
    let (stream_fn, _) = scripted_stream_fn(|_| assistant_text("Hi there!"));
    let stream = agent_loop(
        vec![user("Hello")],
        AgentContext::new(vec![], vec![]),
        identity_config(),
        None,
        Some(stream_fn),
    );
    let (events, messages) = drain(stream).await;
    assert_eq!(roles(&messages), vec!["user", "assistant"]);
    let event_types = types(&events);
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(event_types.contains(&expected), "missing {expected}");
    }
}

#[tokio::test]
async fn builds_provider_context_exclusively_from_transcript_messages() {
    let initial_system = SystemMessage {
        content: SystemContent::Text("Transcript prompt".into()),
        tools_added: Some(vec![]),
        timestamp: 1,
        ..Default::default()
    };
    let expected = initial_system.clone();
    let seen = Arc::new(Mutex::new(None));
    let seen_clone = seen.clone();
    let stream_fn = mock_stream_fn(move |_, context, _| {
        *seen_clone.lock() = context.messages.first().cloned();
        respond(assistant_text("done"))
    });
    let stream = agent_loop(
        vec![AgentMessage::System(initial_system), user("Hello")],
        AgentContext::new(vec![], vec![]),
        identity_config(),
        None,
        Some(stream_fn),
    );
    let _ = drain(stream).await;
    assert_eq!(seen.lock().clone(), Some(Message::System(expected)));
}

#[tokio::test]
async fn handles_custom_message_types_via_convert_to_llm() {
    let mut fields = Map::new();
    fields.insert("text".into(), json!("This is a notification"));
    fields.insert("timestamp".into(), json!(1));
    let notification = AgentMessage::Custom(CustomMessage::new("notification", fields));

    let converted = Arc::new(Mutex::new(Vec::<Message>::new()));
    let converted_clone = converted.clone();
    let mut config = identity_config();
    config.convert_to_llm = hooks::convert_to_llm(move |messages| {
        let result: Vec<Message> = messages
            .into_iter()
            .filter(|m| m.role() != "notification")
            .filter_map(|m| match m {
                AgentMessage::User(_)
                | AgentMessage::Assistant(_)
                | AgentMessage::ToolResult(_) => m.to_message(),
                _ => None,
            })
            .collect();
        *converted_clone.lock() = result.clone();
        async move { result }
    });
    let (stream_fn, _) = scripted_stream_fn(|_| assistant_text("Response"));
    let stream = agent_loop(
        vec![user("Hello")],
        AgentContext::new(vec![notification], vec![]),
        config,
        None,
        Some(stream_fn),
    );
    let _ = drain(stream).await;
    let converted = converted.lock().clone();
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].role(), "user");
}

#[tokio::test]
async fn applies_transform_context_before_convert_to_llm() {
    let context = AgentContext::new(
        vec![
            user("old message 1"),
            assistant_text("old response 1").into(),
            user("old message 2"),
            assistant_text("old response 2").into(),
        ],
        vec![],
    );
    let transformed = Arc::new(AtomicUsize::new(0));
    let converted = Arc::new(AtomicUsize::new(0));
    let (t, c) = (transformed.clone(), converted.clone());
    let mut config = identity_config();
    config.transform_context = Some(hooks::transform_context(move |messages, _| {
        let pruned = messages[messages.len() - 2..].to_vec();
        t.store(pruned.len(), Ordering::SeqCst);
        async move { pruned }
    }));
    config.convert_to_llm = hooks::convert_to_llm(move |messages| {
        let result: Vec<Message> = messages
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    AgentMessage::User(_)
                        | AgentMessage::Assistant(_)
                        | AgentMessage::ToolResult(_)
                )
            })
            .filter_map(AgentMessage::to_message)
            .collect();
        c.store(result.len(), Ordering::SeqCst);
        async move { result }
    });
    let (stream_fn, _) = scripted_stream_fn(|_| assistant_text("Response"));
    let _ = drain(agent_loop(
        vec![user("new message")],
        context,
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(transformed.load(Ordering::SeqCst), 2);
    assert_eq!(converted.load(Ordering::SeqCst), 2);
}

fn usage(values: [u64; 5], cost: [f64; 5]) -> Usage {
    Usage {
        input: values[0],
        output: values[1],
        cache_read: values[2],
        cache_write: values[3],
        total_tokens: values[4],
        cost: UsageCost {
            input: cost[0],
            output: cost[1],
            cache_read: cost[2],
            cache_write: cost[3],
            total: cost[4],
        },
        ..Default::default()
    }
}

fn tool_then_done(
    calls: Vec<pi_ai::ToolCall>,
) -> impl Fn(usize) -> pi_ai::AssistantMessage + Send + Sync + 'static {
    move |index| {
        if index == 0 {
            assistant_tool_calls(calls.clone(), StopReason::ToolUse)
        } else {
            assistant_text("done")
        }
    }
}

#[tokio::test]
async fn handles_tool_calls_and_results() {
    let tool_usage = usage([1, 2, 3, 4, 10], [0.1, 0.2, 0.3, 0.4, 1.0]);
    let patched_usage = usage([5, 6, 7, 8, 26], [0.5, 0.6, 0.7, 0.8, 2.6]);
    let executed = Arc::new(Mutex::new(Vec::new()));
    let executed_clone = executed.clone();
    let tool_usage_clone = tool_usage.clone();
    let tool = FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        move |_, params, _, _| {
            let executed = executed_clone.clone();
            let usage = tool_usage_clone.clone();
            async move {
                let value = params["value"].as_str().unwrap().to_string();
                executed.lock().push(value.clone());
                let mut result = text_result(format!("echoed: {value}"), json!({ "value": value }));
                result.usage = Some(usage);
                Ok(result)
            }
        },
    )
    .into_ref();

    let observed = Arc::new(Mutex::new(None));
    let observed_clone = observed.clone();
    let patched = patched_usage.clone();
    let mut config = identity_config();
    config.after_tool_call = Some(hooks::after_tool_call(move |ctx, _| {
        *observed_clone.lock() = ctx.result.usage.clone();
        let patched = patched.clone();
        async move {
            Ok(Some(AfterToolCallResult {
                usage: Some(patched),
                ..Default::default()
            }))
        }
    }));

    let (stream_fn, _) = scripted_stream_fn(tool_then_done(vec![tool_call(
        "tool-1",
        "echo",
        json!({"value": "hello"}),
    )]));
    let (events, messages) = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;

    assert_eq!(*executed.lock(), vec!["hello"]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
    );
    let end = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .expect("tool end");
    if let AgentEvent::ToolExecutionEnd { is_error, .. } = end {
        assert!(!is_error);
    }
    assert_eq!(*observed.lock(), Some(tool_usage));
    let results = tool_results(&messages);
    assert_eq!(results[0].usage, Some(patched_usage));
}

#[tokio::test]
async fn records_how_long_execute_took_excluding_hooks() {
    let tool = FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        |_, params, _, _| async move {
            sleep_ms(30).await;
            let value = params["value"].as_str().unwrap().to_string();
            Ok(text_result(value.clone(), json!({ "value": value })))
        },
    )
    .into_ref();
    let mut config = identity_config();
    config.before_tool_call = Some(hooks::before_tool_call(|ctx, _| async move {
        sleep_ms(100).await;
        Ok(
            (ctx.tool_call.id == "blocked").then(|| BeforeToolCallResult {
                block: true,
                reason: Some("no".into()),
                ..Default::default()
            }),
        )
    }));
    let (stream_fn, _) = scripted_stream_fn(tool_then_done(vec![
        tool_call("ran", "echo", json!({"value": "a"})),
        tool_call("blocked", "echo", json!({"value": "b"})),
    ]));
    let (_, messages) = drain(agent_loop(
        vec![user("go")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    let results = tool_results(&messages);
    let ran = results[0].duration_ms.expect("ran has duration");
    assert!((25..100).contains(&ran), "duration {ran}");
    assert!(results[1].is_error);
    assert_eq!(results[1].duration_ms, None);
    assert!(
        !serde_json::to_value(&results[1])
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("durationMs")
    );
}

#[tokio::test]
async fn does_not_execute_tool_calls_from_a_length_truncated_message() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let (stream_fn, calls) = scripted_stream_fn(|index| {
        if index == 0 {
            assistant_tool_calls(
                vec![tool_call("tool-1", "echo", json!({"value": "hel"}))],
                StopReason::Length,
            )
        } else {
            assistant_text("done")
        }
    });
    let (events, messages) = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![tool]),
        identity_config(),
        None,
        Some(stream_fn),
    ))
    .await;
    assert!(executed.lock().is_empty());
    let end = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .expect("tool end");
    if let AgentEvent::ToolExecutionEnd {
        is_error, result, ..
    } = end
    {
        assert!(is_error);
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("output token limit"));
    }
    assert_eq!(*calls.lock(), 2);
    assert_eq!(messages.last().unwrap().role(), "assistant");
}

#[tokio::test]
async fn executes_mutated_before_tool_call_args_without_revalidation() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executed_clone = executed.clone();
    let tool = FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        move |_, params, _, _| {
            let executed = executed_clone.clone();
            async move {
                executed.lock().push(params["value"].clone());
                Ok(text_result(
                    format!("echoed: {}", params["value"]),
                    json!({ "value": params["value"] }),
                ))
            }
        },
    )
    .into_ref();
    let mut config = identity_config();
    config.before_tool_call = Some(hooks::before_tool_call(|ctx, _| async move {
        let mut args = ctx.args.clone();
        args["value"] = json!(123);
        Ok(Some(BeforeToolCallResult {
            args: Some(args),
            ..Default::default()
        }))
    }));
    let (stream_fn, _) = scripted_stream_fn(tool_then_done(vec![tool_call(
        "tool-1",
        "echo",
        json!({"value": "hello"}),
    )]));
    let _ = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*executed.lock(), vec![json!(123)]);
}

#[tokio::test]
async fn prepares_tool_arguments_for_validation() {
    let schema = json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "oldText": { "type": "string" }, "newText": { "type": "string" } },
                    "required": ["oldText", "newText"]
                }
            }
        },
        "required": ["edits"]
    });
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executed_clone = executed.clone();
    let tool = FnAgentTool::new(
        "edit",
        "Edit",
        "Edit tool",
        schema,
        move |_, params, _, _| {
            let executed = executed_clone.clone();
            async move {
                let count = params["edits"].as_array().map(Vec::len).unwrap_or(0);
                executed.lock().push(params["edits"].clone());
                Ok(text_result(
                    format!("edited {count}"),
                    json!({ "count": count }),
                ))
            }
        },
    )
    .with_prepare_arguments(|args| {
        let (Some(Value::String(old_text)), Some(Value::String(new_text))) =
            (args.get("oldText"), args.get("newText"))
        else {
            return None;
        };
        let mut edits = args
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        edits.push(json!({ "oldText": old_text, "newText": new_text }));
        let mut prepared = Map::new();
        prepared.insert("edits".into(), Value::Array(edits));
        Some(prepared)
    })
    .into_ref();
    let (stream_fn, _) = scripted_stream_fn(tool_then_done(vec![tool_call(
        "tool-1",
        "edit",
        json!({"oldText": "before", "newText": "after"}),
    )]));
    let _ = drain(agent_loop(
        vec![user("edit something")],
        AgentContext::new(vec![], vec![tool]),
        identity_config(),
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(
        *executed.lock(),
        vec![json!([{ "oldText": "before", "newText": "after" }])]
    );
}

/// A tool whose "first" call waits for `release`; records whether "second" ran first.
fn gated_tool(
    name: &str,
    release: Arc<Notify>,
    first_resolved: Arc<AtomicBool>,
    parallel_observed: Arc<AtomicBool>,
    mode: Option<ToolExecutionMode>,
) -> pi_agent::AgentToolRef {
    let tool = FnAgentTool::new(
        name,
        name,
        format!("{name} tool"),
        value_schema(),
        move |_, params, _, _| {
            let release = release.clone();
            let first_resolved = first_resolved.clone();
            let parallel_observed = parallel_observed.clone();
            async move {
                let value = params["value"].as_str().unwrap().to_string();
                if value == "first" {
                    release.notified().await;
                    first_resolved.store(true, Ordering::SeqCst);
                }
                if value == "second" && !first_resolved.load(Ordering::SeqCst) {
                    parallel_observed.store(true, Ordering::SeqCst);
                }
                Ok(text_result(
                    format!("echoed: {value}"),
                    json!({ "value": value }),
                ))
            }
        },
    );
    match mode {
        Some(mode) => tool.with_execution_mode(mode).into_ref(),
        None => tool.into_ref(),
    }
}

/// Stream fn: first call returns two tool calls and releases `release` after 20ms.
fn two_call_stream_fn(name: &'static str, release: Arc<Notify>) -> pi_agent::StreamFn {
    let calls = Arc::new(AtomicUsize::new(0));
    mock_stream_fn(move |_, _, _| {
        let index = calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            let release = release.clone();
            tokio::spawn(async move {
                sleep_ms(20).await;
                release.notify_one();
            });
            respond(assistant_tool_calls(
                vec![
                    tool_call("tool-1", name, json!({"value": "first"})),
                    tool_call("tool-2", name, json!({"value": "second"})),
                ],
                StopReason::ToolUse,
            ))
        } else {
            respond(assistant_text("done"))
        }
    })
}

#[tokio::test]
async fn emits_tool_execution_end_in_completion_order_but_persists_results_in_source_order() {
    let release = Arc::new(Notify::new());
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let tool = gated_tool(
        "echo",
        release.clone(),
        Arc::new(AtomicBool::new(false)),
        parallel_observed.clone(),
        None,
    );
    let mut config = identity_config();
    config.tool_execution = Some(ToolExecutionMode::Parallel);
    let (events, _) = drain(agent_loop(
        vec![user("echo both")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(two_call_stream_fn("echo", release)),
    ))
    .await;

    let end_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let result_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(r),
            } => Some(r.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let turn_ids: Vec<String> = events
        .iter()
        .flat_map(|e| match e {
            AgentEvent::TurnEnd { tool_results, .. } => tool_results
                .iter()
                .map(|r| r.tool_call_id.clone())
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    assert!(parallel_observed.load(Ordering::SeqCst));
    assert_eq!(end_ids, vec!["tool-2", "tool-1"]);
    assert_eq!(result_ids, vec!["tool-1", "tool-2"]);
    assert_eq!(turn_ids, vec!["tool-1", "tool-2"]);
}

#[tokio::test]
async fn injects_queued_messages_after_all_tool_calls_complete() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let tool = echo_tool(executed.clone());
    let delivered = Arc::new(AtomicBool::new(false));
    let executed_clone = executed.clone();
    let mut config = identity_config();
    config.tool_execution = Some(ToolExecutionMode::Sequential);
    config.get_steering_messages = Some(hooks::get_messages(move || {
        let ready = !executed_clone.lock().is_empty() && !delivered.swap(true, Ordering::SeqCst);
        async move {
            if ready {
                vec![user("interrupt")]
            } else {
                vec![]
            }
        }
    }));

    let saw_interrupt = Arc::new(AtomicBool::new(false));
    let saw = saw_interrupt.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_fn = mock_stream_fn(move |_, context, _| {
        let index = calls.fetch_add(1, Ordering::SeqCst);
        if index == 1 && llm_user_texts(&context).iter().any(|t| t == "interrupt") {
            saw.store(true, Ordering::SeqCst);
        }
        if index == 0 {
            respond(assistant_tool_calls(
                vec![
                    tool_call("tool-1", "echo", json!({"value": "first"})),
                    tool_call("tool-2", "echo", json!({"value": "second"})),
                ],
                StopReason::ToolUse,
            ))
        } else {
            respond(assistant_text("done"))
        }
    });
    let (events, _) = drain(agent_loop(
        vec![user("start")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;

    assert_eq!(*executed.lock(), vec!["first", "second"]);
    let ends: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![false, false]);
    let sequence: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageStart {
                message: AgentMessage::ToolResult(r),
            } => Some(format!("tool:{}", r.tool_call_id)),
            AgentEvent::MessageStart { message } => user_text_of(message),
            _ => None,
        })
        .collect();
    let index_of = |s: &str| sequence.iter().position(|x| x == s).unwrap();
    assert!(index_of("tool:tool-1") < index_of("interrupt"));
    assert!(index_of("tool:tool-2") < index_of("interrupt"));
    assert!(saw_interrupt.load(Ordering::SeqCst));
}

#[tokio::test]
async fn forces_sequential_execution_when_a_tool_requests_it() {
    let release = Arc::new(Notify::new());
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let tool = gated_tool(
        "slow",
        release.clone(),
        Arc::new(AtomicBool::new(false)),
        parallel_observed.clone(),
        Some(ToolExecutionMode::Sequential),
    );
    let (events, _) = drain(agent_loop(
        vec![user("run both")],
        AgentContext::new(vec![], vec![tool]),
        identity_config(),
        None,
        Some(two_call_stream_fn("slow", release)),
    ))
    .await;
    assert!(!parallel_observed.load(Ordering::SeqCst));
    let result_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(r),
            } => Some(r.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(result_ids, vec!["tool-1", "tool-2"]);
}

#[tokio::test]
async fn forces_sequential_execution_when_one_of_multiple_tools_requests_it() {
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let release = Arc::new(Notify::new());
    let (o1, r1) = (order.clone(), release.clone());
    let slow = FnAgentTool::new(
        "slow",
        "Slow",
        "Slow tool",
        value_schema(),
        move |_, params, _, _| {
            let order = o1.clone();
            let release = r1.clone();
            async move {
                let value = params["value"].as_str().unwrap().to_string();
                order.lock().push(format!("slow:{value}"));
                if value == "a" {
                    release.notified().await;
                }
                Ok(text_result(
                    format!("slow: {value}"),
                    json!({ "value": value }),
                ))
            }
        },
    )
    .with_execution_mode(ToolExecutionMode::Sequential)
    .into_ref();
    let o2 = order.clone();
    let fast = FnAgentTool::new(
        "fast",
        "Fast",
        "Fast tool",
        value_schema(),
        move |_, params, _, _| {
            let order = o2.clone();
            async move {
                let value = params["value"].as_str().unwrap().to_string();
                order.lock().push(format!("fast:{value}"));
                Ok(text_result(
                    format!("fast: {value}"),
                    json!({ "value": value }),
                ))
            }
        },
    )
    .into_ref();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_fn = mock_stream_fn(move |_, _, _| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let release = release.clone();
            tokio::spawn(async move {
                sleep_ms(20).await;
                release.notify_one();
            });
            respond(assistant_tool_calls(
                vec![
                    tool_call("tool-1", "slow", json!({"value": "a"})),
                    tool_call("tool-2", "fast", json!({"value": "b"})),
                ],
                StopReason::ToolUse,
            ))
        } else {
            respond(assistant_text("done"))
        }
    });
    let _ = drain(agent_loop(
        vec![user("run both")],
        AgentContext::new(vec![], vec![slow, fast]),
        identity_config(),
        None,
        Some(stream_fn),
    ))
    .await;
    let order = order.lock().clone();
    assert_eq!(order[0], "slow:a");
    assert!(order.contains(&"fast:b".to_string()));
}

#[tokio::test]
async fn allows_parallel_execution_when_all_tools_are_parallel() {
    let release = Arc::new(Notify::new());
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let tool = gated_tool(
        "echo",
        release.clone(),
        Arc::new(AtomicBool::new(false)),
        parallel_observed.clone(),
        Some(ToolExecutionMode::Parallel),
    );
    let _ = drain(agent_loop(
        vec![user("echo both")],
        AgentContext::new(vec![], vec![tool]),
        identity_config(),
        None,
        Some(two_call_stream_fn("echo", release)),
    ))
    .await;
    assert!(parallel_observed.load(Ordering::SeqCst));
}

fn terminating_echo() -> pi_agent::AgentToolRef {
    FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        |_, params, _, _| async move {
            let value = params["value"].as_str().unwrap().to_string();
            let mut result = text_result(value.clone(), json!({ "value": value }));
            result.terminate = Some(true);
            Ok(result)
        },
    )
    .into_ref()
}

#[tokio::test]
async fn runs_finish_turn_after_tool_result_messages_and_before_turn_end() {
    let ordering = Arc::new(Mutex::new(Vec::<String>::new()));
    let o = ordering.clone();
    let mut config = identity_config();
    config.finish_turn = Some(hooks::finish_turn(move |turn, _| {
        o.lock().push("finishTurn".into());
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.context.messages.last().unwrap().role(), "toolResult");
        async { None }
    }));
    let o = ordering.clone();
    let (stream_fn, _) = scripted_stream_fn(|_| {
        assistant_tool_calls(
            vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
            StopReason::ToolUse,
        )
    });
    run_agent_loop(
        vec![user("echo")],
        AgentContext::new(vec![], vec![terminating_echo()]),
        config,
        event_sink(move |event| match &event {
            AgentEvent::MessageEnd { message } => {
                o.lock().push(format!("message_end:{}", message.role()))
            }
            AgentEvent::TurnEnd { .. } => o.lock().push("turn_end".into()),
            _ => {}
        }),
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();
    let ordering = ordering.lock().clone();
    assert_eq!(
        ordering[ordering.len() - 3..],
        ["message_end:toolResult", "finishTurn", "turn_end"]
    );
}

async fn finish_turn_on_failure(reason: StopReason) {
    let ordering = Arc::new(Mutex::new(Vec::<String>::new()));
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let follow_up_polls = Arc::new(AtomicUsize::new(0));
    let mut config = identity_config();
    let o = ordering.clone();
    config.finish_turn = Some(hooks::finish_turn(move |turn, _| {
        assert_eq!(turn.message.stop_reason, reason);
        o.lock().push("finishTurn".into());
        async { Some(AgentTurnDecision::Continue) }
    }));
    let s = steering_polls.clone();
    config.get_steering_messages = Some(hooks::get_messages(move || {
        s.fetch_add(1, Ordering::SeqCst);
        async { vec![] }
    }));
    let f = follow_up_polls.clone();
    config.get_follow_up_messages = Some(hooks::get_messages(move || {
        f.fetch_add(1, Ordering::SeqCst);
        async { vec![user("queued")] }
    }));
    let (stream_fn, calls) = scripted_stream_fn(move |_| {
        let mut message = assistant(vec![], reason);
        message.error_message = Some(reason.as_str().into());
        message
    });
    let o = ordering.clone();
    run_agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![]),
        config,
        event_sink(move |event| {
            if let AgentEvent::TurnEnd { .. } = event {
                o.lock().push("turn_end".into());
            }
        }),
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();
    assert_eq!(*ordering.lock(), vec!["finishTurn", "turn_end"]);
    assert_eq!(*calls.lock(), 1);
    assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
    assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn runs_finish_turn_for_an_error_assistant_without_changing_the_hard_exit() {
    finish_turn_on_failure(StopReason::Error).await;
}

#[tokio::test]
async fn runs_finish_turn_for_an_aborted_assistant_without_changing_the_hard_exit() {
    finish_turn_on_failure(StopReason::Aborted).await;
}

#[tokio::test]
async fn action_end_skips_queue_polling_and_next_turn_preparation() {
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let follow_up_polls = Arc::new(AtomicUsize::new(0));
    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let mut config = identity_config();
    config.finish_turn = Some(hooks::finish_turn(|_, _| async {
        Some(AgentTurnDecision::End)
    }));
    let p = prepare_calls.clone();
    config.prepare_next_turn = Some(hooks::prepare_next_turn(move |_| {
        p.fetch_add(1, Ordering::SeqCst);
        async { None }
    }));
    let s = steering_polls.clone();
    config.get_steering_messages = Some(hooks::get_messages(move || {
        s.fetch_add(1, Ordering::SeqCst);
        async { vec![] }
    }));
    let f = follow_up_polls.clone();
    config.get_follow_up_messages = Some(hooks::get_messages(move || {
        f.fetch_add(1, Ordering::SeqCst);
        async { vec![user("queued")] }
    }));
    let (stream_fn, calls) = scripted_stream_fn(|_| {
        assistant_tool_calls(
            vec![tool_call("tool-1", "noop", json!({}))],
            StopReason::ToolUse,
        )
    });
    let _ = drain(agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![noop_tool("noop")]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 1);
    assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
    assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);
}

fn continue_once_config() -> (AgentLoopConfig, Arc<AtomicUsize>) {
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let f = finish_calls.clone();
    let mut config = identity_config();
    config.finish_turn = Some(hooks::finish_turn(move |_, _| {
        let n = f.fetch_add(1, Ordering::SeqCst) + 1;
        async move { (n == 1).then_some(AgentTurnDecision::Continue) }
    }));
    (config, finish_calls)
}

#[tokio::test]
async fn makes_exactly_one_context_only_request_when_no_natural_request_satisfies_continuation() {
    let (config, finish_calls) = continue_once_config();
    let (stream_fn, calls) =
        scripted_stream_fn(|index| assistant_text(&format!("response {}", index + 1)));
    let _ = drain(agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 2);
    assert_eq!(finish_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn lets_a_natural_tool_result_request_satisfy_continuation() {
    let (config, finish_calls) = continue_once_config();
    let (stream_fn, calls) =
        scripted_stream_fn(tool_then_done(vec![tool_call("tool-1", "noop", json!({}))]));
    let _ = drain(agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![noop_tool("noop")]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 2);
    assert_eq!(finish_calls.load(Ordering::SeqCst), 2);
}

async fn natural_queue_request_satisfies_continuation(steering: bool) {
    let kind = if steering { "steering" } else { "follow-up" };
    let (mut config, finish_calls) = continue_once_config();
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let follow_up_delivered = Arc::new(AtomicBool::new(false));
    let s = steering_polls.clone();
    config.get_steering_messages = Some(hooks::get_messages(move || {
        let n = s.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if steering && n == 2 {
                vec![user("steering")]
            } else {
                vec![]
            }
        }
    }));
    let d = follow_up_delivered.clone();
    config.get_follow_up_messages = Some(hooks::get_messages(move || {
        let deliver = !steering && !d.swap(true, Ordering::SeqCst);
        async move {
            if deliver {
                vec![user("follow-up")]
            } else {
                vec![]
            }
        }
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let second_request_users = Arc::new(Mutex::new(Vec::<String>::new()));
    let (c, users) = (calls.clone(), second_request_users.clone());
    let stream_fn = mock_stream_fn(move |_, context, _| {
        if c.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
            users.lock().extend(llm_user_texts(&context));
        }
        respond(assistant_text("done"))
    });
    let _ = drain(agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(finish_calls.load(Ordering::SeqCst), 2);
    assert!(second_request_users.lock().contains(&kind.to_string()));
}

#[tokio::test]
async fn lets_a_natural_steering_request_satisfy_continuation() {
    natural_queue_request_satisfies_continuation(true).await;
}

#[tokio::test]
async fn lets_a_natural_follow_up_request_satisfy_continuation() {
    natural_queue_request_satisfies_continuation(false).await;
}

#[tokio::test]
async fn prepares_the_initial_request_after_pending_messages_and_can_replace_request_state() {
    let mut replacement_model = create_model();
    replacement_model.id = "replacement".into();
    replacement_model.name = "replacement".into();
    let canonical = user("canonical projection");
    let steering = user("steering");
    let completed = Arc::new(Mutex::new(Vec::<AgentMessage>::new()));
    let delivered = Arc::new(AtomicBool::new(false));
    let prepare_calls = Arc::new(AtomicUsize::new(0));

    let mut config = identity_config();
    let (d, st) = (delivered.clone(), steering.clone());
    config.get_steering_messages = Some(hooks::get_messages(move || {
        let deliver = !d.swap(true, Ordering::SeqCst);
        let st = st.clone();
        async move { if deliver { vec![st] } else { vec![] } }
    }));
    let (p, c, st, can, model) = (
        prepare_calls.clone(),
        completed.clone(),
        steering.clone(),
        canonical.clone(),
        replacement_model.clone(),
    );
    config.prepare_request = Some(hooks::prepare_request(move |request, _| {
        p.fetch_add(1, Ordering::SeqCst);
        assert!(c.lock().contains(&st));
        assert!(request.context.messages.contains(&st));
        let mut context = request.context.clone();
        context.messages = vec![can.clone()];
        let model = model.clone();
        async move {
            Ok(Some(AgentRequestUpdate {
                context: Some(context),
                model: Some(model),
                thinking_level: Some(ThinkingLevel::High),
            }))
        }
    }));

    let (expected_model, expected_message) =
        (replacement_model.clone(), canonical.to_message().unwrap());
    let stream_fn = mock_stream_fn(move |model, context, options| {
        assert_eq!(model, expected_model);
        assert_eq!(context.messages, vec![expected_message.clone()]);
        assert_eq!(options.reasoning, Some(Reasoning::High));
        respond(assistant_text("done"))
    });
    let c = completed.clone();
    run_agent_loop(
        vec![user("prompt")],
        AgentContext::new(vec![], vec![]),
        config,
        event_sink(move |event| {
            if let AgentEvent::MessageEnd { message } = event {
                c.lock().push(message);
            }
        }),
        None,
        Some(stream_fn),
    )
    .await
    .unwrap();
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn does_not_poll_steering_after_prepare_request() {
    let queued = Arc::new(Mutex::new(Vec::<AgentMessage>::new()));
    let late = user("late steering");
    let included = Arc::new(Mutex::new(Vec::<bool>::new()));
    let preparations = Arc::new(AtomicUsize::new(0));
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let mut config = identity_config();
    let (q, s) = (queued.clone(), steering_polls.clone());
    config.get_steering_messages = Some(hooks::get_messages(move || {
        s.fetch_add(1, Ordering::SeqCst);
        let drained: Vec<AgentMessage> = q.lock().drain(..).collect();
        async move { drained }
    }));
    let (q, p, l) = (queued.clone(), preparations.clone(), late.clone());
    config.prepare_request = Some(hooks::prepare_request(move |_, _| {
        if p.fetch_add(1, Ordering::SeqCst) + 1 == 1 {
            q.lock().push(l.clone());
        }
        async { Ok(None) }
    }));
    let (inc, late_message) = (included.clone(), late.to_message().unwrap());
    let stream_fn = mock_stream_fn(move |_, context, _| {
        inc.lock().push(context.messages.contains(&late_message));
        respond(assistant_text("done"))
    });
    let _ = drain(agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*included.lock(), vec![false, true]);
    assert_eq!(preparations.load(Ordering::SeqCst), 2);
    // Startup, post-turn delivery, then the final natural-stop check.
    assert_eq!(steering_polls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn rejects_the_run_when_prepare_request_fails() {
    let mut config = identity_config();
    config.prepare_request = Some(hooks::prepare_request(|_, _| async {
        Err(anyhow::anyhow!("router unavailable"))
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let stream_fn = mock_stream_fn(move |_, _, _| {
        c.fetch_add(1, Ordering::SeqCst);
        respond(assistant_text("done"))
    });
    let result = run_agent_loop(
        vec![user("prompt")],
        AgentContext::new(vec![], vec![]),
        config,
        event_sink(|_| {}),
        None,
        Some(stream_fn),
    )
    .await;
    assert_eq!(result.unwrap_err().to_string(), "router unavailable");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn uses_prepare_next_turn_snapshot_before_continuing() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let prepared = Arc::new(AtomicBool::new(false));
    let mut config = identity_config();
    let (p, done) = (prepare_calls.clone(), prepared.clone());
    config.prepare_next_turn = Some(hooks::prepare_next_turn(move |turn| {
        p.fetch_add(1, Ordering::SeqCst);
        let first = !done.swap(true, Ordering::SeqCst);
        async move {
            first.then(|| AgentLoopTurnUpdate {
                context: Some(AgentContext {
                    messages: turn.context.messages.clone(),
                    tools: turn.context.tools.clone(),
                }),
                messages: Some(vec![AgentMessage::System(SystemMessage {
                    content: SystemContent::Text("updated guidance".into()),
                    timestamp: 1,
                    ..Default::default()
                })]),
                ..Default::default()
            })
        }
    }));
    let has_update = Arc::new(AtomicBool::new(false));
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let (h, c) = (has_update.clone(), llm_calls.clone());
    let stream_fn = mock_stream_fn(move |_, context, _| {
        let n = c.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 2 {
            h.store(
                context.messages.iter().any(|m| {
                    matches!(m, Message::System(s) if s.content == SystemContent::Text("updated guidance".into()))
                }),
                Ordering::SeqCst,
            );
        }
        if n == 1 {
            respond(assistant_tool_calls(
                vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
                StopReason::ToolUse,
            ))
        } else {
            respond(assistant_text("done"))
        }
    });
    let _ = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![echo_tool(executed)]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(llm_calls.load(Ordering::SeqCst), 2);
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 1);
    assert!(has_update.load(Ordering::SeqCst));
}

#[tokio::test]
async fn picks_up_steering_queued_during_prepare_next_turn() {
    let queued = Arc::new(Mutex::new(Vec::<AgentMessage>::new()));
    let late = user("late steering");
    let mut config = identity_config();
    let (q, l) = (queued.clone(), late.clone());
    config.prepare_next_turn = Some(hooks::prepare_next_turn(move |_| {
        q.lock().push(l.clone());
        async { None }
    }));
    let q = queued.clone();
    config.get_steering_messages = Some(hooks::get_messages(move || {
        let drained: Vec<AgentMessage> = q.lock().drain(..).collect();
        async move { drained }
    }));
    let included = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (inc, c, late_message) = (included.clone(), calls.clone(), late.to_message().unwrap());
    let stream_fn = mock_stream_fn(move |_, context, _| {
        let n = c.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 2 {
            inc.store(context.messages.contains(&late_message), Ordering::SeqCst);
        }
        if n == 1 {
            respond(assistant_tool_calls(
                vec![tool_call("tool-1", "noop", json!({}))],
                StopReason::ToolUse,
            ))
        } else {
            respond(assistant_text("done"))
        }
    });
    let _ = drain(agent_loop(
        vec![user("run")],
        AgentContext::new(vec![], vec![noop_tool("noop")]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(included.load(Ordering::SeqCst));
}

#[tokio::test]
async fn action_end_receives_finalized_turn_context_and_stops_before_queue_polling() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let follow_up_polls = Arc::new(AtomicUsize::new(0));
    let callback_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let callback_roles = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut config = identity_config();
    let (ids, rs) = (callback_ids.clone(), callback_roles.clone());
    config.finish_turn = Some(hooks::finish_turn(move |turn, _| {
        *ids.lock() = turn
            .tool_results
            .iter()
            .map(|r| r.tool_call_id.clone())
            .collect();
        *rs.lock() = roles(&turn.context.messages);
        async { Some(AgentTurnDecision::End) }
    }));
    let s = steering_polls.clone();
    config.get_steering_messages = Some(hooks::get_messages(move || {
        s.fetch_add(1, Ordering::SeqCst);
        async { vec![] }
    }));
    let f = follow_up_polls.clone();
    config.get_follow_up_messages = Some(hooks::get_messages(move || {
        f.fetch_add(1, Ordering::SeqCst);
        async { vec![user("follow up should stay queued")] }
    }));
    let (stream_fn, calls) = scripted_stream_fn(|index| {
        if index == 0 {
            assistant_tool_calls(
                vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
                StopReason::ToolUse,
            )
        } else {
            assistant_text("should not run")
        }
    });
    let (events, messages) = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![echo_tool(executed.clone())]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 1);
    assert_eq!(*executed.lock(), vec!["hello"]);
    assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
    assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
    assert_eq!(*callback_ids.lock(), vec!["tool-1"]);
    assert_eq!(
        *callback_roles.lock(),
        vec!["system", "user", "assistant", "toolResult"]
    );
    assert_eq!(
        roles(&messages),
        vec!["system", "user", "assistant", "toolResult"]
    );
    assert_eq!(
        types(&events),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn stops_after_a_tool_batch_when_every_result_terminates() {
    let (stream_fn, calls) = scripted_stream_fn(|_| {
        assistant_tool_calls(
            vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
            StopReason::ToolUse,
        )
    });
    let (events, messages) = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![terminating_echo()]),
        identity_config(),
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 1);
    assert_eq!(
        roles(&messages),
        vec!["system", "user", "assistant", "toolResult"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn stops_after_a_blocked_tool_call_when_before_tool_call_terminates() {
    let executed = Arc::new(AtomicBool::new(false));
    let e = executed.clone();
    let tool = FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        move |_, _, _, _| {
            e.store(true, Ordering::SeqCst);
            async {
                Ok(text_result(
                    "should not execute",
                    json!({ "value": "unexpected" }),
                ))
            }
        },
    )
    .into_ref();
    let mut config = identity_config();
    config.before_tool_call = Some(hooks::before_tool_call(|_, _| async {
        Ok(Some(BeforeToolCallResult {
            block: true,
            reason: Some("Blocked by policy".into()),
            terminate: Some(true),
            ..Default::default()
        }))
    }));
    let (stream_fn, calls) = scripted_stream_fn(|index| {
        if index == 0 {
            assistant_tool_calls(
                vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
                StopReason::ToolUse,
            )
        } else {
            assistant_text("should not run")
        }
    });
    let (_, messages) = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    let result = &tool_results(&messages)[0];
    assert!(!executed.load(Ordering::SeqCst));
    assert_eq!(*calls.lock(), 1);
    assert!(result.is_error);
    assert!(
        result
            .content
            .contains(&UserContentBlock::Text(TextContent::new(
                "Blocked by policy"
            )))
    );
}

#[tokio::test]
async fn continues_after_a_mixed_batch_with_one_terminating_blocked_call() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let mut config = identity_config();
    config.tool_execution = Some(ToolExecutionMode::Parallel);
    config.before_tool_call = Some(hooks::before_tool_call(|ctx, _| async move {
        Ok(
            (ctx.args["value"] == "first").then(|| BeforeToolCallResult {
                block: true,
                reason: Some("Blocked first".into()),
                terminate: Some(true),
                ..Default::default()
            }),
        )
    }));
    let (stream_fn, calls) = scripted_stream_fn(tool_then_done(vec![
        tool_call("tool-1", "echo", json!({"value": "first"})),
        tool_call("tool-2", "echo", json!({"value": "second"})),
    ]));
    let _ = drain(agent_loop(
        vec![user("echo both")],
        AgentContext::new(vec![], vec![echo_tool(executed.clone())]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*executed.lock(), vec!["second"]);
    assert_eq!(*calls.lock(), 2);
}

#[tokio::test]
async fn continues_after_parallel_tool_calls_when_not_all_results_terminate() {
    let tool = FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        |_, params, _, _| async move {
            let value = params["value"].as_str().unwrap().to_string();
            let mut result = text_result(format!("echoed: {value}"), json!({ "value": value }));
            result.terminate = Some(value == "first");
            Ok(result)
        },
    )
    .into_ref();
    let mut config = identity_config();
    config.tool_execution = Some(ToolExecutionMode::Parallel);
    let (stream_fn, calls) = scripted_stream_fn(tool_then_done(vec![
        tool_call("tool-1", "echo", json!({"value": "first"})),
        tool_call("tool-2", "echo", json!({"value": "second"})),
    ]));
    let (_, messages) = drain(agent_loop(
        vec![user("echo both")],
        AgentContext::new(vec![], vec![tool]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 2);
    assert_eq!(
        roles(&messages),
        vec![
            "system",
            "user",
            "assistant",
            "toolResult",
            "toolResult",
            "assistant"
        ]
    );
}

#[tokio::test]
async fn allows_after_tool_call_to_mark_a_batch_as_terminating() {
    let mut config = identity_config();
    config.after_tool_call = Some(hooks::after_tool_call(|_, _| async {
        Ok(Some(AfterToolCallResult {
            terminate: Some(true),
            ..Default::default()
        }))
    }));
    let (stream_fn, calls) = scripted_stream_fn(|_| {
        assistant_tool_calls(
            vec![tool_call("tool-1", "echo", json!({"value": "hello"}))],
            StopReason::ToolUse,
        )
    });
    let _ = drain(agent_loop(
        vec![user("echo something")],
        AgentContext::new(vec![], vec![echo_tool(Arc::new(Mutex::new(Vec::new())))]),
        config,
        None,
        Some(stream_fn),
    ))
    .await;
    assert_eq!(*calls.lock(), 1);
}

// ---------------------------------------------------------------------------
// agent_loop_continue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn continue_fails_when_context_has_no_messages() {
    let error = agent_loop_continue(
        AgentContext::new(vec![], vec![]),
        identity_config(),
        None,
        None,
    )
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "Cannot continue: no messages in context");
}

#[tokio::test]
async fn continue_fails_from_an_assistant_tail() {
    let error = agent_loop_continue(
        AgentContext::new(vec![user("hi"), assistant_text("hello").into()], vec![]),
        identity_config(),
        None,
        None,
    )
    .err()
    .unwrap();
    assert_eq!(
        error.to_string(),
        "Cannot continue from message role: assistant"
    );
}

#[tokio::test]
async fn continues_from_existing_context_without_emitting_user_message_events() {
    let (stream_fn, _) = scripted_stream_fn(|_| assistant_text("Response"));
    let stream = agent_loop_continue(
        AgentContext::new(vec![user("Hello")], vec![]),
        identity_config(),
        None,
        Some(stream_fn),
    )
    .unwrap();
    let (events, messages) = drain(stream).await;
    assert_eq!(roles(&messages), vec!["assistant"]);
    let ends: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageEnd { .. }))
        .collect();
    assert_eq!(ends.len(), 1);
    if let AgentEvent::MessageEnd { message } = ends[0] {
        assert_eq!(message.role(), "assistant");
    }
}

#[tokio::test]
async fn allows_custom_message_types_as_last_message() {
    let mut fields = Map::new();
    fields.insert("text".into(), json!("Hook content"));
    fields.insert("timestamp".into(), json!(5));
    let custom = AgentMessage::Custom(CustomMessage::new("custom", fields));
    let mut config = identity_config();
    config.convert_to_llm = hooks::convert_to_llm(|messages| {
        let converted: Vec<Message> = messages
            .into_iter()
            .filter_map(|m| match m {
                AgentMessage::Custom(c) if c.role == "custom" => Some(Message::User(UserMessage {
                    content: UserContent::Text(
                        c.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    ),
                    timestamp: c.timestamp(),
                    ..Default::default()
                })),
                other => other.to_message(),
            })
            .filter(|m| {
                matches!(
                    m,
                    Message::User(_) | Message::Assistant(_) | Message::ToolResult(_)
                )
            })
            .collect();
        async move { converted }
    });
    let (stream_fn, _) = scripted_stream_fn(|_| assistant_text("Response to custom message"));
    let stream = agent_loop_continue(
        AgentContext::new(vec![custom], vec![]),
        config,
        None,
        Some(stream_fn),
    )
    .unwrap();
    let (_, messages) = drain(stream).await;
    assert_eq!(roles(&messages), vec!["assistant"]);
}

// ---------------------------------------------------------------------------
// run_tool_call
// ---------------------------------------------------------------------------

fn run_tool_call_echo() -> pi_agent::AgentToolRef {
    FnAgentTool::new(
        "echo",
        "Echo",
        "Echo tool",
        value_schema(),
        |_, params, _, on_update| async move {
            on_update(AgentToolResult {
                content: vec![UserContentBlock::text("partial")],
                details: Some(json!({})),
                ..Default::default()
            });
            let value = params["value"].as_str().unwrap().to_string();
            Ok(AgentToolResult {
                content: vec![UserContentBlock::text(value.clone())],
                details: Some(json!({})),
                structured_content: Some(json!({ "value": value })),
                ..Default::default()
            })
        },
    )
    .with_output_schema(value_schema())
    .into_ref()
}

fn failing_tool() -> pi_agent::AgentToolRef {
    FnAgentTool::new(
        "failing",
        "Failing",
        "Returns an error result",
        empty_schema(),
        |_, _, _, _| async {
            Ok(AgentToolResult {
                content: vec![UserContentBlock::text("bad")],
                details: Some(json!({ "partial": true })),
                is_error: Some(true),
                ..Default::default()
            })
        },
    )
    .into_ref()
}

#[tokio::test]
async fn run_tool_call_validates_runs_hooks_and_reports_failures() {
    let hook_calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let updates = Arc::new(Mutex::new(Vec::<AgentToolResult>::new()));
    let mut options = RunToolCallOptions::new(
        vec![run_tool_call_echo(), failing_tool()],
        assistant(vec![], StopReason::Stop),
        AgentContext::default(),
    );
    let h = hook_calls.clone();
    options.before_tool_call = Some(hooks::before_tool_call(move |ctx, _| {
        h.lock().push(format!("before {}", ctx.tool_call.id));
        async move {
            Ok(
                (ctx.args["value"] == "blocked").then(|| BeforeToolCallResult {
                    block: true,
                    reason: Some("nope".into()),
                    ..Default::default()
                }),
            )
        }
    }));
    let h = hook_calls.clone();
    options.after_tool_call = Some(hooks::after_tool_call(move |ctx, _| {
        h.lock().push(format!("after {}", ctx.tool_call.id));
        async { Ok(None) }
    }));
    let u = updates.clone();
    options.on_update = Some(hooks::tool_update_sink(move |partial| {
        u.lock().push(partial);
        async {}
    }));

    let a = run_tool_call(
        tool_call("a", "echo", json!({"value": "a"})),
        options.clone(),
    )
    .await;
    assert_eq!(a.tool_call.id, "a");
    assert_eq!(a.result.structured_content, Some(json!({ "value": "a" })));
    assert!(!a.is_error);

    let b = run_tool_call(
        tool_call("b", "echo", json!({"value": {"nested": true}})),
        options.clone(),
    )
    .await;
    assert!(b.is_error);

    let c = run_tool_call(
        tool_call("c", "echo", json!({"value": "blocked"})),
        options.clone(),
    )
    .await;
    assert_eq!(c.result.content, vec![UserContentBlock::text("nope")]);
    assert!(c.is_error);

    let d = run_tool_call(tool_call("d", "missing", json!({})), options.clone()).await;
    assert_eq!(
        d.result.content,
        vec![UserContentBlock::text("Tool missing not found")]
    );
    assert!(d.is_error);

    let e = run_tool_call(tool_call("e", "failing", json!({})), options.clone()).await;
    assert_eq!(e.result.details, Some(json!({ "partial": true })));
    assert!(e.is_error);

    assert_eq!(
        *updates.lock(),
        vec![AgentToolResult {
            content: vec![UserContentBlock::text("partial")],
            details: Some(json!({})),
            ..Default::default()
        }]
    );
    assert_eq!(
        *hook_calls.lock(),
        vec!["before a", "after a", "before c", "before e", "after e"]
    );
}

#[tokio::test]
async fn after_tool_call_replaces_structured_content_and_drops_it_with_content_only() {
    let redacted = vec![UserContentBlock::text("redacted")];
    let cases = vec![
        AfterToolCallResult {
            content: Some(redacted.clone()),
            ..Default::default()
        },
        AfterToolCallResult {
            structured_content: Some(json!({ "value": "replaced" })),
            ..Default::default()
        },
        AfterToolCallResult {
            content: Some(redacted.clone()),
            structured_content: Some(json!({ "value": "both" })),
            ..Default::default()
        },
        AfterToolCallResult {
            details: Some(json!({ "note": "kept" })),
            ..Default::default()
        },
    ];
    let mut seen = Vec::new();
    for after_result in cases {
        let mut options = RunToolCallOptions::new(
            vec![run_tool_call_echo()],
            assistant(vec![], StopReason::Stop),
            AgentContext::default(),
        );
        options.after_tool_call = Some(hooks::after_tool_call(move |_, _| {
            let after_result = after_result.clone();
            async move { Ok(Some(after_result)) }
        }));
        let outcome = run_tool_call(
            tool_call("x", "echo", json!({"value": "original"})),
            options,
        )
        .await;
        seen.push(outcome.result.structured_content);
    }
    assert_eq!(
        seen,
        vec![
            None,
            Some(json!({ "value": "replaced" })),
            Some(json!({ "value": "both" })),
            Some(json!({ "value": "original" }))
        ]
    );
}
