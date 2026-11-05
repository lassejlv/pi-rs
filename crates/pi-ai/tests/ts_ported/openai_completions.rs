//! Ports of the OpenAI Chat Completions tests: `openai-completions-*.test.ts` and
//! `azure-openai-completions.test.ts`.
//!
//! The TypeScript tests mock the OpenAI SDK; here a local server stands in for the endpoint. Base
//! URLs keep their original host as a path prefix (`http://127.0.0.1:N/api.openai.com/v1`) so the
//! substring-based endpoint detection sees the same URL text as in TypeScript.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use pi_ai::api::openai_completions;
use pi_ai::providers::all::get_builtin_model;
use pi_ai::*;
use serde_json::{Value, json};

use crate::mock_http::{MockServer, Request, Response};
use crate::support::*;

fn builtin(provider: &str, id: &str) -> Model {
    get_builtin_model(provider, id).unwrap_or_else(|| panic!("{provider}/{id}"))
}

/// `{ ...getModel(provider, id) without compat, api: "openai-completions" }`.
fn as_completions(provider: &str, id: &str) -> Model {
    let mut model = builtin(provider, id);
    model.compat = None;
    model.api = "openai-completions".into();
    model
}

fn local_model(id: &str, compat: Option<Value>) -> Model {
    let mut value = json!({
        "id": id, "name": id, "api": "openai-completions", "provider": "local-vllm",
        "baseUrl": "http://localhost:8000/v1", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 8192
    });
    if let Some(compat) = compat {
        value["compat"] = compat;
    }
    model(value)
}

fn rehost(base_url: &str, server: &str) -> String {
    let stripped = base_url.split_once("://").map(|(_, rest)| rest).unwrap_or(base_url);
    format!("{server}/{}", stripped.trim_end_matches('/'))
}

fn default_chunks() -> Vec<Value> {
    vec![json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}}
    })]
}

struct Run {
    request: Request,
    payload: Value,
    message: AssistantMessage,
    events: Vec<AssistantMessageEvent>,
}

#[derive(Clone, Copy, PartialEq)]
enum Entry {
    Stream,
    Simple,
}

async fn run(entry: Entry, model: &Model, context: &Context, options: StreamOptions, chunks: Option<Vec<Value>>) -> Run {
    let body = openai_sse(&chunks.unwrap_or_else(default_chunks));
    let server = MockServer::start(move |_, _| Response::sse(&body)).await;
    let mut model = model.clone();
    model.base_url = rehost(&model.base_url, &server.url);
    let options = StreamOptions { api_key: options.api_key.clone().or(Some("test".into())), ..options };
    let stream = match entry {
        Entry::Stream => compat::stream(&model, context, options),
        Entry::Simple => compat::stream_simple(&model, context, options),
    };
    let (events, message) = stream.collect_all().await;
    let request = server.requests().first().cloned().unwrap_or_else(|| panic!("no request: {:?}", message.error_message));
    let payload = request.json();
    Run { request, payload, message, events }
}

async fn simple(model: &Model, context: &Context, options: StreamOptions) -> Run {
    run(Entry::Simple, model, context, options, None).await
}

async fn simple_chunks(model: &Model, context: &Context, chunks: Vec<Value>) -> Run {
    run(Entry::Simple, model, context, StreamOptions::default(), Some(chunks)).await
}

fn reasoning(level: ThinkingLevel) -> StreamOptions {
    StreamOptions { reasoning: Some(level), ..Default::default() }
}

fn hi() -> Context {
    user_ctx("Hi")
}

fn ping_tool() -> Value {
    json!({"name": "ping", "description": "Ping tool", "parameters": {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]}})
}

fn read_tool() -> Value {
    json!({"name": "read", "description": "Read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}})
}

fn with_tools(text: &str, tools: Vec<Value>) -> Context {
    ctx(json!({"messages": [{"role": "user", "content": text}], "tools": tools}))
}

fn usage(prompt: u64, completion: u64) -> Value {
    json!({"prompt_tokens": prompt, "completion_tokens": completion, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}})
}

fn completions_assistant(provider: &str, model_id: &str, content: Value, stop: &str) -> Value {
    json!({"role": "assistant", "api": "openai-completions", "provider": provider, "model": model_id, "content": content, "stopReason": stop})
}

fn tool_result(id: &str, name: &str, text: &str) -> Value {
    json!({"role": "toolResult", "toolCallId": id, "toolName": name, "content": [{"type": "text", "text": text}], "isError": false})
}

fn content_json(message: &AssistantMessage) -> Value {
    serde_json::to_value(&message.content).unwrap()
}

// ---------------------------------------------------------------------------
// Port of openai-completions-tool-choice.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwards_tool_choice_and_includes_it_without_tools() {
    let model = as_completions("openai", "gpt-4o-mini");
    let options = StreamOptions::default().with_extra("toolChoice", "required");
    let run = simple(&model, &with_tools("Call ping with ok=true", vec![ping_tool()]), options).await;
    assert_eq!(run.payload["tool_choice"], "required");
    assert!(!run.payload["tools"].as_array().unwrap().is_empty());

    let options = StreamOptions { tool_choice: Some(ToolChoice::None), ..Default::default() };
    let run = simple(&model, &user_ctx("Summarize the conversation"), options).await;
    assert_eq!(run.payload["tool_choice"], "none");
    assert!(run.payload.get("tools").is_none());
}

#[tokio::test]
async fn strict_mode_follows_compat_and_endpoint_capability() {
    let mut model = as_completions("openai", "gpt-4o-mini");
    model.compat = Some(serde_json::from_value(json!({"supportsStrictMode": false})).unwrap());
    let run = simple(&model, &with_tools("Call ping with ok=true", vec![ping_tool()]), StreamOptions::default()).await;
    let function = &run.payload["tools"][0]["function"];
    assert!(function.is_object());
    assert!(function.get("strict").is_none());

    // Regression test for #9816: unknown endpoints default to non-strict tools.
    let prefer = json!({"name": "ping", "description": "Ping tool", "constrainedSampling": {"type": "json_schema", "strict": "prefer"},
        "parameters": {"type": "object", "properties": {"required": {"type": "string"}, "optional": {"type": "string"}}, "required": ["required"]}});
    let run = simple(&local_model("local-model", None), &with_tools("Call ping", vec![prefer.clone()]), StreamOptions::default()).await;
    let function = &run.payload["tools"][0]["function"];
    assert!(function.get("strict").is_none());
    assert_eq!(function["parameters"]["required"], json!(["required"]));

    let groq = builtin("groq", "openai/gpt-oss-20b");
    assert_eq!(groq.compat().supports_strict_mode, Some(true));
    let run = simple(&groq, &with_tools("Call ping", vec![prefer]), StreamOptions::default()).await;
    let function = &run.payload["tools"][0]["function"];
    assert_eq!(function["strict"], true);
    assert_eq!(function["parameters"]["required"], json!(["required", "optional"]));
}

#[tokio::test]
async fn groq_reasoning_effort_mapping() {
    let run = simple(&builtin("groq", "qwen/qwen3.6-27b"), &hi(), reasoning(ThinkingLevel::Medium)).await;
    assert_eq!(run.payload["reasoning_effort"], "default");
    let run = simple(&builtin("groq", "openai/gpt-oss-20b"), &hi(), reasoning(ThinkingLevel::Medium)).await;
    assert_eq!(run.payload["reasoning_effort"], "medium");
}

#[tokio::test]
async fn zai_tool_stream_and_thinking() {
    let glm = builtin("zai", "glm-5.2");
    let run = simple(&glm, &with_tools("Call ping with ok=true", vec![ping_tool()]), StreamOptions::default()).await;
    assert_eq!(run.payload["tool_stream"], true);

    let mut overridden = glm.clone();
    let mut compat = overridden.compat();
    compat.zai_tool_stream = Some(true);
    overridden.compat = Some(Box::new(compat));
    let run = simple(&overridden, &with_tools("Call ping with ok=true", vec![ping_tool()]), StreamOptions::default()).await;
    assert_eq!(run.payload["tool_stream"], true);

    let run = simple(&glm, &hi(), StreamOptions::default()).await;
    assert!(run.payload.get("tool_stream").is_none());
    assert_eq!(run.payload["thinking"], json!({"type": "disabled"}));
    assert!(run.payload.get("reasoning_effort").is_none());

    for (level, effort) in [
        (ThinkingLevel::Low, "high"),
        (ThinkingLevel::Medium, "high"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Max, "max"),
    ] {
        let run = simple(&glm, &hi(), reasoning(level)).await;
        assert_eq!(run.payload["thinking"], json!({"type": "enabled", "clear_thinking": false}));
        assert_eq!(run.payload["reasoning_effort"], effort, "{level:?}");
    }

    let context = ctx(json!({"messages": [
        {"role": "user", "content": "Read README.md"},
        completions_assistant("zai", "glm-5.2", json!([
            {"type": "thinking", "thinking": "prior reasoning", "thinkingSignature": "reasoning_content"},
            {"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}}
        ]), "toolUse"),
        tool_result("call_1", "read", "contents"),
        {"role": "user", "content": "Continue"}
    ]}));
    let run = simple(&glm, &context, reasoning(ThinkingLevel::High)).await;
    let assistant = run.payload["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone();
    assert_eq!(assistant["reasoning_content"], "prior reasoning");
    assert_eq!(run.payload["thinking"], json!({"type": "enabled", "clear_thinking": false}));
}

#[test]
fn zai_catalog_metadata() {
    for id in ["glm-4.7", "glm-5-turbo", "glm-5.2"] {
        assert_eq!(builtin("zai", id).compat().zai_tool_stream, Some(true), "{id}");
    }
    for id in ["glm-5.2", "glm-5.2-highspeed"] {
        let model = builtin("zai", id);
        assert_eq!(model.compat().supports_reasoning_effort, Some(true));
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).unwrap(),
            json!({"off": "none", "minimal": null, "low": null, "medium": null, "high": "high", "xhigh": null, "max": "max"})
        );
    }
    for provider in ["zai", "zai-coding-cn"] {
        let model = builtin(provider, "glm-5.3");
        assert_eq!(model.compat().supports_reasoning_effort, Some(true));
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).unwrap(),
            json!({"off": null, "minimal": null, "low": "low", "medium": null, "high": "high", "xhigh": null, "max": "max"})
        );
    }
}

