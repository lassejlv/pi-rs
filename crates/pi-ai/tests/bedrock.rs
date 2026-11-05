//! Offline tests for the Bedrock Converse stream API (ports of `bedrock-*.test.ts`).

mod mock_http;

use std::sync::Arc;

use base64::Engine;
use mock_http::{MockServer, Response};
use parking_lot::Mutex;
use pi_ai::api::aws_eventstream::{HeaderValue, encode_message};
use pi_ai::api::bedrock_converse_stream::{self, resolve_client_config};
use pi_ai::providers::all::get_builtin_model;
use pi_ai::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn event(event_type: &str, payload: Value) -> Vec<u8> {
    encode_message(
        &[
            (":message-type", HeaderValue::String("event".into())),
            (":event-type", HeaderValue::String(event_type.into())),
            (":content-type", HeaderValue::String("application/json".into())),
        ],
        payload.to_string().as_bytes(),
    )
}

fn items(items: &[Value]) -> Vec<u8> {
    let mut body = Vec::new();
    for item in items {
        let (name, payload) = item.as_object().unwrap().iter().next().unwrap();
        body.extend(event(name, payload.clone()));
    }
    body
}

fn eventstream(body: Vec<u8>) -> Response {
    Response::new(200, "application/vnd.amazon.eventstream", body).with_header("x-amzn-requestid", "req-123")
}

fn env(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn skip_auth_options() -> StreamOptions {
    StreamOptions {
        cache_retention: Some(CacheRetention::None),
        env: Some(env(&[("AWS_BEDROCK_SKIP_AUTH", "1"), ("AWS_REGION", "us-east-1")])),
        ..Default::default()
    }
}

fn opus_at(url: &str) -> Model {
    let mut model = get_builtin_model("amazon-bedrock", "us.anthropic.claude-opus-4-8").unwrap();
    model.base_url = url.to_string();
    model
}

fn hello() -> TranscriptContext {
    normalize_context(&Context { system_prompt: None, tools: None, messages: vec![Message::User(UserMessage::new("hello", 1))] })
}

#[tokio::test]
async fn streams_events_and_sends_a_signed_converse_request() {
    let server = MockServer::sequence(vec![eventstream(items(&[
        json!({ "messageStart": { "role": "assistant" } }),
        json!({ "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "text": "hel" } } }),
        json!({ "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "text": "lo" } } }),
        json!({ "contentBlockStop": { "contentBlockIndex": 0 } }),
        json!({ "messageStop": { "stopReason": "end_turn", "additionalModelResponseFields": { "source": "test" } } }),
        json!({ "metadata": { "usage": { "inputTokens": 10, "outputTokens": 2, "totalTokens": 12 }, "metrics": { "latencyMs": 1 } } }),
    ]))])
    .await;
    let mut model = opus_at(&server.url);
    model.id = "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123".into();
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = received.clone();
    let mut headers = ProviderHeaders::new();
    for (name, value) in [("x-custom", "v"), ("authorization", "evil"), ("X-Amz-Date", "evil"), ("HOST", "evil"), ("x-allowed", "ok")] {
        headers.insert(name.into(), Some(value.into()));
    }
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::None),
        env: Some(env(&[("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"), ("AWS_SECRET_ACCESS_KEY", "secret"), ("AWS_SESSION_TOKEN", "session")])),
        headers: Some(headers),
        on_provider_stream_event: Some(Arc::new(move |item: Value, _: Model| { sink.lock().push(item); Box::pin(async {}) })),
        ..Default::default()
    };
    let (events, message) = bedrock_converse_stream::stream(&model, &hello(), options).collect_all().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(message.text(), "hello");
    assert_eq!((message.usage.input, message.usage.output, message.usage.total_tokens), (10, 2, 12));
    assert_eq!(received.lock().len(), 6);
    assert_eq!(received.lock()[4], json!({ "messageStop": { "stopReason": "end_turn", "additionalModelResponseFields": { "source": "test" } } }));
    assert_eq!(events.iter().map(|e| e.type_name()).collect::<Vec<_>>(), ["start", "text_start", "text_delta", "text_delta", "text_end", "done"]);

    let request = server.request(0);
    assert_eq!(
        request.path,
        "/model/arn%3Aaws%3Abedrock%3Aus-west-2%3A123456789012%3Aapplication-inference-profile%2Fabc123/converse-stream"
    );
    let authorization = request.header("authorization").unwrap();
    // The region comes from the inference profile ARN.
    assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"), "{authorization}");
    assert!(authorization.contains("/us-west-2/bedrock/aws4_request, SignedHeaders="));
    assert!(authorization.contains("x-amz-security-token"));
    assert!(authorization.contains("x-custom"));
    assert_eq!(request.header("x-amz-security-token"), Some("session"));
    assert_eq!(request.header("x-custom"), Some("v"));
    assert_eq!(request.header("x-allowed"), Some("ok"));
    assert_ne!(request.header("x-amz-date"), Some("evil"));
    assert_eq!(request.header_count("authorization"), 1);
    assert!(request.header("user-agent").unwrap().starts_with("aws-sdk-js/"));
    assert!(request.header("user-agent").unwrap().ends_with(&format!("pi/{PI_AI_VERSION}")));
    assert_eq!(request.header("amz-sdk-request"), Some("attempt=1; max=3"));
    let body = request.json();
    assert!(body.get("modelId").is_none());
    assert_eq!(body["messages"], json!([{ "role": "user", "content": [{ "text": "hello" }] }]));
    assert_eq!(body["inferenceConfig"], json!({ "maxTokens": model.max_tokens }));
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["messages", "inferenceConfig"]);
}

#[tokio::test]
async fn bearer_tokens_skip_sigv4() {
    let server = MockServer::sequence(vec![eventstream(items(&[
        json!({ "messageStart": { "role": "assistant" } }),
        json!({ "messageStop": { "stopReason": "end_turn" } }),
    ]))])
    .await;
    let model = opus_at(&server.url);
    let options = StreamOptions { api_key: Some("bedrock-api-key".into()), env: Some(env(&[("AWS_REGION", "us-east-1")])), ..Default::default() };
    let message = bedrock_converse_stream::stream(&model, &hello(), options).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = server.request(0);
    assert_eq!(request.header("authorization"), Some("Bearer bedrock-api-key"));
    assert!(request.header("x-amz-date").is_none());
}

#[tokio::test]
async fn forwards_raw_response_headers_and_fails_empty_streams() {
    let server = MockServer::sequence(vec![Response::new(200, "application/vnd.amazon.eventstream", Vec::new())
        .with_header("x-bifrost-provider", "bedrock")
        .with_header("x-amzn-requestid", "req-123")])
    .await;
    let model = opus_at(&server.url);
    let responses = Arc::new(Mutex::new(Vec::<ProviderResponse>::new()));
    let sink = responses.clone();
    let options = StreamOptions {
        on_response: Some(Arc::new(move |r: ProviderResponse, _: Model| {
            sink.lock().push(r);
            Box::pin(async {})
        })),
        ..skip_auth_options()
    };
    let message = bedrock_converse_stream::stream(&model, &hello(), options).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Bedrock stream ended without a stop reason"));
    let responses = responses.lock();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].status, 200);
    assert_eq!(responses[0].headers.get("x-amzn-requestid").map(String::as_str), Some("req-123"));
    assert_eq!(responses[0].headers.get("x-bifrost-provider").map(String::as_str), Some("bedrock"));
}

