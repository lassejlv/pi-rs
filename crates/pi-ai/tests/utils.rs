//! Ports of utility tests: json-parse, retry, overflow, transform-messages, transcript, estimate.

use std::sync::Arc;

use parking_lot::Mutex;
use pi_ai::api::transform_messages::transform_messages;
use pi_ai::providers::faux::*;
use pi_ai::utils::estimate::estimate_context_tokens;
use pi_ai::utils::json_parse::*;
use pi_ai::utils::overflow::is_context_overflow;
use pi_ai::*;
use serde_json::json;

fn error_message(message: &str) -> AssistantMessage {
    faux_assistant_message(
        vec![],
        FauxMessageOptions { stop_reason: Some(StopReason::Error), error_message: Some(message.into()), ..Default::default() },
    )
}

// ---- json-parse.test.ts ----

#[test]
fn streaming_json_keeps_invalid_unicode_escapes() {
    assert_eq!(parse_streaming_json(Some(r#"{"text":"\url{x}"}"#)), json!({"text": "\\url{x}"}));
    assert_eq!(
        parse_streaming_json(Some(r#"{"path":"a.tex","content":"\usepackage{x}","mode":"w"}"#)),
        json!({"path": "a.tex", "content": "\\usepackage{x}", "mode": "w"})
    );
    assert_eq!(parse_streaming_json(Some(r#"{"text":"café \url"}"#)), json!({"text": "café \\url"}));
    assert_eq!(parse_streaming_json(Some(r#"{"text":"ab\u00"#)), json!({"text": "ab"}));
}

#[test]
fn streaming_json_partial_values() {
    assert_eq!(parse_streaming_json(Some(r#"{"a": 1, "b": [1, 2"#)), json!({"a": 1, "b": [1, 2]}));
    assert_eq!(parse_streaming_json(Some(r#"{"a": "hel"#)), json!({"a": "hel"}));
    assert_eq!(parse_streaming_json(Some(r#"{"a": tr"#)), json!({"a": true}));
    assert_eq!(parse_streaming_json(Some(r#"{"a": 12"#)), json!({"a": 12}));
    assert_eq!(parse_streaming_json(Some(r#"{"a": {"b": nu"#)), json!({"a": {"b": null}}));
    assert_eq!(parse_streaming_json(Some("")), json!({}));
    assert_eq!(parse_streaming_json(Some(r#"{"a":"#)), json!({}));
}

#[test]
fn tool_call_argument_parsing() {
    let parsed = parse_tool_call_arguments(Some(r#"{"kind":"seek","note":"\url"}"#));
    assert_eq!(serde_json::Value::Object(parsed.arguments), json!({"kind": "seek", "note": "\\url"}));
    assert!(parsed.arguments_error.is_none());
    assert_eq!(parse_tool_call_arguments(Some("")), ParsedToolCallArguments::default());
    let raw = r#"{"action":"apply","actions":[{"kind":"seek","time":.5}]}"#;
    let malformed = parse_tool_call_arguments(Some(raw));
    assert!(malformed.arguments.is_empty());
    let error = malformed.arguments_error.unwrap();
    assert!(error.contains("\n\nReceived arguments:\n"));
    assert!(error.ends_with(raw));
    let cut = r#"{"kind":"seek","clipId":"clip-fl"#;
    assert!(parse_tool_call_arguments(Some(cut)).arguments_error.unwrap().ends_with(cut));
    assert_eq!(
        parse_tool_call_arguments(Some("[1,2]")).arguments_error.as_deref(),
        Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]")
    );
}

// ---- retry.test.ts ----

#[test]
fn retry_classification() {
    for retryable in [
        "An error occurred while processing your request. You can retry your request, or contact us.",
        r#"{"message":"The system encountered an unexpected error during processing. Try your request again."}"#,
        "ResourceExhausted: Worker local total request limit reached (288/48)",
        "The socket connection was closed unexpectedly.",
        "Error: exceeded request buffer limit while retrying upstream",
        "The pending stream has been canceled (caused by: getaddrinfo ENOTFOUND bedrock-runtime.us-east-1.amazonaws.com)",
        "connect ENOTFOUND api.example.com",
        "EAI_AGAIN api.example.com",
        "The service is temporarily unavailable. Please retry later.",
        "Service unavailable: The service is temporarily unavailable.",
        "request to https://oauth2.googleapis.com/token failed, reason:",
        "request to https://oauth2.googleapis.com/token failed, reason: connect ECONNREFUSED 127.0.0.1:9",
        "unknown: Unable to forward upstream stream",
        "upstream_error: Upstream request failed",
        "upstream_stream_read_error: Upstream response stream was interrupted",
        "Upstream HTTP/2 stream failed",
        "OpenAI Responses stream ended before a terminal response event",
        "The system is currently experiencing high demand and cannot process your request.",
        "subscription_sharing_usage_unavailable: Usage cannot be checked.",
        "overloaded_error",
        "520 status code (no body)",
        "524 status code (no body)",
    ] {
        assert!(is_retryable_assistant_error(&error_message(retryable)), "{retryable}");
    }
    for non_retryable in [
        "429 quota exceeded",
        r#"OpenAI API error (429): {"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached."}"#,
        r#"OpenAI API error (429): {"code":"subscription_sharing_workplace_individual_usage_limit_exceeded","message":"Usage limit reached."}"#,
        r#"401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011CU503xyz"}"#,
        "400 image exceeds 5 MB maximum: 5316784 bytes > 5242880 bytes",
    ] {
        assert!(!is_retryable_assistant_error(&error_message(non_retryable)), "{non_retryable}");
    }
    assert!(!is_retryable_assistant_error(&faux_text_message("not an error")));
}

#[test]
fn retry_delay_caps() {
    assert_eq!(retry_delay_ms(2000, None, 6), 60000);
    assert_eq!(retry_delay_ms(2000, Some(5000), 5), 5000);
    assert_eq!(retry_delay_ms(2000, Some(0), 5), 0);
}

fn policy(max: u32, base: u64, cap: Option<u64>) -> RetryPolicy {
    RetryPolicy { enabled: true, max_retries: max, base_delay_ms: base, max_agent_delay_ms: cap }
}

#[tokio::test]
async fn retry_assistant_call_behaviour() {
    let calls = Arc::new(Mutex::new(0));
    type Finished = Vec<(bool, u32, Option<String>)>;
    let finished: Arc<Mutex<Finished>> = Arc::default();
    let scheduled: Arc<Mutex<Vec<u64>>> = Arc::default();
    let callbacks = {
        let finished = finished.clone();
        let scheduled = scheduled.clone();
        RetryCallbacks {
            on_retry_scheduled: Some(Box::new(move |_a, _m, delay, _e| {
                scheduled.lock().push(delay);
                Box::pin(async {})
            })),
            on_retry_attempt_start: None,
            on_retry_finished: Some(Box::new(move |s, a, e| {
                finished.lock().push((s, a, e));
                Box::pin(async {})
            })),
        }
    };
    let counter = calls.clone();
    let result = retry_assistant_call(
        || {
            let counter = counter.clone();
            async move {
                *counter.lock() += 1;
                error_message("terminated")
            }
        },
        Some(&policy(3, 0, None)),
        None,
        Some(&callbacks),
    )
    .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(*calls.lock(), 4);
    assert_eq!(finished.lock().last().cloned(), Some((false, 3, Some("terminated".to_string()))));

    let n = Arc::new(Mutex::new(0));
    let counter = n.clone();
    scheduled.lock().clear();
    let result = retry_assistant_call(
        || {
            let counter = counter.clone();
            async move {
                let mut c = counter.lock();
                *c += 1;
                if *c < 5 { error_message("terminated") } else { faux_text_message("recovered") }
            }
        },
        Some(&policy(4, 10, Some(15))),
        None,
        Some(&callbacks),
    )
    .await;
    assert_eq!(result.text(), "recovered");
    assert_eq!(scheduled.lock().clone(), vec![10, 15, 15, 15]);

    let quota_calls = Arc::new(Mutex::new(0));
    let counter = quota_calls.clone();
    retry_assistant_call(
        || {
            let counter = counter.clone();
            async move {
                *counter.lock() += 1;
                error_message("insufficient_quota")
            }
        },
        Some(&policy(3, 0, None)),
        None,
        None,
    )
    .await;
    assert_eq!(*quota_calls.lock(), 1);
}

#[tokio::test]
async fn retry_backoff_abort_returns_aborted_message() {
    let signal = tokio_util::sync::CancellationToken::new();
    let cancel = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();
    });
    let result = retry_assistant_call(|| async { error_message("terminated") }, Some(&policy(5, 10_000, None)), Some(&signal), None).await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert!(result.error_message.is_none());
}

// ---- overflow.test.ts ----

#[test]
fn overflow_detection() {
    for message in [
        r#"github-copilot API error (413): {"message":"failed to parse request","code":""}"#,
        "custom-provider API error (413): Payload Too Large",
        r#"413 {"error":{"message":"failed to parse request","code":""}}"#,
        "413",
        "400 `prompt too long; exceeded max context length by 100918 tokens`",
        r#"400 {"code":"1261","message":"Prompt too long"}"#,
        r#"400 {"code":"1261","message":"Prompt exceeds max length"}"#,
        "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
        "Error: 400 Input length (265330) exceeds model's maximum context length (262144).",
        "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
        "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
        "prompt is too long: 213462 tokens > 200000 maximum",
    ] {
        assert!(is_context_overflow(&error_message(message), Some(1_000_000)), "{message}");
    }
    for message in [
        r#"github-copilot API error (400): {"message":"failed to parse request","code":""}"#,
        "custom-provider API error (429): Too many requests",
        "custom-provider API error (4130): invalid input",
        "4130 failed to parse request",
        "Throttling error: Too many tokens, please wait before trying again.",
    ] {
        assert!(!is_context_overflow(&error_message(message), None), "{message}");
    }
    let mut aborted = error_message("413");
    aborted.stop_reason = StopReason::Aborted;
    assert!(!is_context_overflow(&aborted, None));
    let mut cerebras = error_message("400 status code (no body)");
    cerebras.provider = "cerebras".into();
    assert!(is_context_overflow(&cerebras, None));

    let mut silent = faux_text_message("ok");
    silent.usage.input = 200_001;
    assert!(is_context_overflow(&silent, Some(200_000)));
    let mut length = faux_text_message("");
    length.stop_reason = StopReason::Length;
    length.usage.input = 99_000;
    assert!(is_context_overflow(&length, Some(100_000)));
    assert!(is_recoverable_length(&length, 10));
}

// ---- transform-messages ----

fn assistant(provider: &str, api: &str, model: &str, content: Vec<AssistantContent>, stop: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: api.into(),
        provider: provider.into(),
        model: model.into(),
        stop_reason: stop,
        timestamp: 1,
        ..Default::default()
    })
}

fn target_model() -> Model {
    Model {
        id: "claude".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        input: vec![ModelInputModality::Text],
        ..Default::default()
    }
}

#[test]
fn transform_drops_foreign_thinking_and_synthesizes_results() {
    let call = ToolCall { id: "call|x".into(), name: "read".into(), thought_signature: Some("sig".into()), ..Default::default() };
    let messages = vec![
        Message::User(UserMessage::new(
            vec![UserContentBlock::text("look"), UserContentBlock::Image(ImageContent { data: "A".into(), mime_type: "image/png".into() })],
            1,
        )),
        assistant(
            "openai",
            "openai-responses",
            "gpt",
            vec![AssistantContent::Thinking(ThinkingContent::new("secret")), AssistantContent::ToolCall(call)],
            StopReason::ToolUse,
        ),
        Message::System(SystemMessage { content: "update".into(), timestamp: 2, ..Default::default() }),
        assistant("anthropic", "anthropic-messages", "claude", vec![], StopReason::Error),
        Message::User(UserMessage::new("next", 3)),
    ];
    let normalize = |id: &str, _: &Model, _: &AssistantMessage| id.replace('|', "_");
    let out = transform_messages(&messages, &target_model(), Some(&normalize));
    // Images downgraded for a text-only model.
    let user = out[0].as_user().unwrap();
    assert_eq!(
        serde_json::to_value(&user.content).unwrap(),
        json!([{"type": "text", "text": "look"}, {"type": "text", "text": "(image omitted: model does not support images)"}])
    );
    let a = out[1].as_assistant().unwrap();
    assert_eq!(a.content.len(), 1, "foreign thinking dropped");
    let tc = a.content[0].as_tool_call().unwrap();
    assert_eq!(tc.id, "call_x");
    assert!(tc.thought_signature.is_none());
    // Synthetic result comes before the held system message; errored assistant is skipped.
    let synthetic = out[2].as_tool_result().unwrap();
    assert_eq!(synthetic.tool_call_id, "call_x");
    assert!(synthetic.is_error);
    assert!(matches!(out[3], Message::System(_)));
    assert!(matches!(out[4], Message::User(_)));
    assert_eq!(out.len(), 5);
}

#[test]
fn lax_null_content_is_normalized() {
    let messages: Vec<Message> = serde_json::from_value(json!([
        {"role": "user", "content": null, "timestamp": 1},
        {"role": "assistant", "content": null, "api": "openai-completions", "provider": "openai", "model": "test-model",
         "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                   "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
         "stopReason": "stop", "timestamp": 1},
        {"role": "toolResult", "toolCallId": "call_1", "toolName": "web_search", "isError": false, "timestamp": 1}
    ]))
    .unwrap();
    let out = transform_messages(&messages, &target_model(), None);
    assert_eq!(out.len(), 3);
}

// ---- transcript ----

#[test]
fn transcript_replay_and_tool_changes() {
    let tool = |name: &str| Tool::new(name, "d", json!({"type": "object"}));
    let mut sections = indexmap::IndexMap::new();
    sections.insert("rules".to_string(), Some("<rules/>".to_string()));
    let messages = vec![
        Message::System(SystemMessage {
            content: "base".into(),
            sections: Some(sections),
            tools_added: Some(vec![tool("a"), tool("b")]),
            timestamp: 5,
            ..Default::default()
        }),
        Message::User(UserMessage::new("hi", 6)),
        Message::System(SystemMessage {
            content: "more".into(),
            tools_removed: Some(vec![ToolReference { name: "a".into() }]),
            tools_added: Some(vec![tool("c")]),
            timestamp: 7,
            ..Default::default()
        }),
    ];
    let current = get_current_tools(&messages);
    assert_eq!(current.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["b", "c"]);
    assert_eq!(get_current_system_prompt(&messages), "base\n\nmore\n\n<rules/>");
    let collapsed = collapse_system_messages(&transcript_from_messages(messages.clone()));
    assert_eq!(collapsed.messages.len(), 2);
    assert!(has_non_additive_tool_changes(&messages));
    let tools = resolve_transcript_tools(&messages, TranscriptToolSupport::Additions);
    assert_eq!(tools.folded_system_messages, 2);
    let changes = get_tool_state_changes(&[tool("a"), tool("b")], &[tool("b"), tool("c")]);
    assert_eq!(changes.tools_added.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["c"]);
    assert_eq!(changes.tools_removed, vec![ToolReference { name: "a".into() }]);
    let update = render_system_message_update(&SystemMessage {
        content: "x".into(),
        sections: Some([("s".to_string(), None)].into_iter().collect()),
        ..Default::default()
    });
    assert_eq!(update, "x\n\nRemoved system prompt section \"s\".");
}

// ---- context-estimate ----

#[test]
fn context_estimate_uses_last_usage() {
    let mut a = faux_text_message("answer");
    a.timestamp = 10;
    a.usage.total_tokens = 1000;
    let messages = vec![Message::User(UserMessage::new("q", 5)), Message::Assistant(a), Message::User(UserMessage::new("1234567", 11))];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 1000);
    assert_eq!(estimate.trailing_tokens, 2);
    assert_eq!(estimate.tokens, 1002);
    assert_eq!(estimate.last_usage_index, Some(1));
}

#[test]
fn uuid_and_hash() {
    let id = uuidv7();
    assert_eq!(id.len(), 36);
    assert_eq!(pi_ai::utils::hash::short_hash("hello"), pi_ai::utils::hash::short_hash("hello"));
    assert_ne!(pi_ai::utils::hash::short_hash("hello"), pi_ai::utils::hash::short_hash("hellp"));
}