#[tokio::test]
async fn finish_reasons_and_null_chunks() {
    let glm = builtin("zai", "glm-5.2");
    let run = simple_chunks(&glm, &hi(), vec![
        json!({"choices": [{"delta": {"content": "partial"}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "network_error"}], "usage": usage(1, 1)}),
    ])
    .await;
    assert_eq!(run.message.stop_reason, StopReason::Error);
    assert_eq!(run.message.error_message.as_deref(), Some("Provider finish_reason: network_error"));

    let mini = as_completions("openai", "gpt-4o-mini");
    let run = simple_chunks(&mini, &user_ctx("Reply with exactly OK"), vec![
        Value::Null,
        json!({"id": "chatcmpl-test", "choices": [{"delta": {"content": "OK"}, "finish_reason": null}]}),
        json!({"id": "chatcmpl-test", "choices": [{"delta": {}, "finish_reason": "stop"}], "usage": usage(3, 1)}),
    ])
    .await;
    assert_eq!(run.message.stop_reason, StopReason::Stop);
    assert_eq!(run.message.error_message, None);
    assert_eq!(run.message.response_id.as_deref(), Some("chatcmpl-test"));
    assert_eq!(run.message.usage.total_tokens, 4);
    assert_eq!(content_json(&run.message), json!([{"type": "text", "text": "OK"}]));

    let truncated = json!({"id": "chatcmpl-truncated", "choices": [{"delta": {"content": "partial answer"}, "finish_reason": null}]});
    let run = simple_chunks(&mini, &user_ctx("Reply with a longer sentence"), vec![truncated.clone(), truncated]).await;
    assert_eq!(run.message.stop_reason, StopReason::Error);
    assert_eq!(run.message.error_message.as_deref(), Some("Stream ended without finish_reason"));

    let mut lenient = mini.clone();
    lenient.compat = Some(serde_json::from_value(json!({"supportsFinishReason": false})).unwrap());
    let run = simple_chunks(&lenient, &user_ctx("Reply with a complete answer"), vec![
        json!({"id": "chatcmpl-no-finish-reason", "choices": [{"delta": {"content": "complete answer"}, "finish_reason": null}]}),
    ])
    .await;
    assert_eq!(run.message.stop_reason, StopReason::Stop);
    assert_eq!(run.message.error_message, None);
    assert_eq!(content_json(&run.message), json!([{"type": "text", "text": "complete answer"}]));
}

#[tokio::test]
async fn ignores_empty_custom_objects_on_function_tool_call_deltas() {
    let run = simple_chunks(
        &as_completions("openai", "gpt-4o-mini"),
        &with_tools("Read README.md", vec![read_tool()]),
        vec![json!({"id": "chatcmpl-empty-custom", "choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}, "custom": {}}
        ]}, "finish_reason": "tool_calls"}]})],
    )
    .await;
    assert_eq!(
        content_json(&run.message),
        json!([{"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}}])
    );
}

fn tool_event_index(event: &AssistantMessageEvent) -> Option<usize> {
    match event {
        AssistantMessageEvent::ToolCallStart { content_index, .. }
        | AssistantMessageEvent::ToolCallDelta { content_index, .. }
        | AssistantMessageEvent::ToolCallEnd { content_index, .. } => Some(*content_index),
        _ => None,
    }
}

#[tokio::test]
async fn coalesces_tool_call_deltas_by_stable_index() {
    let delta = |id: &str, name: Value, args: &str| json!({"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": args}}]});
    let run = simple_chunks(
        &as_completions("openai", "gpt-4o-mini"),
        &with_tools("Read README.md", vec![read_tool()]),
        vec![
            json!({"id": "c", "choices": [{"delta": delta("functions.read:0", json!("read"), ""), "finish_reason": null}]}),
            json!({"id": "c", "choices": [{"delta": delta("chatcmpl-tool-a", Value::Null, "{\"path\":\"README"), "finish_reason": null}]}),
            json!({"id": "c", "choices": [{"delta": delta("chatcmpl-tool-b", Value::Null, ".md\"}"), "finish_reason": "tool_calls"}], "usage": usage(10, 5)}),
        ],
    )
    .await;
    assert_eq!(run.message.stop_reason, StopReason::ToolUse);
    let indexes: Vec<usize> = run.events.iter().filter_map(tool_event_index).collect();
    assert_eq!(indexes, vec![0, 0, 0, 0, 0]);
    assert_eq!(run.message.content.len(), 1);
    let call = serde_json::to_value(&run.message.content[0]).unwrap();
    assert_eq!(call, json!({"type": "toolCall", "id": "functions.read:0", "name": "read", "arguments": {"path": "README.md"}}));
}

#[tokio::test]
async fn accumulates_mixed_content_reasoning_and_parallel_tool_calls() {
    let tool = |name: &str, props: &[&str]| {
        let properties: serde_json::Map<String, Value> = props.iter().map(|p| (p.to_string(), json!({"type": "string"}))).collect();
        json!({"name": name, "description": name, "parameters": {"type": "object", "properties": properties, "required": props}})
    };
    let tools = vec![tool("read", &["path"]), tool("grep", &["pattern", "path"]), tool("list", &["path"]), tool("write", &["path", "content"])];
    let call = |index: Option<u64>, id: Option<&str>, name: Option<&str>, args: &str| {
        let mut value = json!({"type": "function", "function": {"arguments": args}});
        if let Some(index) = index {
            value["index"] = json!(index);
        }
        if let Some(id) = id {
            value["id"] = json!(id);
        }
        if let Some(name) = name {
            value["function"]["name"] = json!(name);
        }
        value
    };
    let chunks = vec![
        json!({"id": "m", "choices": [{"delta": {"content": "answer 1", "reasoning_content": "think 1", "tool_calls": [
            call(Some(0), Some("tc_read_initial"), Some("read"), "{\"path\":\"README"),
            call(Some(1), Some("tc_grep_initial"), Some("grep"), "{\"pattern\":\"TODO"),
            call(None, Some("tc_list_no_index"), Some("list"), "{\"path\":\"packages"),
            call(None, Some("tc_write_no_index"), Some("write"), "{\"path\":\"out"),
        ]}, "finish_reason": null}]}),
        json!({"id": "m", "choices": [{"delta": {"content": " answer 2", "tool_calls": [
            call(Some(1), Some("tc_grep_changed"), None, "\",\"path\":\"src"),
            call(None, Some("tc_write_no_index"), None, ".txt\",\"content\":\"ok\"}"),
            call(None, Some("tc_list_no_index"), None, "/ai\"}"),
        ]}, "finish_reason": null}]}),
        json!({"id": "m", "choices": [{"delta": {"content": "\n", "reasoning_content": " think 2", "tool_calls": [
            call(Some(0), Some("tc_read_changed"), None, ".md\"}"),
            call(Some(1), None, None, "\"}"),
        ]}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 8, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 2}}}),
    ];
    let context = ctx(json!({"messages": [{"role": "user", "content": "Think, answer, and use tools."}], "tools": tools}));
    let run = simple_chunks(&as_completions("openai", "gpt-4o-mini"), &context, chunks).await;
    assert_eq!(run.message.stop_reason, StopReason::ToolUse, "{:?}", run.message.error_message);
    let types: Vec<&str> = run.events.iter().map(|e| e.type_name()).collect();
    let count = |t: &str| types.iter().filter(|x| **x == t).count();
    assert_eq!(
        [count("text_start"), count("text_delta"), count("text_end"), count("thinking_start"), count("thinking_delta"), count("thinking_end")],
        [1, 3, 1, 1, 2, 1]
    );
    assert_eq!([count("toolcall_start"), count("toolcall_delta"), count("toolcall_end")], [4, 9, 4]);
    let mut by_index: HashMap<usize, Vec<&str>> = HashMap::new();
    for event in &run.events {
        if let Some(index) = tool_event_index(event) {
            by_index.entry(index).or_default().push(event.type_name());
        }
    }
    let seq = |deltas: usize| {
        let mut v = vec!["toolcall_start"];
        v.extend(std::iter::repeat_n("toolcall_delta", deltas));
        v.push("toolcall_end");
        v
    };
    assert_eq!(by_index[&2], seq(2));
    assert_eq!(by_index[&3], seq(3));
    assert_eq!(by_index[&4], seq(2));
    assert_eq!(by_index[&5], seq(2));
    assert_eq!(
        content_json(&run.message),
        json!([
            {"type": "thinking", "thinking": "think 1 think 2", "thinkingSignature": "reasoning_content"},
            {"type": "text", "text": "answer 1 answer 2\n"},
            {"type": "toolCall", "id": "tc_read_initial", "name": "read", "arguments": {"path": "README.md"}},
            {"type": "toolCall", "id": "tc_grep_initial", "name": "grep", "arguments": {"pattern": "TODO", "path": "src"}},
            {"type": "toolCall", "id": "tc_list_no_index", "name": "list", "arguments": {"path": "packages/ai"}},
            {"type": "toolCall", "id": "tc_write_no_index", "name": "write", "arguments": {"path": "out.txt", "content": "ok"}}
        ])
    );
}

#[tokio::test]
async fn instruction_roles_for_openrouter_and_openai() {
    let system = ctx(json!({"systemPrompt": "Follow instructions.", "messages": [{"role": "user", "content": "Hi"}]}));
    let run = simple(&builtin("openrouter", "deepseek/deepseek-v4-pro"), &system, StreamOptions::default()).await;
    assert_eq!(run.payload["messages"][0]["role"], "system");
    for model in [builtin("openrouter", "openai/gpt-5.2-codex"), builtin("openrouter", "anthropic/claude-fable-5.1:batch")] {
        let run = simple(&model, &system, StreamOptions::default()).await;
        assert_eq!(run.payload["messages"][0]["role"], "developer", "{}", model.id);
    }
    let run = simple(&as_completions("openai", "gpt-5.5"), &system, StreamOptions::default()).await;
    assert_eq!(run.payload["messages"][0]["role"], "developer");
}