#[tokio::test]
async fn raw_stop_reasons_tool_arguments_and_cache_costs() {
    for (reason, expected, error) in [
        ("end_turn", StopReason::Stop, None),
        ("max_tokens", StopReason::Length, None),
        ("model_context_window_exceeded", StopReason::Length, None),
        ("tool_use", StopReason::ToolUse, None),
        ("guardrail_intervened", StopReason::Error, Some("Provider stopped with: guardrail_intervened")),
    ] {
        let server = MockServer::sequence(vec![eventstream(items(&[
            json!({ "messageStart": { "role": "assistant" } }),
            json!({ "messageStop": { "stopReason": reason } }),
        ]))])
        .await;
        let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
        assert_eq!(message.stop_reason, expected, "{reason}");
        assert_eq!(message.raw_stop_reason.as_deref(), Some(reason));
        assert_eq!(message.error_message.as_deref(), error);
    }

    let input = r#"{"path":"/workspace/foobar/file.js","edits":[{"oldText":"first","newText":"updated first"},{"oldText":"second","newText":"updated second","":""}]}"#;
    let server = MockServer::sequence(vec![eventstream(items(&[
        json!({ "messageStart": { "role": "assistant" } }),
        json!({ "contentBlockStart": { "contentBlockIndex": 0, "start": { "toolUse": { "toolUseId": "tool-1", "name": "edit" } } } }),
        json!({ "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "toolUse": { "input": input } } } }),
        json!({ "contentBlockStop": { "contentBlockIndex": 0 } }),
        json!({ "messageStop": { "stopReason": "tool_use" } }),
    ]))])
    .await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(
        serde_json::to_value(&message.content[0]).unwrap(),
        json!({ "type": "toolCall", "id": "tool-1", "name": "edit", "arguments": serde_json::from_str::<Value>(input).unwrap() })
    );

    let server = MockServer::sequence(vec![eventstream(items(&[
        json!({ "messageStart": { "role": "assistant" } }),
        json!({ "metadata": { "usage": { "inputTokens": 100, "outputTokens": 5, "totalTokens": 1000105, "cacheWriteInputTokens": 1000000,
            "cacheDetails": [{ "ttl": "1h", "inputTokens": 150000 }, { "ttl": "5m", "inputTokens": 600000 }, { "ttl": "1h", "inputTokens": 250000 }] } } }),
        json!({ "messageStop": { "stopReason": "end_turn" } }),
    ]))])
    .await;
    let model = opus_at(&server.url);
    let message = bedrock_converse_stream::stream(&model, &hello(), skip_auth_options()).result().await;
    assert_eq!(message.usage.cache_write, 1_000_000);
    assert_eq!(message.usage.cache_write_1h, Some(400_000));
    let expected = (600_000.0 * model.cost.cache_write + 400_000.0 * model.cost.input * 2.0) / 1_000_000.0;
    assert!((message.usage.cost.cache_write - expected).abs() < 1e-9);
}

#[tokio::test]
async fn redacted_reasoning_is_preserved_and_replayed() {
    let bytes: Vec<u8> = (0u8..40).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let (head, tail) = bytes.split_at(7);
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut gpt = get_builtin_model("amazon-bedrock", "global.openai.gpt-6-sol").unwrap();

    let server = MockServer::sequence(vec![
        eventstream(items(&[
            json!({ "messageStart": { "role": "assistant" } }),
            json!({ "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "reasoningContent": { "redactedContent": b64(head) } } } }),
            json!({ "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "reasoningContent": { "redactedContent": b64(tail) } } } }),
            json!({ "contentBlockStop": { "contentBlockIndex": 0 } }),
            json!({ "contentBlockDelta": { "contentBlockIndex": 1, "delta": { "text": "done" } } }),
            json!({ "contentBlockStop": { "contentBlockIndex": 1 } }),
            json!({ "messageStop": { "stopReason": "end_turn" } }),
        ])),
        eventstream(items(&[
            json!({ "messageStart": { "role": "assistant" } }),
            json!({ "contentBlockDelta": { "contentBlockIndex": 0, "delta": { "reasoningContent": { "redactedContent": encoded } } } }),
            json!({ "messageStop": { "stopReason": "end_turn" } }),
        ])),
    ])
    .await;
    gpt.base_url = server.url.clone();
    let message = bedrock_converse_stream::stream(&gpt, &hello(), skip_auth_options()).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(message.content.iter().map(|c| c.type_name()).collect::<Vec<_>>(), ["thinking", "text"]);
    let AssistantContent::Thinking(thinking) = &message.content[0] else { panic!() };
    assert_eq!(thinking.redacted, Some(true));
    assert_eq!(thinking.thinking, "[Reasoning redacted]");
    assert_eq!(thinking.thinking_signature.as_deref(), Some(encoded.as_str()));

    // Without contentBlockStop the payload is still encoded.
    let message = bedrock_converse_stream::stream(&gpt, &hello(), skip_auth_options()).result().await;
    let AssistantContent::Thinking(thinking) = &message.content[0] else { panic!() };
    assert_eq!(thinking.thinking_signature.as_deref(), Some(encoded.as_str()));

    // Replay as reasoningContent.redactedContent ahead of the matching toolUse.
    let history = vec![
        Message::User(UserMessage::new("read the file", 1)),
        Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent { thinking: String::new(), thinking_signature: Some(encoded.clone()), redacted: Some(true), ..Default::default() }),
                AssistantContent::ToolCall(ToolCall { id: "tool-1".into(), name: "read".into(), arguments: json!({ "path": "/tmp/a.txt" }).as_object().cloned().unwrap(), ..Default::default() }),
            ],
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            model: gpt.id.clone(),
            stop_reason: StopReason::ToolUse,
            timestamp: 1,
            ..Default::default()
        }),
        Message::ToolResult(ToolResultMessage { tool_call_id: "tool-1".into(), tool_name: "read".into(), content: vec![UserContentBlock::text("file body")], timestamp: 1, ..Default::default() }),
    ];
    let payload = capture_payload(&gpt, history, StreamOptions::default()).await;
    let assistant = payload["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone();
    assert_eq!(
        assistant["content"],
        json!([{ "reasoningContent": { "redactedContent": encoded } }, { "toolUse": { "toolUseId": "tool-1", "name": "read", "input": { "path": "/tmp/a.txt" } } }])
    );
}

// ---------------------------------------------------------------------------
// Errors and diagnostics (bedrock-error-metadata.test.ts)
// ---------------------------------------------------------------------------

fn diagnostic(message: &AssistantMessage) -> Option<Value> {
    message
        .diagnostics
        .as_ref()?
        .iter()
        .find(|d| d.diagnostic_type == "bedrock_response_failure")
        .map(|d| serde_json::to_value(d).unwrap())
}

#[tokio::test]
async fn failure_diagnostics() {
    const REQUEST_ID: &str = "11111111-2222-3333-4444-555555555555";
    let validation = || {
        Response::json(400, json!({ "message": "The provided model identifier is invalid." }))
            .with_header("x-amzn-errortype", "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/")
            .with_header("x-amzn-requestid", REQUEST_ID)
    };
    let server = MockServer::sequence(vec![validation()]).await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Validation error: The provided model identifier is invalid."));
    let first = diagnostic(&message).unwrap();
    assert_eq!(first["details"], json!({ "status": 400, "errorCode": "ValidationException", "requestId": REQUEST_ID }));
    let mut keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["details", "timestamp", "type"]);

    // A modeled mid-stream exception: the SDK throws the bare shape.
    let exception = encode_message(
        &[(":message-type", HeaderValue::String("exception".into())), (":exception-type", HeaderValue::String("throttlingException".into()))],
        br#"{"message":"Too many requests, please wait."}"#,
    );
    let mut body = event("messageStart", json!({ "role": "assistant" }));
    body.extend(exception);
    let server = MockServer::sequence(vec![Response::new(200, "application/vnd.amazon.eventstream", body).with_header("x-amzn-requestid", REQUEST_ID)]).await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(message.error_message.as_deref(), Some(r#"{"message":"Too many requests, please wait."}"#));
    assert_eq!(diagnostic(&message).unwrap()["details"], json!({ "requestId": REQUEST_ID }));

    // An unmodeled stream error is a named Error.
    let error_frame = encode_message(
        &[
            (":message-type", HeaderValue::String("error".into())),
            (":error-code", HeaderValue::String("ModelStreamErrorException".into())),
            (":error-message", HeaderValue::String("Model stream terminated unexpectedly.".into())),
        ],
        b"",
    );
    let mut body = event("messageStart", json!({ "role": "assistant" }));
    body.extend(error_frame);
    let server = MockServer::sequence(vec![Response::new(200, "application/vnd.amazon.eventstream", body).with_header("x-amzn-requestid", REQUEST_ID)]).await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(message.error_message.as_deref(), Some("Model stream terminated unexpectedly."));
    assert_eq!(diagnostic(&message).unwrap()["details"], json!({ "errorCode": "ModelStreamErrorException", "requestId": REQUEST_ID }));

    // A gateway response without an error type keeps its body and omits the Unknown placeholder.
    let server = MockServer::sequence(vec![Response::text(403, "<html>blocked by gateway</html>").with_header("x-amzn-requestid", REQUEST_ID)]).await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(message.error_message.as_deref(), Some("Unknown: 403: <html>blocked by gateway</html>"));
    assert_eq!(diagnostic(&message).unwrap()["details"], json!({ "status": 403, "requestId": REQUEST_ID }));

    // Over-long header values are dropped.
    let server = MockServer::sequence(vec![Response::json(400, json!({ "message": "x" }))
        .with_header("x-amzn-errortype", &format!("{}Exception", "E".repeat(300)))
        .with_header("x-amzn-requestid", &"R".repeat(300))])
    .await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(diagnostic(&message).unwrap()["details"], json!({ "status": 400 }));

    // Transport failures carry no metadata.
    let message = bedrock_converse_stream::stream(&opus_at("http://127.0.0.1:9"), &hello(), StreamOptions { env: Some(env(&[("AWS_BEDROCK_SKIP_AUTH", "1"), ("AWS_REGION", "us-east-1"), ("AWS_MAX_ATTEMPTS", "1")])), ..skip_auth_options() }).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(diagnostic(&message).is_none());

    // Aborted turns get no diagnostic.
    let server = MockServer::sequence(vec![validation()]).await;
    let signal = CancellationToken::new();
    signal.cancel();
    let options = StreamOptions { signal: Some(signal), ..skip_auth_options() };
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), options).result().await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert!(diagnostic(&message).is_none());
}

#[tokio::test]
async fn retries_throttling_with_sdk_attempt_headers() {
    let server = MockServer::sequence(vec![
        Response::json(429, json!({ "message": "slow down" })).with_header("x-amzn-errortype", "ThrottlingException"),
        eventstream(items(&[json!({ "messageStart": { "role": "assistant" } }), json!({ "messageStop": { "stopReason": "end_turn" } })])),
    ])
    .await;
    let message = bedrock_converse_stream::stream(&opus_at(&server.url), &hello(), skip_auth_options()).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].header("amz-sdk-request"), Some("attempt=2; max=3"));
    assert_eq!(requests[0].header("amz-sdk-invocation-id"), requests[1].header("amz-sdk-invocation-id"));
}