#[test]
fn reasoning_replay_catalog_metadata() {
    let kimi = builtin("openrouter", "moonshotai/kimi-k2.6");
    assert_eq!(kimi.compat().supports_developer_role, Some(false));
    assert_eq!(kimi.compat().requires_reasoning_content_on_assistant_messages, Some(true));
    for provider in ["xiaomi", "xiaomi-token-plan-cn", "xiaomi-token-plan-ams", "xiaomi-token-plan-sgp"] {
        let compat = serde_json::to_value(builtin(provider, "mimo-v2.6-pro").compat()).unwrap();
        assert_eq!(compat["requiresReasoningContentOnAssistantMessages"], true, "{provider}");
        assert_eq!(compat["thinkingFormat"], "deepseek", "{provider}");
        assert!(compat.get("maxTokensField").is_none(), "{provider}");
        assert!(compat.get("supportsDeveloperRole").is_none(), "{provider}");
    }
    for provider in ["qwen-token-plan", "qwen-token-plan-cn", "qwen-token-plan-individual"] {
        let compat = serde_json::to_value(builtin(provider, "qwen3.7-max").compat()).unwrap();
        assert_eq!(compat["thinkingFormat"], "qwen", "{provider}");
        assert!(compat.get("requiresReasoningContentOnAssistantMessages").is_none(), "{provider}");
        assert_eq!(compat["supportsDeveloperRole"], false, "{provider}");
        assert_eq!(compat["supportsStore"], false, "{provider}");
    }
}

#[tokio::test]
async fn xiaomi_replays_tool_calls_with_empty_reasoning_content() {
    let context = ctx(json!({"messages": [
        {"role": "user", "content": "Read README.md"},
        completions_assistant("xiaomi", "mimo-v2.6-pro", json!([{"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}}]), "toolUse"),
        tool_result("call_1", "read", "contents")
    ]}));
    let run = simple(&builtin("xiaomi", "mimo-v2.6-pro"), &context, reasoning(ThinkingLevel::High)).await;
    let assistant = run.payload["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone();
    assert_eq!(assistant["reasoning_content"], "");
    assert_eq!(run.payload["thinking"], json!({"type": "enabled"}));
    assert!(run.payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn reasoning_delta_field_normalization() {
    let chunk = vec![json!({"id": "chatcmpl-r", "choices": [{"delta": {"reasoning": "think"}, "finish_reason": "stop"}]})];
    let run = simple_chunks(&as_completions("opencode-go", "kimi-k3"), &user_ctx("Use reasoning."), chunk.clone()).await;
    assert_eq!(content_json(&run.message), json!([{"type": "thinking", "thinking": "think", "thinkingSignature": "reasoning_content"}]));
    let run = simple_chunks(&as_completions("openai", "gpt-4o-mini"), &user_ctx("Use reasoning."), chunk).await;
    assert_eq!(content_json(&run.message), json!([{"type": "thinking", "thinking": "think", "thinkingSignature": "reasoning"}]));
}

#[test]
fn replays_opencode_go_reasoning_as_reasoning_content() {
    let model = as_completions("opencode-go", "kimi-k3");
    let context = normalize_context(&ctx(json!({"messages": [completions_assistant("opencode-go", "kimi-k3", json!([
        {"type": "thinking", "thinking": "think", "thinkingSignature": "reasoning"},
        {"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}}
    ]), "stop")]})));
    let mut compat = openai_completions::get_compat(&model);
    compat.supports_store = false;
    compat.supports_developer_role = false;
    compat.thinking_format = serde_json::from_value(json!("openai")).unwrap();
    compat.supports_strict_mode = true;
    let messages = openai_completions::convert_messages(&model, &context, &compat, &HashMap::new()).unwrap();
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["reasoning_content"], "think");
    assert!(messages[0].get("reasoning").is_none());
}

#[tokio::test]
async fn kimi_thinking_payloads() {
    let opencode = builtin("opencode", "kimi-k2.6");
    let run = simple(&opencode, &hi(), StreamOptions::default()).await;
    assert_eq!(run.payload["thinking"], json!({"type": "disabled"}));
    assert!(run.payload.get("reasoning_effort").is_none());
    let run = simple(&opencode, &hi(), reasoning(ThinkingLevel::High)).await;
    assert_eq!(run.payload["thinking"], json!({"type": "enabled"}));
    assert!(run.payload.get("reasoning_effort").is_none());
    for provider in ["moonshotai", "moonshotai-cn"] {
        let run = simple(&builtin(provider, "kimi-k2.7-code"), &hi(), StreamOptions::default()).await;
        assert!(run.payload.get("thinking").is_none(), "{provider}");
        assert!(run.payload.get("reasoning_effort").is_none(), "{provider}");
    }
    let run = simple(&builtin("moonshotai-cn", "kimi-k2.6"), &hi(), StreamOptions::default()).await;
    assert_eq!(run.payload["thinking"], json!({"type": "disabled"}));
    assert!(run.payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn max_tokens_field_for_opencode_deepseek_and_zai() {
    let max = StreamOptions { max_tokens: Some(123), ..Default::default() };
    let mut cases = vec![builtin("opencode-go", "kimi-k3"), builtin("opencode", "kimi-k2.6")];
    for model in &cases {
        assert_eq!(serde_json::to_value(model.compat()).unwrap()["maxTokensField"], "max_tokens", "{}", model.id);
    }
    let natives = [builtin("deepseek", "deepseek-flash"), builtin("deepseek", "deepseek-v4-pro")];
    for model in &natives {
        assert_eq!(serde_json::to_value(model.compat()).unwrap()["maxTokensField"], "max_tokens", "{}", model.id);
    }
    cases.extend(natives);
    let mut custom = local_model("custom-deepseek-model", None);
    custom.provider = "custom-deepseek".into();
    custom.base_url = "https://api.deepseek.com".into();
    let mut upper = custom.clone();
    upper.id = "custom-uppercase-deepseek-model".into();
    upper.base_url = "https://API.DeepSeek.COM".into();
    cases.extend([custom, upper]);
    for model in [builtin("zai", "glm-5-turbo"), builtin("zai", "glm-5.2")] {
        assert_eq!(serde_json::to_value(model.compat()).unwrap()["maxTokensField"], "max_tokens", "{}", model.id);
        cases.push(model);
    }
    for model in cases {
        let run = simple(&model, &hi(), max.clone()).await;
        assert_eq!(run.payload["max_tokens"], 123, "{}", model.id);
        assert!(run.payload.get("max_completion_tokens").is_none(), "{}", model.id);
    }
}

#[tokio::test]
async fn omits_reasoning_effort_for_opencode_grok_build() {
    let run = simple(&builtin("opencode", "grok-build-0.1"), &hi(), reasoning(ThinkingLevel::High)).await;
    assert!(run.payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn usage_accounting() {
    let mini = as_completions("openai", "gpt-4o-mini");
    let run = simple_chunks(&mini, &user_ctx("Use reasoning."), vec![json!({"id": "u", "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 33, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 21}}})])
    .await;
    assert_eq!((run.message.usage.input, run.message.usage.output, run.message.usage.total_tokens), (10, 33, 43));

    let cache_usage = json!({"prompt_tokens": 100, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 50, "cache_write_tokens": 30}, "completion_tokens_details": {"reasoning_tokens": 0}});
    let first = json!({"id": "cw", "choices": [{"delta": {"content": "OK"}, "finish_reason": null}]});
    for last in [
        json!({"id": "cw", "choices": [{"delta": {}, "finish_reason": "stop"}], "usage": cache_usage.clone()}),
        json!({"id": "cw", "choices": [{"delta": {}, "finish_reason": "stop", "usage": cache_usage.clone()}]}),
    ] {
        let run = simple_chunks(&mini, &user_ctx("Reply with exactly OK"), vec![first.clone(), last]).await;
        let usage = &run.message.usage;
        assert_eq!((usage.input, usage.cache_read, usage.cache_write, usage.total_tokens), (20, 50, 30, 105));
    }
}

#[tokio::test]
async fn openrouter_reasoning_object_and_chat_template_kwargs() {
    let run = simple(&builtin("openrouter", "deepseek/deepseek-r1-0528"), &hi(), reasoning(ThinkingLevel::High)).await;
    assert_eq!(run.payload["reasoning"], json!({"effort": "high"}));
    assert!(run.payload.get("reasoning_effort").is_none());

    let template = local_model("deepseek-ai/DeepSeek-V3.1", Some(json!({"thinkingFormat": "chat-template", "supportsReasoningEffort": false,
        "chatTemplateKwargs": {"thinking": {"$var": "thinking.enabled"}}})));
    for (level, expected) in [(Some(ThinkingLevel::High), true), (None, false)] {
        let run = simple(&template, &hi(), StreamOptions { reasoning: level, ..Default::default() }).await;
        assert_eq!(run.payload["chat_template_kwargs"], json!({"thinking": expected}));
        assert!(run.payload.get("thinking").is_none());
        assert!(run.payload.get("reasoning_effort").is_none());
    }

    let qwen = local_model("Qwen/Qwen3-Coder", Some(json!({"thinkingFormat": "qwen-chat-template", "supportsReasoningEffort": false})));
    for (level, expected) in [(Some(ThinkingLevel::High), true), (None, false)] {
        let run = simple(&qwen, &hi(), StreamOptions { reasoning: level, ..Default::default() }).await;
        assert_eq!(run.payload["chat_template_kwargs"], json!({"enable_thinking": expected, "preserve_thinking": true}));
        assert!(run.payload.get("reasoning_effort").is_none());
    }

    let mut effort = local_model("unsloth/gpt-oss-120b-GGUF", Some(json!({"thinkingFormat": "chat-template", "supportsReasoningEffort": false,
        "chatTemplateKwargs": {"preserve_thinking": true, "reasoning_effort": {"$var": "thinking.effort", "omitWhenOff": true}}})));
    effort.thinking_level_map = Some(serde_json::from_value(json!({"xhigh": "max"})).unwrap());
    let run = simple(&effort, &hi(), reasoning(ThinkingLevel::Xhigh)).await;
    assert_eq!(run.payload["chat_template_kwargs"], json!({"preserve_thinking": true, "reasoning_effort": "max"}));
    assert!(run.payload.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn ant_ling_compatibility() {
    let ring = builtin("ant-ling", "Ring-2.6-1T");
    let compat = serde_json::to_value(ring.compat()).unwrap();
    for (key, value) in [
        ("supportsStore", json!(false)),
        ("supportsDeveloperRole", json!(false)),
        ("supportsReasoningEffort", json!(false)),
        ("maxTokensField", json!("max_tokens")),
        ("thinkingFormat", json!("ant-ling")),
        ("supportsLongCacheRetention", json!(false)),
        ("supportsStrictMode", json!(true)),
    ] {
        assert_eq!(compat[key], value, "{key}");
    }
    assert!(compat.get("requiresReasoningContentOnAssistantMessages").is_none());
    let options = StreamOptions {
        max_tokens: Some(123),
        reasoning: Some(ThinkingLevel::High),
        cache_retention: Some(CacheRetention::Long),
        session_id: Some("ant-ling-session".into()),
        ..Default::default()
    };
    let system = ctx(json!({"systemPrompt": "Follow instructions.", "messages": [{"role": "user", "content": "Hi"}]}));
    let run = simple(&ring, &system, options).await;
    let p = &run.payload;
    assert_eq!(p["max_tokens"], 123);
    assert!(p.get("max_completion_tokens").is_none());
    assert_eq!(p["messages"][0]["role"], "system");
    assert_eq!(p["reasoning"], json!({"effort": "high"}));
    for key in ["reasoning_effort", "store", "prompt_cache_key", "prompt_cache_retention"] {
        assert!(p.get(key).is_none(), "{key}");
    }

    let run = run_stream_payload(&ring, &hi(), StreamOptions::default().with_extra("reasoningEffort", "medium")).await;
    assert!(run.get("reasoning").is_none());
    let run = simple(&builtin("ant-ling", "Ling-2.6-flash"), &hi(), reasoning(ThinkingLevel::High)).await;
    assert!(run.payload.get("reasoning").is_none());
}

async fn run_stream_payload(model: &Model, context: &Context, options: StreamOptions) -> Value {
    run(Entry::Stream, model, context, options, None).await.payload
}

// ---------------------------------------------------------------------------
// Port of openai-completions-cache-control-format.test.ts
// ---------------------------------------------------------------------------

fn custom_qwen() -> Model {
    model(json!({
        "id": "custom-qwen", "name": "Custom Qwen", "api": "openai-completions", "provider": "openrouter",
        "baseUrl": "https://example.com/v1", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 32000,
        "compat": {"cacheControlFormat": "anthropic"}
    }))
}

fn cache_context(messages: Option<Value>) -> Context {
    ctx(json!({"systemPrompt": "System prompt", "messages": messages.unwrap_or(json!([{"role": "user", "content": "Hello"}])), "tools": [read_tool()]}))
}

fn expect_anthropic_cache_markers(params: &Value) {
    let messages = params["messages"].as_array().unwrap();
    let instruction = messages.iter().find(|m| m["role"] == "system" || m["role"] == "developer").unwrap();
    assert_eq!(instruction["content"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(params["tools"].as_array().unwrap().len(), 1);
    assert_eq!(params["tools"][0]["cache_control"], json!({"type": "ephemeral"}));
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["cache_control"], json!({"type": "ephemeral"}));
}

#[tokio::test]
async fn cache_control_format_markers() {
    let run0 = run(Entry::Stream, &custom_qwen(), &cache_context(None), StreamOptions::default(), None).await;
    expect_anthropic_cache_markers(&run0.payload);

    let batch = builtin("openrouter", "anthropic/claude-fable-5.1:batch");
    let run1 = run(Entry::Stream, &batch, &cache_context(None), StreamOptions::default(), None).await;
    expect_anthropic_cache_markers(&run1.payload);

    let messages = json!([
        {"role": "user", "content": "Read the file"},
        completions_assistant("openrouter", &batch.id, json!([{"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}}]), "toolUse"),
        tool_result("call_1", "read", "file contents")
    ]);
    let run2 = run(Entry::Stream, &batch, &cache_context(Some(messages)), StreamOptions::default(), None).await;
    let all = run2.payload["messages"].as_array().unwrap();
    assert_eq!(all.iter().find(|m| m["role"] == "user").unwrap()["content"], "Read the file");
    let tool = all.last().unwrap();
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["content"][0]["cache_control"], json!({"type": "ephemeral"}));

    let none = StreamOptions { cache_retention: Some(CacheRetention::None), ..Default::default() };
    let run3 = run(Entry::Stream, &custom_qwen(), &cache_context(None), none, None).await;
    let all = run3.payload["messages"].as_array().unwrap();
    let instruction = all.iter().find(|m| m["role"] == "system" || m["role"] == "developer").unwrap();
    assert!(!instruction["content"].is_array());
    assert!(run3.payload["tools"][0].get("cache_control").is_none());
    assert!(all.last().unwrap()["content"].is_string());
}

// ---------------------------------------------------------------------------
// Port of openai-completions-prompt-cache.test.ts
// ---------------------------------------------------------------------------

fn openai_completions_model(overrides: Value) -> Model {
    let mut value = serde_json::to_value(as_completions("openai", "gpt-4o-mini")).unwrap();
    for (k, v) in overrides.as_object().unwrap() {
        value[k] = v.clone();
    }
    model(value)
}

fn sys_hi() -> Context {
    ctx(json!({"systemPrompt": "sys", "messages": [{"role": "user", "content": "hi"}]}))
}

fn session(id: &str) -> StreamOptions {
    StreamOptions { session_id: Some(id.into()), ..Default::default() }
}

async fn cache_request(model: &Model, options: StreamOptions) -> Run {
    run(Entry::Stream, model, &sys_hi(), options, None).await
}

#[tokio::test]
async fn prompt_cache_fields_for_direct_openai() {
    let model = openai_completions_model(json!({}));
    let run1 = cache_request(&model, session("session-123")).await;
    assert_eq!(run1.payload["prompt_cache_key"], "session-123");
    assert!(run1.payload.get("prompt_cache_retention").is_none());

    let run2 = cache_request(&model, StreamOptions { cache_retention: Some(CacheRetention::Long), ..session("session-456") }).await;
    assert_eq!(run2.payload["prompt_cache_key"], "session-456");
    assert_eq!(run2.payload["prompt_cache_retention"], "24h");

    let run3 = cache_request(&model, session(&"x".repeat(67))).await;
    assert_eq!(run3.payload["prompt_cache_key"], "x".repeat(64));

    let run4 = cache_request(&model, StreamOptions { cache_retention: Some(CacheRetention::None), ..session("session-789") }).await;
    assert!(run4.payload.get("prompt_cache_key").is_none());
    assert!(run4.payload.get("prompt_cache_retention").is_none());

    let proxy = openai_completions_model(json!({"baseUrl": "https://proxy.example.com/v1", "compat": {"supportsLongCacheRetention": false}}));
    let run5 = cache_request(&proxy, StreamOptions { cache_retention: Some(CacheRetention::Long), ..session("session-proxy") }).await;
    assert!(run5.payload.get("prompt_cache_key").is_none());
    assert!(run5.payload.get("prompt_cache_retention").is_none());

    let env: ProviderEnv = [("PI_CACHE_RETENTION".to_string(), "long".to_string())].into_iter().collect();
    let run6 = cache_request(&model, StreamOptions { env: Some(env), ..session("session-env") }).await;
    assert_eq!(run6.payload["prompt_cache_key"], "session-env");
    assert_eq!(run6.payload["prompt_cache_retention"], "24h");
}

#[tokio::test]
async fn session_affinity_headers() {
    let affinity = openai_completions_model(json!({"baseUrl": "https://proxy.example.com/v1", "compat": {"sendSessionAffinityHeaders": true}}));
    let r = cache_request(&affinity, session("session-affinity")).await.request;
    assert_eq!(r.header("session_id"), Some("session-affinity"));
    assert_eq!(r.header("x-client-request-id"), Some("session-affinity"));
    assert_eq!(r.header("x-session-affinity"), Some("session-affinity"));

    for id in ["accounts/fireworks/models/glm-5p3", "accounts/fireworks/routers/glm-5p3-fast"] {
        let r = cache_request(&builtin("fireworks", id), session("fireworks-session")).await.request;
        assert_eq!(r.header("x-session-affinity"), Some("fireworks-session"), "{id}");
    }

    let r = cache_request(&builtin("baseten", "zai-org/GLM-5.2"), session("baseten-catalog-session")).await.request;
    assert_eq!(r.header("x-session-affinity"), Some("baseten-catalog-session"));
    assert_eq!(r.header("x-client-request-id"), Some("baseten-catalog-session"));

    let nosession = openai_completions_model(json!({"compat": {"sendSessionAffinityHeaders": true, "sessionAffinityFormat": "openai-nosession"}}));
    let run1 = cache_request(&nosession, session("session-nosession")).await;
    assert!(run1.payload.get("session_id").is_none());
    assert_eq!(run1.payload["prompt_cache_key"], "session-nosession");
    assert_eq!(run1.request.header("session_id"), None);
    assert_eq!(run1.request.header("x-client-request-id"), Some("session-nosession"));
    assert_eq!(run1.request.header("x-session-affinity"), Some("session-nosession"));
    assert_eq!(run1.request.header("x-session-id"), None);

    let openrouter_format = openai_completions_model(json!({"baseUrl": "https://proxy.example.com/v1", "compat": {"sendSessionAffinityHeaders": true, "sessionAffinityFormat": "openrouter"}}));
    let builtin_openrouter = builtin("openrouter", "auto");
    for (model, id) in [(openrouter_format, "session-proxy"), (builtin_openrouter, "session-openrouter")] {
        let run = cache_request(&model, session(id)).await;
        assert!(run.payload.get("session_id").is_none());
        assert!(run.payload.get("prompt_cache_key").is_none());
        assert_eq!(run.request.header("x-session-id"), Some(id));
        for header in ["session_id", "x-client-request-id", "x-session-affinity"] {
            assert_eq!(run.request.header(header), None, "{header}");
        }
    }

    let disabled = openai_completions_model(json!({"provider": "openrouter", "baseUrl": "https://openrouter.ai/api/v1", "compat": {"sendSessionAffinityHeaders": false}}));
    let run = cache_request(&disabled, session("session-openrouter")).await;
    assert!(run.payload.get("session_id").is_none());
    assert!(run.payload.get("prompt_cache_key").is_none());
    assert_eq!(run.request.header("x-session-id"), None);

    let r = cache_request(&affinity, StreamOptions { cache_retention: Some(CacheRetention::None), ..session("session-affinity") }).await.request;
    for header in ["session_id", "x-client-request-id", "x-session-affinity"] {
        assert_eq!(r.header(header), None, "{header}");
    }

    let mut headers = ProviderHeaders::new();
    headers.insert("session_id".into(), Some("override-session".into()));
    headers.insert("x-client-request-id".into(), Some("override-request".into()));
    headers.insert("x-session-affinity".into(), Some("override-affinity".into()));
    let r = cache_request(&affinity, StreamOptions { headers: Some(headers), ..session("session-affinity") }).await.request;
    assert_eq!(r.header("session_id"), Some("override-session"));
    assert_eq!(r.header("x-client-request-id"), Some("override-request"));
    assert_eq!(r.header("x-session-affinity"), Some("override-affinity"));
    assert_eq!(r.header_count("x-session-affinity"), 1);
}

// ---------------------------------------------------------------------------
// Port of openai-completions-provider-stream-event.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exposes_provider_chunks_including_openrouter_metadata() {
    // Regression test for #9784.
    let first = json!({"id": "chatcmpl-1", "model": "anthropic/claude-sonnet-4.6", "choices": [{"index": 0, "delta": {"content": "hello"}}]});
    let last = json!({"id": "chatcmpl-1", "model": "anthropic/claude-sonnet-4.6", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12, "cost": 0.0012, "is_byok": false},
        "openrouter_metadata": {"strategy": "direct", "region": "iad"}});
    let model = model(json!({"id": "openrouter/auto", "name": "OpenRouter Auto", "api": "openai-completions", "provider": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 8192}));
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = seen.clone();
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event: Value, _m: Model| {
            sink.lock().push(event);
            Box::pin(async {})
        })),
        ..Default::default()
    };
    let result = run(Entry::Simple, &model, &user_ctx("hi"), options, Some(vec![first.clone(), last.clone()])).await;
    assert_eq!(content_json(&result.message), json!([{"type": "text", "text": "hello"}]));
    assert_eq!(*seen.lock(), vec![first, last]);
}

// ---------------------------------------------------------------------------
// Port of openai-completions-raw-stop-reason.test.ts
// ---------------------------------------------------------------------------

fn test_openai_model() -> Model {
    model(json!({"id": "test-model", "name": "Test Model", "api": "openai-completions", "provider": "openai",
        "baseUrl": "https://api.openai.com/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 4096}))
}

#[tokio::test]
async fn preserves_raw_finish_reasons() {
    let run1 = run(Entry::Stream, &test_openai_model(), &user_ctx("hello"), StreamOptions::default(),
        Some(vec![json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})])).await;
    assert_eq!(run1.message.stop_reason, StopReason::Stop);
    assert_eq!(run1.message.raw_stop_reason.as_deref(), Some("stop"));
    assert_eq!(run1.message.error_message, None);

    let run2 = run(Entry::Stream, &test_openai_model(), &user_ctx("hello"), StreamOptions::default(),
        Some(vec![json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {}, "finish_reason": "content_filter"}]})])).await;
    assert_eq!(run2.message.stop_reason, StopReason::Error);
    assert_eq!(run2.message.raw_stop_reason.as_deref(), Some("content_filter"));
    assert_eq!(run2.message.error_message.as_deref(), Some("Provider finish_reason: content_filter"));
}

// ---------------------------------------------------------------------------
// Port of openai-completions-reasoning-details.test.ts
// ---------------------------------------------------------------------------

fn gemini_model() -> Model {
    model(json!({"id": "google/gemini-test", "name": "Gemini Test", "api": "openai-completions", "provider": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 100000, "maxTokens": 4096}))
}

fn rd_chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id": "chatcmpl-test", "model": "google/gemini-test", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

fn rd_tool_call_chunk() -> Value {
    rd_chunk(json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}}]}), None)
}

fn encrypted_detail() -> Value {
    json!({"type": "reasoning.encrypted", "id": "call_1", "data": "encrypted-signature"})
}

async fn rd_run(history: Vec<Value>, chunks: Vec<Value>) -> Run {
    let context = ctx(json!({"messages": history, "tools": [read_tool()]}));
    run(Entry::Stream, &gemini_model(), &context, StreamOptions::default(), Some(chunks)).await
}

fn assistant_payload(payload: &Value) -> Value {
    payload["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").cloned().unwrap()
}

fn replay_value(message: &AssistantMessage) -> Value {
    serde_json::to_value(Message::Assistant(message.clone())).unwrap()
}

fn thinking_of(message: &AssistantMessage) -> Value {
    serde_json::to_value(message.content.iter().find(|b| b.as_thinking().is_some()).unwrap()).unwrap()
}

fn ok_chunks() -> Vec<Value> {
    vec![rd_chunk(json!({"content": "ok"}), None), rd_chunk(json!({}), Some("stop"))]
}

#[tokio::test]
async fn reasoning_details_round_trip_through_the_thinking_signature() {
    let first = rd_run(vec![], vec![rd_chunk(json!({"reasoning_details": [encrypted_detail()]}), None), rd_tool_call_chunk(), rd_chunk(json!({}), Some("tool_calls"))]).await;
    assert_eq!(
        thinking_of(&first.message),
        json!({"type": "thinking", "thinking": "", "thinkingSignature": serde_json::to_string(&json!([encrypted_detail()])).unwrap()})
    );
    let call = first.message.content.iter().find(|b| b.as_tool_call().is_some()).unwrap();
    assert_eq!(serde_json::to_value(call).unwrap(), json!({"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}}));
    let replay = rd_run(vec![replay_value(&first.message)], ok_chunks()).await;
    assert_eq!(assistant_payload(&replay.payload)["reasoning_details"], json!([encrypted_detail()]));

    // Older stored messages keep the detail on the tool call's thoughtSignature.
    let mut older = first.message.clone();
    older.content.retain(|b| b.as_thinking().is_none());
    let mut value = replay_value(&older);
    for block in value["content"].as_array_mut().unwrap() {
        if block["type"] == "toolCall" {
            block["thoughtSignature"] = json!(serde_json::to_string(&encrypted_detail()).unwrap());
        }
    }
    let replay = rd_run(vec![value], ok_chunks()).await;
    assert_eq!(assistant_payload(&replay.payload)["reasoning_details"], json!([encrypted_detail()]));
}

#[tokio::test]
async fn reasoning_details_keep_their_sequence_and_merge_deltas() {
    let signed_text = json!({"type": "reasoning.text", "text": "I should call the read tool.", "signature": "sha256:signed-text", "id": "reasoning-text-1", "format": "anthropic-claude-v1", "index": 0});
    let summary = json!({"type": "reasoning.summary", "summary": "Decided to inspect the requested file.", "id": "reasoning-summary-1", "format": "anthropic-claude-v1", "index": 1});
    let first = rd_run(vec![], vec![
        rd_chunk(json!({"reasoning": "I should call the read tool.", "reasoning_details": [signed_text.clone()]}), None),
        rd_chunk(json!({"reasoning_details": [encrypted_detail(), summary.clone()]}), None),
        rd_tool_call_chunk(),
        rd_chunk(json!({}), Some("tool_calls")),
    ])
    .await;
    let expected = json!([signed_text, encrypted_detail(), summary]);
    assert_eq!(
        thinking_of(&first.message),
        json!({"type": "thinking", "thinking": "I should call the read tool.", "thinkingSignature": serde_json::to_string(&expected).unwrap()})
    );
    let replay = rd_run(vec![replay_value(&first.message)], ok_chunks()).await;
    let payload = assistant_payload(&replay.payload);
    assert_eq!(payload["reasoning_details"], expected);
    assert!(payload.get("reasoning").is_none());

    let later = json!({"type": "reasoning.summary", "summary": "After encrypted block.", "format": "openai-responses-v1", "index": 0});
    let first = rd_run(vec![], vec![
        rd_chunk(json!({"reasoning_details": [{"type": "reasoning.text", "text": "The", "index": 0}]}), None),
        rd_chunk(json!({"reasoning_details": [{"type": "reasoning.text", "text": " user wants the time.", "signature": "sha256:text-signature", "format": "openai-responses-v1", "index": 0}]}), None),
        rd_chunk(json!({"reasoning_details": [{"type": "reasoning.summary", "summary": "Looked", "index": 0}]}), None),
        rd_chunk(json!({"reasoning_details": [{"type": "reasoning.summary", "summary": " up time.", "format": "openai-responses-v1", "index": 0}]}), None),
        rd_chunk(json!({"reasoning_details": [encrypted_detail()]}), None),
        rd_chunk(json!({"reasoning_details": [later.clone()]}), None),
        rd_tool_call_chunk(),
        rd_chunk(json!({}), Some("tool_calls")),
    ])
    .await;
    let expected = json!([
        {"type": "reasoning.text", "text": "The user wants the time.", "index": 0, "signature": "sha256:text-signature", "format": "openai-responses-v1"},
        {"type": "reasoning.summary", "summary": "Looked up time.", "index": 0, "format": "openai-responses-v1"},
        encrypted_detail(),
        later
    ]);
    assert_eq!(
        thinking_of(&first.message),
        json!({"type": "thinking", "thinking": "", "thinkingSignature": serde_json::to_string(&expected).unwrap()})
    );
    let replay = rd_run(vec![replay_value(&first.message)], ok_chunks()).await;
    assert_eq!(assistant_payload(&replay.payload)["reasoning_details"], expected);
}

#[tokio::test]
async fn thinking_precedes_text_within_one_delta() {
    // https://github.com/earendil-works/pi/issues/10926
    let run1 = rd_run(vec![], vec![rd_chunk(json!({"role": "assistant", "reasoning_content": "thinking", "content": "answer"}), Some("stop"))]).await;
    assert_eq!(
        content_json(&run1.message),
        json!([{"type": "thinking", "thinking": "thinking", "thinkingSignature": "reasoning_content"}, {"type": "text", "text": "answer"}])
    );
    let run2 = rd_run(vec![], vec![rd_chunk(json!({"reasoning_details": [encrypted_detail()], "content": "answer"}), Some("stop"))]).await;
    assert_eq!(run2.message.content.iter().map(|b| b.type_name()).collect::<Vec<_>>(), vec!["thinking", "text"]);
}

// ---------------------------------------------------------------------------
// Port of openai-completions-response-model.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_model_tracks_routed_chunk_models() {
    let auto = model(json!({"id": "openrouter/auto", "name": "OpenRouter Auto", "api": "openai-completions", "provider": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 8192}));
    let chunks = |model_field: Option<&str>, second: Option<&str>| {
        let with = |mut c: Value, m: Option<&str>| {
            if let Some(m) = m {
                c["model"] = json!(m);
            }
            c
        };
        vec![
            with(json!({"id": "c", "choices": [{"index": 0, "delta": {"content": "hi"}}]}), model_field),
            with(json!({"id": "c", "choices": [{"index": 0, "delta": {"content": "!"}}]}), second),
            with(json!({"id": "c", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage(10, 5)}), model_field),
        ]
    };
    let routed = run(Entry::Stream, &auto, &user_ctx("hi"), StreamOptions::default(), Some(chunks(Some("anthropic/claude-opus-4.8"), Some("anthropic/claude-opus-4.8")))).await;
    assert_eq!(routed.message.model, "openrouter/auto");
    assert_eq!(routed.message.response_model.as_deref(), Some("anthropic/claude-opus-4.8"));
    assert_eq!(routed.message.provider, "openrouter");
    assert_eq!(routed.message.stop_reason, StopReason::Stop);

    let echoed = run(Entry::Stream, &auto, &user_ctx("hi"), StreamOptions::default(), Some(chunks(Some("openrouter/auto"), Some("openrouter/auto")))).await;
    assert_eq!(echoed.message.response_model, None);

    let missing = run(Entry::Stream, &auto, &user_ctx("hi"), StreamOptions::default(), Some(chunks(None, Some("")))).await;
    assert_eq!(missing.message.model, "openrouter/auto");
    assert_eq!(missing.message.response_model, None);
}

// ---------------------------------------------------------------------------
// Port of openai-completions-retry.test.ts
// ---------------------------------------------------------------------------

fn opencode_go_model(base_url: &str) -> Model {
    model(json!({"id": "test-model", "name": "Test Model", "api": "openai-completions", "provider": "opencode-go",
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100}))
}

fn ok_stream() -> Response {
    Response::sse(&openai_sse(&[
        json!({"id": "chatcmpl-test", "choices": [{"index": 0, "delta": {"content": "ok"}}]}),
        json!({"id": "chatcmpl-test", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ]))
}

#[tokio::test]
async fn completions_provider_retries() {
    let context = normalize_context(&ctx(json!({"systemPrompt": "", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}], "timestamp": 0}], "tools": []})));
    // No retries by default: one request per stream.
    let server = MockServer::start(|_, _| Response::json(500, json!({"error": {"message": "server error"}}))).await;
    let result = openai_completions::stream(&opencode_go_model(&format!("{}/zen/go/v1", server.url)), &context, StreamOptions { api_key: Some("test".into()), ..Default::default() }).result().await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(server.requests().len(), 1);

    // Provider-requested delays are honored up to maxRetryDelayMs.
    let server = MockServer::start(|_, index| match index {
        0 => Response::json(429, json!({"error": {"message": "rate limited"}})).with_header("retry-after-ms", "100"),
        1 => Response::json(500, json!({"error": {"message": "server error"}})).with_header("retry-after-ms", "100"),
        _ => ok_stream(),
    })
    .await;
    let started = std::time::Instant::now();
    let options = StreamOptions { api_key: Some("test".into()), max_retries: Some(2), max_retry_delay_ms: Some(100), ..Default::default() };
    let result = openai_completions::stream(&opencode_go_model(&format!("{}/zen/go/v1", server.url)), &context, options).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.text(), "ok");
    assert_eq!(server.requests().len(), 3);
    assert!(started.elapsed() >= std::time::Duration::from_millis(200));

    let server = MockServer::start(|_, _| Response::json(429, json!({"error": {"message": "rate limited"}})).with_header("retry-after", "277403")).await;
    let options = StreamOptions { api_key: Some("test".into()), max_retries: Some(2), max_retry_delay_ms: Some(1000), ..Default::default() };
    let result = openai_completions::stream(&opencode_go_model(&format!("{}/zen/go/v1", server.url)), &context, options).result().await;
    assert_eq!(result.stop_reason, StopReason::Error);
    let message = result.error_message.unwrap();
    assert!(message.contains("Server requested 277403s retry delay (max: 1s)"), "{message}");
    assert!(message.contains("rate limited"), "{message}");
    assert_eq!(server.requests().len(), 1);
}

// ---------------------------------------------------------------------------
// Port of openai-completions-thinking-token-budget.test.ts
// ---------------------------------------------------------------------------

fn vllm_model(compat: Value) -> Model {
    model(json!({"id": "zai-org/glm-5.2", "name": "GLM 5.2 (local vLLM)", "api": "openai-completions", "provider": "local-vllm",
        "baseUrl": "http://localhost:8000/v1", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 262144, "maxTokens": 16384,
        "compat": compat}))
}

fn default_vllm() -> Model {
    vllm_model(json!({"thinkingFormat": "zai", "supportsThinkingTokenBudget": true}))
}

async fn budget(model: &Model, level: Option<ThinkingLevel>, budgets: Value, max_tokens: Option<u64>) -> Value {
    let options = StreamOptions {
        reasoning: level,
        thinking_budgets: Some(serde_json::from_value(budgets).unwrap()),
        max_tokens,
        ..Default::default()
    };
    simple(model, &hi(), options).await.payload
}

#[tokio::test]
async fn thinking_token_budget() {
    let p = budget(&default_vllm(), Some(ThinkingLevel::Medium), json!({"medium": 4096}), None).await;
    assert_eq!(p["thinking_token_budget"], 4096);

    let p = budget(&vllm_model(json!({"thinkingFormat": "zai"})), Some(ThinkingLevel::Medium), json!({"medium": 4096}), None).await;
    for key in ["thinking_token_budget", "thinking_budget", "thinking_budget_tokens"] {
        assert!(p.get(key).is_none(), "{key}");
    }

    let p = budget(&default_vllm(), None, json!({"high": 8192}), None).await;
    assert!(p.get("thinking_token_budget").is_none());

    for level in [ThinkingLevel::Xhigh, ThinkingLevel::Max] {
        let p = budget(&default_vllm(), Some(level), json!({"high": 8192}), None).await;
        assert_eq!(p["thinking_token_budget"], 8192, "{level:?}");
    }

    let p = budget(&default_vllm(), Some(ThinkingLevel::High), json!({}), None).await;
    assert_eq!(p["thinking_token_budget"], 16384 - 1024);

    let p = budget(&default_vllm(), Some(ThinkingLevel::High), json!({"high": 8192}), Some(4096)).await;
    assert_eq!(p["thinking_token_budget"], 4096 - 1024);

    for field in ["thinking_budget", "thinking_budget_tokens"] {
        let p = budget(&vllm_model(json!({"thinkingFormat": "qwen", "thinkingTokenBudgetField": field})), Some(ThinkingLevel::Medium), json!({"medium": 4096}), None).await;
        assert_eq!(p[field], 4096, "{field}");
        assert!(p.get("thinking_token_budget").is_none(), "{field}");
    }

    let p = budget(
        &vllm_model(json!({"thinkingFormat": "zai", "supportsThinkingTokenBudget": true, "thinkingTokenBudgetField": "thinking_budget"})),
        Some(ThinkingLevel::Medium),
        json!({"medium": 4096}),
        None,
    )
    .await;
    assert_eq!(p["thinking_budget"], 4096);
    assert!(p.get("thinking_token_budget").is_none());

    let template = vllm_model(json!({"thinkingFormat": "chat-template", "chatTemplateKwargs": {
        "enable_thinking": {"$var": "thinking.enabled"}, "thinking_budget": {"$var": "thinking.budget"}}}));
    let p = budget(&template, Some(ThinkingLevel::High), json!({}), None).await;
    assert_eq!(p["chat_template_kwargs"], json!({"enable_thinking": true, "thinking_budget": 16384 - 1024}));
    assert!(p.get("thinking_token_budget").is_none());
    let p = budget(&template, None, json!({}), None).await;
    assert_eq!(p["chat_template_kwargs"], json!({"enable_thinking": false}));
}

// ---------------------------------------------------------------------------
// Port of openai-completions-tool-result-images.test.ts
// ---------------------------------------------------------------------------

fn image_model() -> Model {
    let mut model = as_completions("openai", "gpt-4o-mini");
    model.input = vec![ModelInputModality::Text, ModelInputModality::Image];
    model
}

fn image_compat(model: &Model) -> openai_completions::ResolvedCompat {
    let mut compat = openai_completions::get_compat(model);
    compat.supports_store = true;
    compat.supports_developer_role = true;
    compat.supports_strict_mode = true;
    compat.cache_control_format = Some(serde_json::from_value(json!("anthropic")).unwrap());
    compat.send_session_affinity_headers = false;
    compat
}

fn convert(model: &Model, messages: Value) -> Vec<Value> {
    let context = normalize_context(&ctx(json!({"messages": messages})));
    openai_completions::convert_messages(model, &context, &image_compat(model), &HashMap::new()).unwrap()
}

#[test]
fn omits_empty_text_parts_from_user_messages_with_images() {
    // Regression test for https://github.com/earendil-works/pi/issues/9797
    let model = image_model();
    let messages = convert(&model, json!([{"role": "user", "content": [{"type": "text", "text": ""}, {"type": "image", "data": "ZmFrZQ==", "mimeType": "image/png"}]}]));
    assert_eq!(messages, vec![json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,ZmFrZQ=="}}]})]);
}

#[test]
fn batches_tool_result_images_after_consecutive_tool_results() {
    let model = image_model();
    let image_result = |id: &str| {
        json!({"role": "toolResult", "toolCallId": id, "toolName": "read", "isError": false, "content": [
            {"type": "text", "text": "Read image file [image/png]"}, {"type": "image", "data": "ZmFrZQ==", "mimeType": "image/png"}]})
    };
    let messages = convert(&model, json!([
        {"role": "user", "content": "Read the images"},
        completions_assistant(&model.provider, &model.id, json!([
            {"type": "toolCall", "id": "tool-1", "name": "read", "arguments": {"path": "img-1.png"}},
            {"type": "toolCall", "id": "tool-2", "name": "read", "arguments": {"path": "img-2.png"}}
        ]), "toolUse"),
        image_result("tool-1"),
        image_result("tool-2")
    ]));
    assert_eq!(messages.iter().map(|m| m["role"].as_str().unwrap()).collect::<Vec<_>>(), vec!["user", "assistant", "tool", "tool", "user"]);
    let images = messages.last().unwrap()["content"].as_array().unwrap().iter().filter(|p| p["type"] == "image_url").count();
    assert_eq!(images, 2);
}

#[test]
fn uses_no_tool_output_placeholder_for_blank_tool_results() {
    // Whitespace-only cases: https://github.com/earendil-works/pi/issues/8720
    let model = image_model();
    for text in ["", "\r\n", " \n\t"] {
        let messages = convert(&model, json!([
            {"role": "user", "content": "Run the command"},
            completions_assistant(&model.provider, &model.id, json!([{"type": "toolCall", "id": "tool-1", "name": "bash", "arguments": {"command": "true"}}]), "toolUse"),
            {"role": "toolResult", "toolCallId": "tool-1", "toolName": "bash", "isError": false, "content": [{"type": "text", "text": text}]}
        ]));
        let tool = messages.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["content"], "(no tool output)", "{text:?}");
    }
}

// ---------------------------------------------------------------------------
// Port of openai-completions-vllm-priority.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vllm_priority_field() {
    let with_priority = openai_completions_model(json!({"compat": {"vllmPriority": 10}}));
    assert_eq!(run_stream_payload(&with_priority, &sys_hi(), StreamOptions::default()).await["priority"], 10);
    let without = openai_completions_model(json!({}));
    assert!(run_stream_payload(&without, &sys_hi(), StreamOptions::default()).await.get("priority").is_none());
}

// ---------------------------------------------------------------------------
// Port of azure-openai-completions.test.ts
// ---------------------------------------------------------------------------

struct AzureRun {
    requests: Vec<Request>,
    message: AssistantMessage,
}

async fn azure_run(model_id: &str, context: Context, simple_entry: bool, options: StreamOptions, extra_env: &[(&str, &str)]) -> AzureRun {
    let body = openai_sse(&default_chunks());
    let server = MockServer::start(move |_, _| Response::sse(&body)).await;
    let mut env: ProviderEnv = [("AZURE_OPENAI_BASE_URL".to_string(), server.url.clone())].into_iter().collect();
    for (k, v) in extra_env {
        env.insert(k.to_string(), v.to_string());
    }
    let provider = pi_ai::providers::azure::azure_provider();
    let model = builtin("azure", model_id);
    let options = StreamOptions { api_key: Some("test-key".into()), env: Some(env), ..options };
    let transcript = normalize_context(&context);
    let stream = if simple_entry { provider.stream_simple(&model, &transcript, options) } else { provider.stream(&model, &transcript, options) };
    let message = stream.result().await;
    AzureRun { requests: server.requests(), message }
}

fn azure_hi() -> Context {
    ctx(json!({"systemPrompt": "sys", "messages": [{"role": "user", "content": "hi"}]}))
}

#[tokio::test]
async fn azure_deepseek_reasoning_effort_and_caching() {
    // Regression for #9645.
    let run1 = azure_run("deepseek-v4-pro", azure_hi(), true, reasoning(ThinkingLevel::High), &[]).await;
    let p = run1.requests[0].json();
    assert_eq!(p["reasoning_effort"], "high");
    assert!(p.get("thinking").is_none());

    let run2 = azure_run("deepseek-v4-pro", azure_hi(), true, reasoning(ThinkingLevel::Max), &[]).await;
    assert_eq!(run2.requests[0].json()["reasoning_effort"], "high");

    let run3 = azure_run("deepseek-v4-pro", azure_hi(), true, StreamOptions::default(), &[]).await;
    let p = run3.requests[0].json();
    assert!(p.get("reasoning_effort").is_none());
    assert!(p.get("thinking").is_none());

    let run4 = azure_run("deepseek-v4-pro", azure_hi(), false, session("session-env"), &[("PI_CACHE_RETENTION", "long")]).await;
    let p = run4.requests[0].json();
    assert!(p.get("prompt_cache_key").is_none());
    assert!(p.get("prompt_cache_retention").is_none());

    let run5 = azure_run("deepseek-v4-pro", azure_hi(), false, StreamOptions { cache_retention: Some(CacheRetention::Long), ..session("session-1") }, &[]).await;
    let p = run5.requests[0].json();
    assert!(p.get("prompt_cache_key").is_none());
    assert!(p.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn azure_deepseek_system_messages_and_reasoning_replay() {
    let run1 = azure_run("deepseek-v4-pro", azure_hi(), false, StreamOptions::default().with_extra("reasoningEffort", "low"), &[]).await;
    let first = run1.requests[0].json()["messages"][0].clone();
    assert_eq!(first["role"], "system");
    assert_eq!(first["content"], "sys");

    let resumed = ctx(json!({"systemPrompt": "first", "messages": [
        {"role": "user", "content": "hi"}, {"role": "system", "content": "second"}, {"role": "user", "content": "again"}
    ]}));
    let run2 = azure_run("deepseek-v4-pro", resumed, false, StreamOptions::default(), &[]).await;
    let roles: Vec<Value> = run2.requests[0].json()["messages"].as_array().unwrap().iter().map(|m| m["role"].clone()).collect();
    assert_eq!(roles, vec![json!("system"), json!("user"), json!("system"), json!("user")]);

    let with_assistant = ctx(json!({"systemPrompt": "sys", "messages": [
        {"role": "user", "content": "first"},
        completions_assistant("azure", "deepseek-v4-pro", json!([
            {"type": "thinking", "thinking": "internal reasoning", "thinkingSignature": "reasoning_content"},
            {"type": "text", "text": "answer"}
        ]), "stop"),
        {"role": "user", "content": "second"}
    ]}));
    let run3 = azure_run("deepseek-v4-pro", with_assistant, false, StreamOptions::default(), &[]).await;
    let assistant = assistant_payload(&run3.requests[0].json());
    assert_eq!(assistant["reasoning_content"], "internal reasoning");
}

#[tokio::test]
async fn azure_completions_endpoint_resolution() {
    let model = builtin("azure", "deepseek-v4-pro");
    let env: ProviderEnv = [("AZURE_OPENAI_BASE_URL".to_string(), "https://my-resource.services.ai.azure.com".to_string())].into_iter().collect();
    let options = StreamOptions { env: Some(env), ..Default::default() };
    assert_eq!(
        pi_ai::api::azure_openai_config::resolve_azure_base_url(&model, Some(&options)).unwrap(),
        "https://my-resource.services.ai.azure.com/openai/v1"
    );

    {
        let _lock = ENV_LOCK.lock().await;
        let _guard = EnvGuard::set(&[("AZURE_OPENAI_BASE_URL", None), ("AZURE_OPENAI_RESOURCE_NAME", None)]);
        let provider = pi_ai::providers::azure::azure_provider();
        let result = provider
            .stream(&model, &normalize_context(&azure_hi()), StreamOptions { api_key: Some("test-key".into()), ..Default::default() })
            .result()
            .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error_message.as_deref().unwrap().contains("Azure OpenAI base URL is required"), "{:?}", result.error_message);
    }

    let run1 = azure_run("deepseek-v4-pro", azure_hi(), false, StreamOptions::default(), &[]).await;
    assert_eq!(run1.requests[0].json()["model"], "deepseek-v4-pro");
    assert_eq!(run1.message.model, "deepseek-v4-pro");
    assert_eq!(run1.requests[0].path_only(), "/chat/completions");

    let mapped = [("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", "deepseek-v4-pro=my-deepseek")];
    let run2 = azure_run("deepseek-v4-pro", azure_hi(), true, StreamOptions::default(), &mapped).await;
    assert_eq!(run2.requests[0].json()["model"], "my-deepseek");
    assert_eq!(run2.message.model, "deepseek-v4-pro");

    let seen = Arc::new(Mutex::new(None::<Value>));
    let sink = seen.clone();
    let options = StreamOptions {
        on_payload: Some(Arc::new(move |payload: Value, _m: Model| {
            *sink.lock() = Some(payload["model"].clone());
            let mut payload = payload;
            payload["temperature"] = json!(0.1);
            Box::pin(async move { Some(payload) })
        })),
        ..Default::default()
    };
    let run3 = azure_run("deepseek-v4-pro", azure_hi(), false, options, &mapped).await;
    assert_eq!(*seen.lock(), Some(json!("my-deepseek")));
    let p = run3.requests[0].json();
    assert_eq!(p["model"], "my-deepseek");
    assert_eq!(p["temperature"], 0.1);
}

#[tokio::test]
async fn azure_api_map_routes_by_model_api() {
    let responses = azure_run("gpt-4o-mini", azure_hi(), false, StreamOptions::default(), &[]).await;
    assert!(responses.requests[0].path_only().ends_with("/responses"), "{}", responses.requests[0].path);
    let completions = azure_run("deepseek-v4-pro", azure_hi(), false, StreamOptions::default(), &[]).await;
    assert!(completions.requests[0].path_only().ends_with("/chat/completions"));
}

// ---------------------------------------------------------------------------
// Port of openai-completions-empty-tools.test.ts
// ---------------------------------------------------------------------------

fn gpt_4o_mini_completions() -> Model {
    let mut model = get_builtin_model("openai", "gpt-4o-mini").unwrap();
    model.api = "openai-completions".into();
    model.compat = None;
    model
}

async fn completions_payload(model: &Model, context: Value, max_tokens: Option<u64>) -> Value {
    let options = StreamOptions { api_key: Some("test".into()), max_tokens, ..Default::default() };
    simple_payload(model, &ctx(context), options).await
}

#[tokio::test]
async fn empty_tools_and_max_tokens() {
    // Regression for #3649: `tools: []` is rejected by some OpenAI-compatible backends.
    let model = gpt_4o_mini_completions();
    let hi = json!([{"role": "user", "content": "hi", "timestamp": 1}]);
    let body = completions_payload(&model, json!({"messages": hi, "tools": []}), None).await;
    assert!(body.get("tools").is_none());
    let body = completions_payload(&model, json!({"messages": hi}), None).await;
    assert!(body.get("tools").is_none());
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"], model.max_tokens);
    let body = completions_payload(&model, json!({"messages": hi}), Some(1234)).await;
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"], 1234);

    // Defaults and explicit values are clamped to the remaining context.
    let mut small = gpt_4o_mini_completions();
    small.context_window = 10000;
    small.max_tokens = 8000;
    let long = json!([{"role": "user", "content": "x".repeat(8000), "timestamp": 1}]);
    for explicit in [None, Some(7000)] {
        let body = completions_payload(&small, json!({"messages": long}), explicit).await;
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"], 3618, "{explicit:?}");
    }

    // Tool history still sends `tools: []` (Anthropic/LiteLLM proxies require it).
    let body = completions_payload(
        &model,
        json!({"messages": [
            {"role": "user", "content": "use the tool", "timestamp": 1},
            {"role": "assistant", "content": [{"type": "toolCall", "id": "t1", "name": "noop", "arguments": {}}], "stopReason": "toolUse",
             "api": "openai-completions", "provider": "openai", "model": "gpt-4o-mini", "timestamp": 2},
            {"role": "toolResult", "toolCallId": "t1", "toolName": "noop", "content": [{"type": "text", "text": "done"}], "isError": false, "timestamp": 3}
        ], "tools": []}),
        None,
    )
    .await;
    assert_eq!(body["tools"], json!([]));
}

/// Stands in for the Cloudflare gateway host: records the materialized base URL, then sends the
/// request (with every header the adapter built) to the mock server.
struct GatewayRedirect {
    server: String,
    base_urls: Arc<Mutex<Vec<String>>>,
}

impl ProviderStreams for GatewayRedirect {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.stream_simple(model, context, options)
    }
    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.base_urls.lock().push(model.base_url.clone());
        let mut model = model.clone();
        model.base_url = self.server.clone();
        let api = pi_ai::compat::get_api_provider(&model.api).expect("builtin api");
        api.streams.stream_simple(&model, context, StreamOptions { max_retries: Some(0), ..options })
    }
}

/// The compat entry point without an api key: the Cloudflare AI Gateway provider auth resolves
/// `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_GATEWAY_ID`.
async fn cloudflare_gateway_request(id: &str, system_prompt: Option<&str>, options: StreamOptions) -> (Request, String) {
    let server = MockServer::start(|_, _| Response::json(400, json!({"error": {"message": "stubbed"}}))).await;
    let base_urls: Arc<Mutex<Vec<String>>> = Arc::default();
    let redirect = Arc::new(GatewayRedirect { server: server.url.clone(), base_urls: base_urls.clone() });
    let provider = create_provider(CreateProviderOptions {
        id: "cloudflare-ai-gateway".into(),
        auth: pi_ai::auth::ProviderAuth { api_key: Some(pi_ai::providers::cloudflare::cloudflare_ai_gateway_auth()), oauth: None },
        models: pi_ai::providers::all::get_builtin_models("cloudflare-ai-gateway").into_iter().map(AnyModel::Chat).collect(),
        api: Some(ProviderApi::Single(Arc::new(pi_ai::providers::cloudflare::CloudflareStreams(redirect)))),
        ..Default::default()
    })
    .unwrap();
    let models = create_models(CreateModelsOptions {
        auth_context: Some(MapAuthContext::new(&[
            ("CLOUDFLARE_API_KEY", "cf-token"),
            ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
            ("CLOUDFLARE_GATEWAY_ID", "gateway-id"),
        ])),
        ..Default::default()
    });
    models.set_provider(provider);
    let model = get_builtin_model("cloudflare-ai-gateway", id).unwrap();
    let mut context = user_ctx("hi");
    context.system_prompt = system_prompt.map(str::to_string);
    let result = models.stream_simple(&model, &context, options).result().await;
    let request = server.requests().into_iter().next().unwrap_or_else(|| panic!("no request: {:?}", result.error_message));
    let base_url = base_urls.lock().first().cloned().unwrap();
    (request, base_url)
}

#[tokio::test]
async fn cloudflare_ai_gateway_compat_requests() {
    let workers = "workers-ai/@cf/moonshotai/kimi-k2.6";
    let gateway_url = "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/compat";
    let options = StreamOptions { max_tokens: Some(1234), reasoning: Some(ThinkingLevel::High), ..Default::default() };
    let (request, base_url) = cloudflare_gateway_request(workers, Some("You are helpful."), options).await;
    let body = request.json();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["max_tokens"], 1234);
    assert!(body.get("max_completion_tokens").is_none());
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("store").is_none());
    assert_eq!(base_url, gateway_url);
    assert_eq!(request.header("authorization"), None);
    assert_eq!(request.header("cf-aig-authorization"), Some("Bearer cf-token"));

    // The base URL resolves through provider auth.
    let (_, base_url) = cloudflare_gateway_request(workers, None, StreamOptions::default()).await;
    assert_eq!(base_url, gateway_url);

    // BYOK: an inline upstream Authorization survives next to the gateway auth.
    let mut headers = ProviderHeaders::new();
    headers.insert("Authorization".into(), Some("Bearer upstream-token".into()));
    let (request, _) = cloudflare_gateway_request("gpt-5.1", None, StreamOptions { headers: Some(headers), ..Default::default() }).await;
    assert_eq!(request.header("authorization"), Some("Bearer upstream-token"));
    assert_eq!(request.header("cf-aig-authorization"), Some("Bearer cf-token"));

    // Session affinity headers for Workers AI through the gateway.
    let (request, _) = cloudflare_gateway_request(workers, None, StreamOptions { session_id: Some("session-1".into()), ..Default::default() }).await;
    assert_eq!(request.header("session_id"), Some("session-1"));
    assert_eq!(request.header("x-client-request-id"), Some("session-1"));
    assert_eq!(request.header("x-session-affinity"), Some("session-1"));
}

// ---------------------------------------------------------------------------
// Port of openai-completions-thinking-as-text.test.ts
// ---------------------------------------------------------------------------

fn thinking_as_text_model(base_url: &str) -> Model {
    model(json!({"id": "repro-model", "name": "Repro Model", "api": "openai-completions", "provider": "repro-provider", "baseUrl": base_url,
        "reasoning": true, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000,
        "maxTokens": 4096, "compat": {"supportsStore": true, "supportsDeveloperRole": true, "supportsReasoningEffort": true,
        "supportsUsageInStreaming": true, "supportsFinishReason": true, "maxTokensField": "max_completion_tokens", "requiresToolResultName": false,
        "requiresAssistantAfterToolResult": false, "requiresThinkingAsText": true, "requiresReasoningContentOnAssistantMessages": false,
        "thinkingFormat": "openai", "zaiToolStream": false, "supportsThinkingTokenBudget": false, "supportsStrictMode": true,
        "supportsOpenAIGrammarTools": false, "supportsMidConvoSystemMessages": false, "supportsMidConvoToolAdditions": false,
        "sendSessionAffinityHeaders": false, "sessionAffinityFormat": "openai", "supportsLongCacheRetention": true}}))
}

fn thinking_replay_context(content: Value) -> Context {
    ctx(json!({"messages": [
        {"role": "user", "content": "hello", "timestamp": 1},
        {"role": "assistant", "content": content, "api": "openai-completions", "provider": "repro-provider", "model": "repro-model", "stopReason": "stop", "timestamp": 2},
        {"role": "user", "content": "continue", "timestamp": 3}
    ]}))
}

#[tokio::test]
async fn thinking_replays_as_assistant_text_parts() {
    let model = thinking_as_text_model("http://127.0.0.1:1");
    let both = json!([{"type": "thinking", "thinking": "internal reasoning"}, {"type": "text", "text": "visible answer"}]);
    let expected_both = json!({"role": "assistant", "content": [{"type": "text", "text": "internal reasoning"}, {"type": "text", "text": "visible answer"}]});
    let body = stream_payload(&model, &thinking_replay_context(both.clone()), StreamOptions::default()).await;
    assert_eq!(body["messages"][1], expected_both);
    let body = stream_payload(&model, &thinking_replay_context(json!([{"type": "thinking", "thinking": "internal reasoning"}])), StreamOptions::default()).await;
    assert_eq!(body["messages"][1], json!({"role": "assistant", "content": [{"type": "text", "text": "internal reasoning"}]}));

    // The replay reaches the endpoint and the stream completes.
    let sse = openai_sse(&[
        json!({"id": "chatcmpl-repro", "object": "chat.completion.chunk", "created": 0, "model": "repro-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}]}),
        json!({"id": "chatcmpl-repro", "object": "chat.completion.chunk", "created": 0, "model": "repro-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}}),
    ]);
    let server = MockServer::start(move |req, _| {
        if req.method != "POST" || req.path != "/chat/completions" {
            return Response::text(404, "");
        }
        Response::sse(&sse)
    })
    .await;
    let model = thinking_as_text_model(&server.url);
    let context = normalize_context(&thinking_replay_context(both));
    let options = StreamOptions { api_key: Some("test-key".into()), ..Default::default() };
    let (events, _) = openai_completions::stream(&model, &context, options).collect_all().await;
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].json()["messages"][1], expected_both);
    assert!(matches!(events.last(), Some(AssistantMessageEvent::Done { .. })), "{:?}", events.last());
}