// ---------------------------------------------------------------------------
// Payloads (bedrock-convert-messages / bedrock-thinking-payload)
// ---------------------------------------------------------------------------

async fn capture_payload(model: &Model, messages: Vec<Message>, mut options: StreamOptions) -> Value {
    capture_context(model, normalize_context(&Context { system_prompt: None, tools: None, messages }), &mut options).await
}

async fn capture_context(model: &Model, context: TranscriptContext, options: &mut StreamOptions) -> Value {
    let captured = Arc::new(Mutex::new(None::<Value>));
    let sink = captured.clone();
    let signal = CancellationToken::new();
    signal.cancel();
    options.signal = Some(signal);
    if options.cache_retention.is_none() {
        options.cache_retention = Some(CacheRetention::None);
    }
    options.on_payload = Some(Arc::new(move |payload: Value, _: Model| {
        *sink.lock() = Some(payload);
        Box::pin(async { None })
    }));
    let message = bedrock_converse_stream::stream(model, &context, options.clone()).result().await;
    captured.lock().clone().unwrap_or_else(|| panic!("payload not captured: {:?}", message.error_message))
}

fn sonnet_45() -> Model {
    let mut model = get_builtin_model("amazon-bedrock", "us.anthropic.claude-sonnet-4-5-20250929-v1:0").unwrap();
    model.compat = Some(serde_json::from_value(json!({ "supportsStrictMode": true })).unwrap());
    model
}

fn nova() -> Model {
    let mut model = sonnet_45();
    model.id = "amazon.nova-lite-v1:0".into();
    model.name = "Nova Lite".into();
    model.reasoning = false;
    model.compat = None;
    model
}

fn assistant_with(model: &Model, content: Vec<AssistantContent>, stop: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: "bedrock-converse-stream".into(),
        provider: "amazon-bedrock".into(),
        model: model.id.clone(),
        stop_reason: stop,
        timestamp: 1,
        ..Default::default()
    })
}

#[tokio::test]
async fn converts_messages_with_placeholders_and_error_images() {
    let model = sonnet_45();
    let payload = capture_payload(&model, vec![Message::User(UserMessage::new("   ", 1))], StreamOptions::default()).await;
    assert_eq!(payload["messages"], json!([{ "role": "user", "content": [{ "text": "<empty>" }] }]));

    let payload = capture_payload(
        &model,
        vec![Message::User(UserMessage::new(vec![UserContentBlock::text(""), UserContentBlock::text("hello")], 1))],
        StreamOptions::default(),
    )
    .await;
    assert_eq!(payload["messages"][0]["content"], json!([{ "text": "hello" }]));

    let payload = capture_payload(
        &model,
        vec![Message::ToolResult(ToolResultMessage { tool_call_id: "tool-1".into(), tool_name: "tool".into(), content: vec![UserContentBlock::text("")], timestamp: 1, ..Default::default() })],
        StreamOptions::default(),
    )
    .await;
    assert_eq!(payload["messages"][0]["content"][0]["toolResult"]["content"], json!([{ "text": "<empty>" }]));

    let image = |data: &str| UserContentBlock::Image(ImageContent { data: data.into(), mime_type: "image/png".into() });
    let history = vec![
        assistant_with(&model, vec![
            AssistantContent::ToolCall(ToolCall { id: "call-1".into(), name: "codemode".into(), ..Default::default() }),
            AssistantContent::ToolCall(ToolCall { id: "call-2".into(), name: "read".into(), ..Default::default() }),
        ], StopReason::ToolUse),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "codemode".into(),
            content: vec![UserContentBlock::text("Script failed"), image("AAAA"), UserContentBlock::text("Script error")],
            is_error: true,
            timestamp: 1,
            ..Default::default()
        }),
        Message::ToolResult(ToolResultMessage { tool_call_id: "call-2".into(), tool_name: "read".into(), content: vec![image("BBBB")], timestamp: 1, ..Default::default() }),
    ];
    let payload = capture_payload(&model, history, StreamOptions::default()).await;
    let content = payload["messages"][1]["content"].as_array().unwrap().clone();
    assert_eq!(content.len(), 4);
    assert_eq!(
        content[0]["toolResult"],
        json!({ "toolUseId": "call-1", "content": [{ "text": "Script failed" }, { "text": "(image attached after the tool results)" }, { "text": "Script error" }], "status": "error" })
    );
    assert_eq!(content[1]["toolResult"]["status"], "success");
    assert_eq!(content[1]["toolResult"]["content"], json!([{ "image": { "source": { "bytes": "BBBB" }, "format": "png" } }]));
    assert_eq!(content[2], json!({ "text": "Images from the error result of tool call call-1 (codemode):" }));
    assert_eq!(content[3]["image"]["format"], "png");

    let payload = capture_payload(
        &nova(),
        vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "codemode".into(),
            content: vec![UserContentBlock::text("Script failed"), image("AAAA")],
            is_error: true,
            timestamp: 1,
            ..Default::default()
        })],
        StreamOptions::default(),
    )
    .await;
    assert_eq!(
        payload["messages"][0]["content"],
        json!([{ "toolResult": { "toolUseId": "call-1", "content": [{ "text": "Script failed" }, { "image": { "source": { "bytes": "AAAA" }, "format": "png" } }], "status": "error" } }])
    );

    let arguments = json!({ "path": "/workspace/foobar/file.js", "edits": [{ "oldText": "first", "newText": "updated first" }, { "oldText": "second", "newText": "updated second", "": "" }] });
    let history = vec![
        assistant_with(&model, vec![AssistantContent::ToolCall(ToolCall { id: "tool-1".into(), name: "edit".into(), arguments: arguments.as_object().cloned().unwrap(), ..Default::default() })], StopReason::ToolUse),
        Message::ToolResult(ToolResultMessage { tool_call_id: "tool-1".into(), tool_name: "edit".into(), content: vec![UserContentBlock::text("done")], timestamp: 1, ..Default::default() }),
    ];
    let payload = capture_payload(&model, history, StreamOptions::default()).await;
    assert_eq!(
        payload["messages"][0]["content"][0]["toolUse"]["input"],
        json!({ "path": "/workspace/foobar/file.js", "edits": [{ "oldText": "first", "newText": "updated first" }, { "oldText": "second", "newText": "updated second" }] })
    );
}

#[tokio::test]
async fn strict_tools_are_gated_by_model_capability() {
    let mut tool = Tool::new("lookup", "Look up a value", json!({ "type": "object", "properties": { "value": { "type": "string" } }, "required": ["value"] }));
    tool.constrained_sampling = Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict: JsonSchemaStrictness::Require }));
    let context = |tool: Tool| normalize_context(&Context { system_prompt: None, tools: Some(vec![tool]), messages: vec![Message::User(UserMessage::new("Use the tool", 1))] });
    let payload = capture_context(&sonnet_45(), context(tool.clone()), &mut StreamOptions::default()).await;
    assert_eq!(payload["toolConfig"]["tools"][0]["toolSpec"]["strict"], true);
    tool.constrained_sampling = Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict: JsonSchemaStrictness::Prefer }));
    let payload = capture_context(&nova(), context(tool), &mut StreamOptions::default()).await;
    assert!(payload["toolConfig"]["tools"][0]["toolSpec"].get("strict").is_none());
}

fn reasoning(level: ThinkingLevel) -> StreamOptions {
    StreamOptions { reasoning: Some(level), ..Default::default() }
}

#[tokio::test]
async fn thinking_payloads() {
    let binding = json!({ "type": "adaptive", "display": "summarized", "block_binding": { "prefix_mismatch_behavior": "drop_block" } });
    let hello_messages = || vec![Message::User(UserMessage::new("Hello", 1))];
    let mut opus_48 = get_builtin_model("amazon-bedrock", "global.anthropic.claude-opus-4-6-v1").unwrap();
    opus_48.id = "global.anthropic.claude-opus-4-8-v1".into();
    opus_48.name = "Claude Opus 4.8 (Global)".into();
    for (model, level, effort) in [
        (opus_48.clone(), ThinkingLevel::High, "high"),
        (opus_48.clone(), ThinkingLevel::Xhigh, "xhigh"),
        (get_builtin_model("amazon-bedrock", "global.anthropic.claude-fable-5").unwrap(), ThinkingLevel::High, "high"),
        (get_builtin_model("amazon-bedrock", "global.anthropic.claude-sonnet-5").unwrap(), ThinkingLevel::High, "high"),
        (get_builtin_model("amazon-bedrock", "global.anthropic.claude-opus-5").unwrap(), ThinkingLevel::Xhigh, "xhigh"),
        (get_builtin_model("amazon-bedrock", "global.anthropic.claude-opus-5-5").unwrap(), ThinkingLevel::High, "high"),
    ] {
        let payload = capture_payload(&model, hello_messages(), reasoning(level)).await;
        let fields = &payload["additionalModelRequestFields"];
        assert_eq!(fields["thinking"], binding, "{}", model.id);
        assert_eq!(fields["output_config"], json!({ "effort": effort }), "{}", model.id);
        assert_eq!(fields["anthropic_beta"], json!(["thinking-binding-controls-2026-08-01"]));
    }
    for id in ["global.anthropic.claude-opus-4-6-v1", "global.anthropic.claude-sonnet-4-6"] {
        let payload = capture_payload(&get_builtin_model("amazon-bedrock", id).unwrap(), hello_messages(), reasoning(ThinkingLevel::High)).await;
        assert_eq!(payload["additionalModelRequestFields"]["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
        assert!(payload["additionalModelRequestFields"].get("anthropic_beta").is_none());
    }
    let mut gov = get_builtin_model("amazon-bedrock", "us.anthropic.claude-sonnet-4-5-20250929-v1:0").unwrap();
    gov.id = "us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0".into();
    gov.name = "Claude Sonnet 4.5 (GovCloud)".into();
    let payload = capture_payload(&gov, hello_messages(), reasoning(ThinkingLevel::High)).await;
    assert_eq!(payload["additionalModelRequestFields"]["thinking"], json!({ "type": "enabled", "budget_tokens": 16384 }));
    assert_eq!(payload["additionalModelRequestFields"]["anthropic_beta"], json!(["interleaved-thinking-2025-05-14"]));

    let options = reasoning(ThinkingLevel::High).with_extra("region", "us-gov-west-1");
    let payload = capture_payload(&opus_48, hello_messages(), options).await;
    assert_eq!(payload["additionalModelRequestFields"]["thinking"], json!({ "type": "adaptive" }));
    assert_eq!(payload["additionalModelRequestFields"]["output_config"], json!({ "effort": "high" }));
    assert!(payload["additionalModelRequestFields"].get("anthropic_beta").is_none());

    for (level, effort) in [
        (ThinkingLevel::Minimal, "low"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Xhigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ] {
        for id in ["global.openai.gpt-6-sol", "us.openai.gpt-6-luna", "global.openai.gpt-5.6-sol"] {
            let payload = capture_payload(&get_builtin_model("amazon-bedrock", id).unwrap(), hello_messages(), reasoning(level)).await;
            assert_eq!(payload["additionalModelRequestFields"], json!({ "reasoning": { "effort": effort } }), "{id} {effort}");
        }
    }
    let mut profile = get_builtin_model("amazon-bedrock", "global.openai.gpt-6-sol").unwrap();
    profile.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile".into();
    profile.name = "GPT-6 Sol".into();
    let payload = capture_payload(&profile, hello_messages(), reasoning(ThinkingLevel::Medium)).await;
    assert_eq!(payload["additionalModelRequestFields"], json!({ "reasoning": { "effort": "medium" } }));

    let oss = get_builtin_model("amazon-bedrock", "openai.gpt-oss-120b-1:0").unwrap();
    for (level, effort) in [(ThinkingLevel::Minimal, "low"), (ThinkingLevel::Medium, "medium"), (ThinkingLevel::Xhigh, "high")] {
        let payload = capture_payload(&oss, hello_messages(), reasoning(level)).await;
        assert_eq!(payload["additionalModelRequestFields"], json!({ "reasoning_effort": effort }));
    }
    let payload = capture_payload(&get_builtin_model("amazon-bedrock", "global.openai.gpt-6-sol").unwrap(), hello_messages(), StreamOptions::default()).await;
    assert!(payload.get("additionalModelRequestFields").is_none());

    let mut app_profile = get_builtin_model("amazon-bedrock", "global.anthropic.claude-opus-4-6-v1").unwrap();
    app_profile.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile".into();
    app_profile.name = "Claude Opus 4.6".into();
    let payload = capture_payload(&app_profile, hello_messages(), reasoning(ThinkingLevel::High)).await;
    assert_eq!(payload["additionalModelRequestFields"]["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
    assert_eq!(payload["additionalModelRequestFields"]["output_config"], json!({ "effort": "high" }));

    // Cache points for application inference profiles named after a supported Claude model.
    app_profile.name = "Claude Sonnet 4.6".into();
    let context = normalize_context(&Context { system_prompt: Some("You are helpful.".into()), tools: None, messages: hello_messages() });
    let mut options = StreamOptions { cache_retention: Some(CacheRetention::Short), ..Default::default() };
    let payload = capture_context(&app_profile, context, &mut options).await;
    assert_eq!(payload["system"], json!([{ "text": "You are helpful." }, { "cachePoint": { "type": "default" } }]));
    assert_eq!(payload["messages"][0]["content"][1], json!({ "cachePoint": { "type": "default" } }));
}

#[tokio::test]
async fn simple_stream_budgets_for_budget_based_claude() {
    let model = get_builtin_model("amazon-bedrock", "us.anthropic.claude-sonnet-4-5-20250929-v1:0").unwrap();
    let captured = Arc::new(Mutex::new(None::<Value>));
    let sink = captured.clone();
    let signal = CancellationToken::new();
    signal.cancel();
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Medium),
        max_tokens: Some(4000),
        signal: Some(signal),
        cache_retention: Some(CacheRetention::None),
        on_payload: Some(Arc::new(move |payload: Value, _: Model| {
            *sink.lock() = Some(payload);
            Box::pin(async { None })
        })),
        ..Default::default()
    };
    bedrock_converse_stream::stream_simple(&model, &hello(), options).result().await;
    let payload = captured.lock().clone().unwrap();
    assert_eq!(payload["inferenceConfig"]["maxTokens"], 12192);
    assert_eq!(payload["additionalModelRequestFields"]["thinking"]["budget_tokens"], 8192);
}

// ---------------------------------------------------------------------------
// Client configuration (bedrock-endpoint-resolution / bedrock-credentials)
// ---------------------------------------------------------------------------

#[test]
fn client_configuration() {
    if std::env::var_os("AWS_PROFILE").is_some() || std::env::var_os("AWS_REGION").is_some() || std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return; // The ambient AWS environment changes the expectations below.
    }
    let eu = get_builtin_model("amazon-bedrock", "eu.anthropic.claude-sonnet-4-5-20250929-v1:0").unwrap();
    assert_eq!(eu.base_url, "https://bedrock-runtime.eu-central-1.amazonaws.com");
    let us = get_builtin_model("amazon-bedrock", "us.anthropic.claude-opus-4-8").unwrap();

    let with_env = |pairs: &[(&str, &str)]| StreamOptions { env: Some(env(pairs)), ..Default::default() };
    let config = resolve_client_config(&us, &with_env(&[("AWS_REGION", "us-east-2")]));
    assert_eq!(config.region.as_deref(), Some("us-east-2"));
    assert_eq!(config.endpoint, None);

    let config = resolve_client_config(&eu, &StreamOptions::default());
    assert_eq!(config.endpoint.as_deref(), Some("https://bedrock-runtime.eu-central-1.amazonaws.com"));
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));

    let config = resolve_client_config(&eu, &StreamOptions::default().with_extra("profile", "bedrock-profile"));
    assert_eq!(config.profile.as_deref(), Some("bedrock-profile"));
    assert_eq!(config.endpoint.as_deref(), Some("https://bedrock-runtime.eu-central-1.amazonaws.com"));
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));
    let config = resolve_client_config(&eu, &with_env(&[("AWS_PROFILE", "scoped-bedrock-profile")]));
    assert_eq!(config.profile.as_deref(), Some("scoped-bedrock-profile"));
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));

    let mut custom = us.clone();
    custom.base_url = "https://bedrock-vpc.example.com".into();
    let config = resolve_client_config(&custom, &with_env(&[("AWS_REGION", "us-west-2")]));
    assert_eq!(config.endpoint.as_deref(), Some("https://bedrock-vpc.example.com"));
    assert_eq!(config.region.as_deref(), Some("us-west-2"));

    let mut arn = us.clone();
    arn.id = "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123".into();
    assert_eq!(resolve_client_config(&arn, &with_env(&[("AWS_REGION", "us-east-1")])).region.as_deref(), Some("us-west-2"));
    arn.id = "arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:application-inference-profile/abc123".into();
    assert_eq!(resolve_client_config(&arn, &with_env(&[("AWS_REGION", "us-east-1")])).region.as_deref(), Some("us-gov-west-1"));

    let config = resolve_client_config(&us, &StreamOptions { api_key: Some("bedrock-api-key".into()), ..Default::default() });
    assert_eq!(config.bearer_token.as_deref(), Some("bedrock-api-key"));

    // Explicit and scoped profiles win over access keys; otherwise keys are used.
    let keys = [("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"), ("AWS_SECRET_ACCESS_KEY", "secretexample")];
    let config = resolve_client_config(&us, &with_env(&keys).with_extra("profile", "explicit-profile"));
    assert_eq!(config.profile.as_deref(), Some("explicit-profile"));
    assert!(config.credentials.is_none());
    let mut scoped = keys.to_vec();
    scoped.push(("AWS_PROFILE", "scoped-profile"));
    let config = resolve_client_config(&us, &with_env(&scoped));
    assert_eq!(config.profile.as_deref(), Some("scoped-profile"));
    assert!(config.credentials.is_none());
    let config = resolve_client_config(&us, &with_env(&keys));
    assert!(config.profile.is_none());
    let credentials = config.credentials.unwrap();
    assert_eq!((credentials.access_key_id.as_str(), credentials.secret_access_key.as_str()), ("AKIAEXAMPLE", "secretexample"));
    assert!(credentials.session_token.is_none());
}
