//! Offline tests for the Google Generative AI and Vertex APIs (ports of `google-*.test.ts`).

mod mock_http;

use std::sync::Arc;

use mock_http::{MockServer, Response};
use parking_lot::Mutex;
use pi_ai::api::google_shared::{
    convert_messages, convert_tools, is_thinking_part, requires_tool_call_id, resolve_google_function_calling_mode,
    resolve_google_thinking_level, retain_thought_signature, supports_google_strict_tool_sampling, GoogleClient,
};
use pi_ai::api::{google_auth, google_generative_ai, google_vertex};
use pi_ai::providers::all::get_builtin_model;
use pi_ai::*;
use serde_json::{Value, json};

fn context(text: &str) -> TranscriptContext {
    normalize_context(&Context { system_prompt: None, messages: vec![Message::User(UserMessage::new(text, 1))], tools: None })
}

fn sse(chunks: &[Value]) -> String {
    chunks.iter().map(|c| format!("data: {c}\r\n\r\n")).collect()
}

fn usage_chunk(reason: &str) -> Value {
    json!({ "candidates": [{ "finishReason": reason }], "usageMetadata": { "promptTokenCount": 2, "candidatesTokenCount": 1, "totalTokenCount": 3 } })
}

fn gemini(base_url: &str, id: &str) -> Model {
    let mut model = get_builtin_model("google", id).unwrap();
    model.base_url = format!("{base_url}/v1beta");
    model
}

fn vertex(base_url: &str, id: &str) -> Model {
    let mut model = get_builtin_model("google-vertex", id).unwrap();
    model.base_url = base_url.to_string();
    model
}

fn key_options(key: &str) -> StreamOptions {
    StreamOptions { api_key: Some(key.into()), ..Default::default() }
}

fn tool_call_chunk(reason: &str) -> Value {
    json!({
        "responseId": "google-response-id",
        "candidates": [{ "finishReason": reason, "content": { "parts": [{ "functionCall": { "id": "call-1", "name": "echo", "args": { "value": "truncated" } } }] } }],
        "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 0, "totalTokenCount": 1 }
    })
}

#[tokio::test]
async fn streams_gemini_text_with_sdk_shaped_request() {
    let server = MockServer::sequence(vec![Response::sse(&sse(&[
        json!({ "responseId": "resp_google", "candidates": [{ "content": { "parts": [{ "text": "hel" }] } }] }),
        json!({ "candidates": [{ "content": { "parts": [{ "text": "lo" }] } }] }),
        usage_chunk("STOP"),
    ]))])
    .await;
    let model = gemini(&server.url, "gemini-2.5-flash");
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = received.clone();
    let options = StreamOptions {
        api_key: Some("test-api-key".into()),
        on_provider_stream_event: Some(Arc::new(move |chunk: Value, _: Model| { sink.lock().push(chunk); Box::pin(async {}) })),
        ..Default::default()
    };
    let (events, result) = google_generative_ai::stream(&model, &context("hello"), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.response_id.as_deref(), Some("resp_google"));
    assert_eq!(result.content, vec![AssistantContent::Text(TextContent::new("hello"))]);
    assert_eq!((result.usage.input, result.usage.output, result.usage.total_tokens), (2, 1, 3));
    assert_eq!(received.lock().len(), 3);
    assert_eq!(
        events.iter().map(|e| e.type_name()).collect::<Vec<_>>(),
        ["start", "text_start", "text_delta", "text_delta", "text_end", "done"]
    );

    let request = server.request(0);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse");
    assert_eq!(request.header("x-goog-api-key"), Some("test-api-key"));
    assert_eq!(request.header("user-agent"), Some(format_user_agent(None).as_str()));
    assert!(request.header("x-goog-api-client").unwrap().starts_with("google-genai-sdk/"));
    let body = request.json();
    assert_eq!(body["contents"], json!([{ "parts": [{ "text": "hello" }], "role": "user" }]));
    assert_eq!(body["generationConfig"], json!({}));
}

#[tokio::test]
async fn explicit_headers_override_the_default_user_agent() {
    let server = MockServer::sequence(vec![Response::sse(&sse(&[usage_chunk("STOP")]))]).await;
    let model = gemini(&server.url, "gemini-2.5-flash");
    let mut headers = ProviderHeaders::new();
    headers.insert("User-Agent".into(), Some("custom-agent".into()));
    let options = StreamOptions { api_key: Some("k".into()), headers: Some(headers), ..Default::default() };
    google_generative_ai::stream(&model, &context("hello"), options).result().await;
    assert_eq!(server.request(0).header("user-agent"), Some("custom-agent"));
}

#[tokio::test]
async fn preserves_raw_finish_reasons_and_maps_tool_calls() {
    let server = MockServer::start(|request, _| {
        let reason = match request.json()["contents"][0]["parts"][0]["text"].as_str() {
            Some("malformed") => json!({ "responseId": "r", "candidates": [{ "finishReason": "MALFORMED_FUNCTION_CALL" }] }),
            Some("length") => tool_call_chunk("MAX_TOKENS"),
            _ => tool_call_chunk("STOP"),
        };
        Response::sse(&sse(&[reason]))
    })
    .await;
    let model = gemini(&server.url, "gemini-2.5-flash");

    let message = google_generative_ai::stream(&model, &context("malformed"), key_options("k")).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("MALFORMED_FUNCTION_CALL"));
    assert_eq!(message.error_message.as_deref(), Some("Provider stopped with: MALFORMED_FUNCTION_CALL"));

    let message = google_generative_ai::stream(&model, &context("length"), key_options("k")).result().await;
    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("MAX_TOKENS"));
    assert!(message.tool_calls().next().is_some());

    let message = google_generative_ai::stream(&model, &context("tool"), key_options("k")).result().await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let call = message.tool_calls().next().unwrap();
    assert_eq!(call.id, "call-1");
    assert_eq!(call.arguments.get("value"), Some(&json!("truncated")));
}

#[tokio::test]
async fn reports_api_errors_and_retries_retryable_statuses() {
    let server = MockServer::sequence(vec![
        Response::json(429, json!({ "error": { "code": 429, "message": "slow down", "status": "RESOURCE_EXHAUSTED" } })),
        Response::sse(&sse(&[json!({ "candidates": [{ "content": { "parts": [{ "text": "ok" }] }, "finishReason": "STOP" }] })])),
    ])
    .await;
    let model = gemini(&server.url, "gemini-2.5-flash");
    let options = StreamOptions { api_key: Some("k".into()), max_retries: Some(1), ..Default::default() };
    let message = google_generative_ai::stream(&model, &context("hi"), options).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(server.requests().len(), 2);

    let server = MockServer::sequence(vec![Response::json(
        400,
        json!({ "error": { "code": 400, "message": "bad", "status": "INVALID_ARGUMENT" } }),
    )])
    .await;
    let model = gemini(&server.url, "gemini-2.5-flash");
    let options = StreamOptions { api_key: Some("k".into()), max_retries: Some(2), ..Default::default() };
    let message = google_generative_ai::stream(&model, &context("hi"), options).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(r#"{"error":{"code":400,"message":"bad","status":"INVALID_ARGUMENT"}}"#)
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn streams_thinking_and_signatures() {
    let server = MockServer::sequence(vec![Response::sse(&sse(&[
        json!({ "candidates": [{ "content": { "parts": [{ "text": "plan", "thought": true, "thoughtSignature": "c2lnMQ==" }] } }] }),
        json!({ "candidates": [{ "content": { "parts": [{ "text": " more", "thought": true }] } }] }),
        json!({ "candidates": [{ "content": { "parts": [{ "text": "answer", "thoughtSignature": "c2lnMg==" }] }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 10, "cachedContentTokenCount": 4, "candidatesTokenCount": 3, "thoughtsTokenCount": 2, "totalTokenCount": 15 } }),
    ]))])
    .await;
    let model = gemini(&server.url, "gemini-2.5-flash");
    let (events, message) = google_generative_ai::stream(&model, &context("hi"), key_options("k")).collect_all().await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        events.iter().map(|e| e.type_name()).collect::<Vec<_>>(),
        ["start", "thinking_start", "thinking_delta", "thinking_delta", "thinking_end", "text_start", "text_delta", "text_end", "done"]
    );
    let AssistantContent::Thinking(thinking) = &message.content[0] else { panic!() };
    assert_eq!(thinking.thinking, "plan more");
    assert_eq!(thinking.thinking_signature.as_deref(), Some("c2lnMQ=="));
    let AssistantContent::Text(text) = &message.content[1] else { panic!() };
    assert_eq!(text.text_signature.as_deref(), Some("c2lnMg=="));
    assert_eq!((message.usage.input, message.usage.output, message.usage.cache_read, message.usage.reasoning), (6, 5, 4, Some(2)));
}

#[tokio::test]
async fn surfaces_in_stream_error_objects() {
    let server = MockServer::sequence(vec![Response::chunked(
        200,
        "text/event-stream",
        vec![
            format!("data: {}\n\n", json!({ "candidates": [{ "content": { "parts": [{ "text": "a" }] } }] })).into_bytes(),
            json!({ "error": { "code": 500, "message": "boom", "status": "INTERNAL" } }).to_string().into_bytes(),
        ],
    )])
    .await;
    let model = gemini(&server.url, "gemini-2.5-flash");
    let message = google_generative_ai::stream(&model, &context("hi"), key_options("k")).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(r#"got status: INTERNAL. {"error":{"code":500,"message":"boom","status":"INTERNAL"}}"#)
    );
}

#[tokio::test]
async fn rejects_custom_http_clients_and_missing_keys() {
    let model = get_builtin_model("google", "gemini-2.5-flash").unwrap();
    let options = StreamOptions { api_key: Some("k".into()), http_client: Some(reqwest::Client::new()), ..Default::default() };
    let message = google_generative_ai::stream(&model, &context("hi"), options).result().await;
    assert_eq!(message.error_message.as_deref(), Some("Custom fetch is not supported by the Google Generative AI adapter"));
    let message = google_generative_ai::stream(&model, &context("hi"), StreamOptions::default()).result().await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: google"));
}

// ---------------------------------------------------------------------------
// Payloads (google-thinking-level-map.test.ts)
// ---------------------------------------------------------------------------

fn level_map(entries: &[(ModelThinkingLevel, Option<&str>)]) -> ThinkingLevelMap {
    entries.iter().map(|(k, v)| (*k, v.map(str::to_string))).collect()
}

fn test_model(api: &str, id: &str, map: ThinkingLevelMap) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: if api == "google-vertex" { "test-vertex".into() } else { "test-google".into() },
        base_url: "http://127.0.0.1:9/v1beta".into(),
        reasoning: true,
        thinking_level_map: Some(map),
        input: vec![ModelInputModality::Text],
        context_window: 128000,
        max_tokens: 4096,
        ..Default::default()
    }
}

async fn capture_payload(model: &Model, reasoning: Option<ThinkingLevel>, budgets: Option<ThinkingBudgets>) -> Value {
    let captured = Arc::new(Mutex::new(None::<Value>));
    let sink = captured.clone();
    let options = StreamOptions {
        api_key: Some("test".into()),
        reasoning,
        thinking_budgets: budgets,
        on_payload: Some(Arc::new(move |payload: Value, _: Model| {
            *sink.lock() = Some(payload);
            Box::pin(async { None })
        })),
        ..Default::default()
    };
    let result = if model.api == "google-vertex" {
        google_vertex::stream_simple(model, &context("Hello"), options).result().await
    } else {
        google_generative_ai::stream_simple(model, &context("Hello"), options).result().await
    };
    captured.lock().clone().unwrap_or_else(|| panic!("payload not captured: {:?}", result.error_message))
}

#[test]
fn resolves_thinking_levels_and_mappings() {
    use ModelThinkingLevel as L;
    let empty = test_model("google-generative-ai", "gemini-3.7-flash", ThinkingLevelMap::new());
    for (level, expected) in [(L::Minimal, "minimal"), (L::Low, "low"), (L::Medium, "medium"), (L::High, "high")] {
        assert_eq!(resolve_google_thinking_level(&empty, level).unwrap().as_str(), expected);
    }
    for (mapped, expected) in
        [("minimal", "minimal"), ("low", "low"), ("medium", "medium"), ("high", "high"), ("MINIMAL", "minimal"), ("HIGH", "high")]
    {
        let model = test_model(
            "google-generative-ai",
            "gemini-3.7-flash",
            level_map(&[(L::High, Some(mapped)), (L::Xhigh, Some(mapped)), (L::Max, Some(mapped))]),
        );
        for level in [L::High, L::Xhigh, L::Max] {
            assert_eq!(resolve_google_thinking_level(&model, level).unwrap().as_str(), expected);
        }
    }
    let invalid = test_model("google-generative-ai", "gemini-3.7-flash", level_map(&[(L::Xhigh, Some("extreme"))]));
    assert_eq!(
        resolve_google_thinking_level(&invalid, L::Xhigh).unwrap_err().to_string(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: xhigh -> extreme"
    );
    assert_eq!(
        resolve_google_thinking_level(&empty, L::Max).unwrap_err().to_string(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: max -> undefined"
    );
}

#[tokio::test]
async fn thinking_config_payloads() {
    use ModelThinkingLevel as L;
    let restricted = level_map(&[
        (L::Off, None),
        (L::Minimal, None),
        (L::Low, Some("low")),
        (L::Medium, Some("medium")),
        (L::High, Some("high")),
        (L::Xhigh, None),
        (L::Max, None),
    ]);
    for api in ["google-generative-ai", "google-vertex"] {
        let payload = capture_payload(&test_model(api, "gemini-3.8-flash", restricted.clone()), None, None).await;
        assert_eq!(payload["config"]["thinkingConfig"], json!({ "thinkingLevel": "LOW" }), "{api}");

        let payload =
            capture_payload(&test_model(api, "gemini-3.1-pro-preview", restricted.clone()), Some(ThinkingLevel::Medium), None).await;
        assert_eq!(payload["config"]["thinkingConfig"], json!({ "includeThoughts": true, "thinkingLevel": "MEDIUM" }));

        let payload = capture_payload(&test_model(api, "gemini-2.5-flash", ThinkingLevelMap::new()), None, None).await;
        assert_eq!(payload["config"]["thinkingConfig"], json!({ "thinkingBudget": 0 }));

        let payload = capture_payload(&test_model(api, "gemini-4-argon", ThinkingLevelMap::new()), Some(ThinkingLevel::Medium), None).await;
        assert_eq!(payload["config"]["thinkingConfig"], json!({ "includeThoughts": true, "thinkingLevel": "MEDIUM" }));
    }
    for reasoning in [ThinkingLevel::Xhigh, ThinkingLevel::Max] {
        let model = test_model("google-generative-ai", "gemini-3.7-flash", level_map(&[(L::Xhigh, Some("high")), (L::Max, Some("high"))]));
        let payload = capture_payload(&model, Some(reasoning), None).await;
        assert_eq!(payload["config"]["thinkingConfig"], json!({ "includeThoughts": true, "thinkingLevel": "HIGH" }));
    }
    let model = test_model("google-generative-ai", "gemini-3.7-flash", level_map(&[(L::High, Some("LOW"))]));
    let payload = capture_payload(&model, Some(ThinkingLevel::High), None).await;
    assert_eq!(payload["config"]["thinkingConfig"]["thinkingLevel"], "LOW");

    let budgets = ThinkingBudgets { high: Some(1234), ..Default::default() };
    let model = test_model("google-generative-ai", "gemini-2.5-flash", level_map(&[(L::Xhigh, Some("high"))]));
    let payload = capture_payload(&model, Some(ThinkingLevel::Xhigh), Some(budgets)).await;
    assert_eq!(payload["config"]["thinkingConfig"]["thinkingBudget"], 1234);

    let model = test_model("google-vertex", "gemini-3.7-flash", level_map(&[(L::Xhigh, Some("high"))]));
    let payload = capture_payload(&model, Some(ThinkingLevel::Xhigh), None).await;
    assert_eq!(payload["config"]["thinkingConfig"], json!({ "includeThoughts": true, "thinkingLevel": "HIGH" }));

    let budgets = ThinkingBudgets { high: Some(4321), ..Default::default() };
    let model = test_model("google-vertex", "gemini-2.5-flash", level_map(&[(L::Max, Some("high"))]));
    let payload = capture_payload(&model, Some(ThinkingLevel::Max), Some(budgets)).await;
    assert_eq!(payload["config"]["thinkingConfig"]["thinkingBudget"], 4321);
}

// ---------------------------------------------------------------------------
// convertTools / convertMessages (google-shared-*.test.ts)
// ---------------------------------------------------------------------------

fn make_tool(parameters: Value) -> Tool {
    Tool::new("test_tool", "A test tool", parameters)
}

#[test]
fn convert_tools_strips_meta_keys_for_openapi_parameters() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#", "$id": "urn:bash-tool", "$comment": "c",
        "$defs": { "commandDef": { "type": "string" } }, "definitions": { "legacyDef": { "type": "number" } },
        "type": "object",
        "properties": {
            "command": { "type": "string" },
            "deep": { "$schema": "x", "$id": "urn:nested", "type": "string" },
            "refProp": { "$ref": "#/$defs/someDef", "type": "string" }
        },
        "required": ["command"]
    }))];
    let result = convert_tools(&tools, true, true).unwrap().unwrap();
    assert_eq!(
        result[0]["functionDeclarations"][0]["parameters"],
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" }, "deep": { "type": "string" }, "refProp": { "$ref": "#/$defs/someDef", "type": "string" } },
            "required": ["command"]
        })
    );
    assert!(tools[0].parameters.get("$schema").is_some());
    let result = convert_tools(&tools, false, true).unwrap().unwrap();
    assert_eq!(result[0]["functionDeclarations"][0]["parametersJsonSchema"], tools[0].parameters);
    assert!(convert_tools(&[], false, true).unwrap().is_none());
    assert!(convert_tools(&[], true, true).unwrap().is_none());
}

#[test]
fn validated_function_calling_for_strict_tools() {
    let mut tool = make_tool(json!({ "type": "object", "properties": {} }));
    tool.constrained_sampling =
        Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict: JsonSchemaStrictness::Require }));
    assert!(supports_google_strict_tool_sampling("gemini-3.1-pro-preview"));
    assert!(!supports_google_strict_tool_sampling("gemini-2.5-pro"));
    assert_eq!(resolve_google_function_calling_mode(std::slice::from_ref(&tool), None, true).unwrap(), Some("VALIDATED"));
    let error = resolve_google_function_calling_mode(&[tool], None, false).unwrap_err();
    assert!(error.to_string().contains("Tool \"test_tool\" requires JSON-schema constrained sampling"));
}

#[test]
fn thinking_part_detection_and_signature_retention() {
    assert!(is_thinking_part(&json!({ "thought": true })));
    assert!(!is_thinking_part(&json!({ "thoughtSignature": "sig" })));
    assert!(!is_thinking_part(&json!({ "thought": false, "thoughtSignature": "" })));
    let first = retain_thought_signature(None, Some("sig-1"));
    assert_eq!(first.as_deref(), Some("sig-1"));
    assert_eq!(retain_thought_signature(first.clone(), None).as_deref(), Some("sig-1"));
    assert_eq!(retain_thought_signature(first, Some("")).as_deref(), Some("sig-1"));
    assert_eq!(retain_thought_signature(Some("sig-1".into()), Some("sig-2")).as_deref(), Some("sig-2"));
}

fn convert_model(api: &str, provider: &str, id: &str, input: Vec<ModelInputModality>) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        base_url: "https://example.com".into(),
        reasoning: true,
        input,
        context_window: 128000,
        max_tokens: 8192,
        ..Default::default()
    }
}

fn assistant(api: &str, provider: &str, model: &str, content: Vec<AssistantContent>) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: api.into(),
        provider: provider.into(),
        model: model.into(),
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        ..Default::default()
    })
}

fn call(id: &str, args: Value, signature: Option<&str>) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments: args.as_object().cloned().unwrap(),
        thought_signature: signature.map(str::to_string),
        ..Default::default()
    })
}

fn tool_result(id: &str, content: Vec<UserContentBlock>) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "bash".into(),
        content,
        timestamp: 1,
        ..Default::default()
    })
}

fn unsigned_context(model: &Model, history_model: &str, signature: Option<&str>) -> TranscriptContext {
    normalize_context(&Context {
        system_prompt: None,
        tools: None,
        messages: vec![
            Message::User(UserMessage::new("Hi", 1)),
            assistant(&model.api, &model.provider, history_model, vec![
                call("call_1", json!({ "command": "echo hi" }), signature),
                call("call_2", json!({ "command": "ls -la" }), None),
            ]),
            tool_result("call_1", vec![UserContentBlock::text("hi")]),
            tool_result("call_2", vec![UserContentBlock::text("files")]),
        ],
    })
}

fn parts(contents: &[Value]) -> Vec<Value> {
    contents.iter().flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default()).collect()
}

#[test]
fn gemini3_tool_call_ids_and_signatures() {
    for model in [
        convert_model("google-generative-ai", "google", "gemini-3-pro-preview", vec![ModelInputModality::Text]),
        convert_model("google-generative-ai", "google", "gemini-3.6-flash", vec![ModelInputModality::Text]),
        convert_model("google-vertex", "google-vertex", "gemini-3-pro-preview", vec![ModelInputModality::Text]),
    ] {
        let contents = convert_messages(&model, &unsigned_context(&model, &model.id, None));
        let all = parts(&contents);
        let call_ids: Vec<&str> = all.iter().filter_map(|p| p["functionCall"]["id"].as_str()).collect();
        let response_ids: Vec<&str> = all.iter().filter_map(|p| p["functionResponse"]["id"].as_str()).collect();
        assert_eq!(call_ids, ["call_1", "call_2"]);
        assert_eq!(response_ids, ["call_1", "call_2"]);
        let model_turn = contents.iter().find(|c| c["role"] == "model").unwrap();
        assert!(!model_turn.to_string().contains("skip_thought_signature_validator"));
        assert!(model_turn["parts"].as_array().unwrap().iter().all(|p| p.get("thoughtSignature").is_none()));
    }

    let model = convert_model("google-generative-ai", "google", "gemini-3-pro-preview", vec![ModelInputModality::Text]);
    let other = convert_messages(&model, &unsigned_context(&model, "other-model", None));
    let turn = other.iter().find(|c| c["role"] == "model").unwrap();
    let calls: Vec<&Value> = turn["parts"].as_array().unwrap().iter().filter(|p| p.get("functionCall").is_some()).collect();
    assert_eq!(calls.len(), 2);
    assert!(!turn.to_string().contains("Historical context"));

    let valid = "AAAAAAAAAAAAAAAAAAAAAA==";
    let signed = convert_messages(&model, &unsigned_context(&model, &model.id, Some(valid)));
    let turn = signed.iter().find(|c| c["role"] == "model").unwrap();
    assert_eq!(turn["parts"][0]["thoughtSignature"], valid);
    assert!(turn["parts"][1].get("thoughtSignature").is_none());

    let old = convert_model("google-generative-ai", "google", "gemini-2.5-flash", vec![ModelInputModality::Text]);
    let contents = convert_messages(&old, &unsigned_context(&old, "other-model", None));
    let all = parts(&contents);
    assert!(all.iter().filter(|p| p.get("functionCall").is_some()).all(|p| p["functionCall"].get("id").is_none()));
    assert_eq!(all.iter().filter(|p| p.get("functionResponse").is_some()).count(), 2);
    assert!(all.iter().filter(|p| p.get("functionResponse").is_some()).all(|p| p["functionResponse"].get("id").is_none()));

    for (expected, id) in [(false, "gemini-2.5-flash"), (true, "gemini-3.6-flash"), (true, "claude-sonnet-4-5"), (true, "gpt-oss-120b")] {
        assert_eq!(requires_tool_call_id(id), expected, "{id}");
    }
}

fn image_routing_context(model: &Model, tweak: bool) -> TranscriptContext {
    let image = UserContentBlock::Image(ImageContent { data: "abc".into(), mime_type: "image/png".into() });
    let read = |id: &str, path: &str| {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({ "path": path }).as_object().cloned().unwrap(),
            ..Default::default()
        })
    };
    let result = |id: &str, content: Vec<UserContentBlock>| {
        Message::ToolResult(ToolResultMessage { tool_call_id: id.into(), tool_name: "read".into(), content, timestamp: 1, ..Default::default() })
    };
    let (a, img) = if tweak {
        (vec![UserContentBlock::text("\r\n")], vec![UserContentBlock::text(" \n"), image])
    } else {
        (vec![UserContentBlock::text("alpha text")], vec![image])
    };
    normalize_context(&Context {
        system_prompt: None,
        tools: None,
        messages: vec![
            Message::User(UserMessage::new("read the files", 1)),
            assistant(&model.api, &model.provider, &model.id, vec![read("call_a", "a.txt"), read("call_img", "image.png"), read("call_b", "b.txt")]),
            result("call_a", a),
            result("call_img", img),
            result("call_b", vec![UserContentBlock::text("beta text")]),
        ],
    })
}

#[test]
fn image_tool_result_routing() {
    let input = vec![ModelInputModality::Text, ModelInputModality::Image];
    let old = convert_model("google-generative-ai", "google", "gemini-2.5-flash", input.clone());
    let contents = convert_messages(&old, &image_routing_context(&old, false));
    assert_eq!(contents.len(), 5);
    assert!(contents[2]["parts"].as_array().unwrap().iter().all(|p| p.get("functionResponse").is_some()));
    assert_eq!(contents[3]["parts"][0]["text"], "Tool result image:");
    assert!(contents[3]["parts"][1].get("inlineData").is_some());
    assert!(contents[4]["parts"][0].get("functionResponse").is_some());

    let new = convert_model("google-generative-ai", "google", "gemini-3-pro-preview", input);
    let contents = convert_messages(&new, &image_routing_context(&new, false));
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 3);
    let image_response = &contents[2]["parts"][1]["functionResponse"];
    assert_eq!(image_response["parts"].as_array().unwrap().len(), 1);
    assert!(image_response["parts"][0].get("inlineData").is_some());

    let contents = convert_messages(&new, &image_routing_context(&new, true));
    let responses: Vec<&Value> = contents[2]["parts"].as_array().unwrap().iter().map(|p| &p["functionResponse"]["response"]).collect();
    assert_eq!(*responses[0], json!({ "output": "" }));
    assert_eq!(*responses[1], json!({ "output": "(see attached image)" }));
    assert_eq!(*responses[2], json!({ "output": "beta text" }));
}

#[test]
fn signed_empty_blocks() {
    let valid = "AAAAAAAAAAAAAAAAAAAAAA==";
    let model = convert_model("google-generative-ai", "google", "gemini-3-pro-preview", vec![ModelInputModality::Text]);
    let run = |history_model: &str, content: Vec<AssistantContent>| {
        let context = normalize_context(&Context {
            system_prompt: None,
            tools: None,
            messages: vec![Message::User(UserMessage::new("Hi", 1)), assistant(&model.api, &model.provider, history_model, content)],
        });
        let contents = convert_messages(&model, &context);
        contents.into_iter().find(|c| c["role"] == "model").unwrap()
    };
    let ls = || call("call_1", json!({ "command": "ls" }), None);

    let turn = run(&model.id, vec![
        AssistantContent::Thinking(ThinkingContent { thinking: String::new(), thinking_signature: Some(valid.into()), redacted: None, ..Default::default() }),
        ls(),
    ]);
    let signed: Vec<&Value> = turn["parts"].as_array().unwrap().iter().filter(|p| p["thoughtSignature"] == valid).collect();
    assert_eq!(signed.len(), 1);
    assert_eq!(signed[0]["thought"], true);

    let turn = run(&model.id, vec![AssistantContent::Text(TextContent { text: String::new(), text_signature: Some(valid.into()), ..Default::default() }), ls()]);
    assert_eq!(turn["parts"].as_array().unwrap().iter().filter(|p| p["thoughtSignature"] == valid).count(), 1);

    let turn = run(&model.id, vec![AssistantContent::Thinking(ThinkingContent::new("")), AssistantContent::Text(TextContent::new("   ")), ls()]);
    assert_eq!(turn["parts"].as_array().unwrap().len(), 1);
    assert!(turn["parts"][0].get("functionCall").is_some());

    let turn = run("other-model", vec![
        AssistantContent::Thinking(ThinkingContent { thinking: String::new(), thinking_signature: Some(valid.into()), redacted: None, ..Default::default() }),
        AssistantContent::Text(TextContent { text: String::new(), text_signature: Some(valid.into()), ..Default::default() }),
        ls(),
    ]);
    assert_eq!(turn["parts"].as_array().unwrap().len(), 1);
    assert!(!turn.to_string().contains(valid));
}

// ---------------------------------------------------------------------------
// Vertex (google-vertex-api-key-resolution.test.ts, google-raw-stop-reason.test.ts)
// ---------------------------------------------------------------------------

fn vertex_options(api_key: Option<&str>) -> StreamOptions {
    let mut options = StreamOptions { api_key: api_key.map(str::to_string), ..Default::default() };
    options.extra.insert("project".into(), json!("test-project"));
    options.extra.insert("location".into(), json!("us-central1"));
    options
}

#[tokio::test]
async fn vertex_api_key_and_custom_base_urls() {
    let server = MockServer::start(|_, _| Response::sse(&sse(&[json!({ "responseId": "vertex-response-id", "candidates": [{ "content": { "parts": [{ "text": "ok" }] }, "finishReason": "STOP" }] })]))).await;
    let model = vertex(&server.url, "gemini-3-flash-preview");
    let message = google_vertex::stream(&model, &context("hello"), key_options("AIzaSyExampleRealisticLookingApiKey123456")).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(message.response_id.as_deref(), Some("vertex-response-id"));
    let request = server.request(0);
    assert_eq!(request.path, "/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse");
    assert_eq!(request.header("x-goog-api-key"), Some("AIzaSyExampleRealisticLookingApiKey123456"));
    assert!(request.header("authorization").is_none());

    let with_version = vertex(&format!("{}/v1/projects/test-project/locations/global", server.url), "gemini-3-flash-preview");
    google_vertex::stream(&with_version, &context("hello"), key_options("AIzaKey")).result().await;
    assert_eq!(
        server.request(1).path,
        "/v1/projects/test-project/locations/global/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
    );

    let mut headers = ProviderHeaders::new();
    headers.insert("User-Agent".into(), Some("custom-agent".into()));
    let options = StreamOptions { headers: Some(headers), ..key_options("AIzaKey") };
    google_vertex::stream(&model, &context("hello"), options).result().await;
    assert_eq!(server.request(2).header("user-agent"), Some("custom-agent"));
}

#[test]
fn vertex_client_endpoints() {
    let model = get_builtin_model("google-vertex", "gemini-3-flash-preview").unwrap();
    let adc = google_vertex::client_options(&model, &vertex_options(Some("<authenticated>"))).unwrap();
    assert!(adc.api_key.is_none());
    assert_eq!(adc.project.as_deref(), Some("test-project"));
    assert_eq!(adc.location.as_deref(), Some("us-central1"));
    assert_eq!(adc.api_version.as_deref(), Some("v1"));
    assert!(adc.base_url.is_none(), "generated {{location}} placeholders are not forwarded");
    let client = GoogleClient::new(adc).unwrap();
    assert_eq!(
        client.stream_url("publishers/google/models/gemini-3-flash-preview"),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/test-project/locations/us-central1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
    );
    let marker = google_vertex::client_options(&model, &vertex_options(Some("gcp-vertex-credentials"))).unwrap();
    assert!(marker.api_key.is_none());

    let keyed = google_vertex::client_options(&model, &key_options("AIzaSyExampleRealisticLookingApiKey123456")).unwrap();
    assert!(keyed.project.is_none() && keyed.location.is_none());
    let client = GoogleClient::new(keyed).unwrap();
    assert_eq!(
        client.stream_url("publishers/google/models/gemini-3-flash-preview"),
        "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
    );

    let mut custom = model.clone();
    custom.base_url = "https://proxy.example.com".into();
    let options = google_vertex::client_options(&custom, &vertex_options(None)).unwrap();
    assert_eq!(options.base_url.as_deref(), Some("https://proxy.example.com"));
    assert!(options.base_url_collection_scope);
    assert_eq!(
        GoogleClient::new(options).unwrap().stream_url("publishers/google/models/m"),
        "https://proxy.example.com/v1/publishers/google/models/m:streamGenerateContent?alt=sse"
    );

    let mut eu = vertex_options(None);
    eu.extra.insert("location".into(), json!("eu"));
    let client = GoogleClient::new(google_vertex::client_options(&model, &eu).unwrap()).unwrap();
    assert!(client.stream_url("publishers/google/models/m").starts_with("https://aiplatform.eu.rep.googleapis.com/v1/projects/test-project/locations/eu/"));

    let missing = google_vertex::client_options(&model, &StreamOptions::default());
    if std::env::var("GOOGLE_CLOUD_PROJECT").is_err() && std::env::var("GCLOUD_PROJECT").is_err() {
        assert_eq!(
            missing.unwrap_err().to_string(),
            "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
        );
    }
}

#[tokio::test]
async fn vertex_adc_service_account_tokens() {
    let key = include_str!("fixtures/test_rsa_key.pem");
    let token_server = MockServer::start(|request, index| {
        let body = request.text();
        assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"));
        Response::json(200, json!({ "access_token": format!("token-{index}"), "expires_in": 3600, "token_type": "Bearer" }))
    })
    .await;
    google_auth::set_google_token_url_override(Some(format!("{}/token", token_server.url)));

    let dir = std::env::temp_dir().join(format!("pi-ai-google-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key_file = dir.join("sa.json");
    std::fs::write(
        &key_file,
        json!({ "type": "service_account", "client_email": "sa@test.iam.gserviceaccount.com", "private_key": key, "private_key_id": "kid", "quota_project_id": "quota-p" })
            .to_string(),
    )
    .unwrap();

    let api_server = MockServer::start(|_, _| {
        Response::sse(&sse(&[json!({ "candidates": [{ "content": { "parts": [{ "text": "ok" }] }, "finishReason": "SAFETY" }] })]))
    })
    .await;
    let model = vertex(&api_server.url, "gemini-3-flash-preview");
    let mut options = vertex_options(Some("<authenticated>"));
    let mut env = ProviderEnv::new();
    env.insert("GOOGLE_APPLICATION_CREDENTIALS".into(), key_file.to_string_lossy().into_owned());
    options.env = Some(env);

    let message = google_vertex::stream(&model, &context("hello"), options.clone()).result().await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("SAFETY"));
    assert_eq!(message.error_message.as_deref(), Some("Provider stopped with: SAFETY"));
    let request = api_server.request(0);
    assert_eq!(request.header("authorization"), Some("Bearer token-0"));
    assert_eq!(request.header("x-goog-user-project"), Some("quota-p"));
    assert!(request.header("x-goog-api-key").is_none());
    assert_eq!(request.path, "/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse");

    // The assertion is an RS256 JWT for the token endpoint, verifiable with the public key.
    let form = token_server.request(0).text();
    let assertion = form.split('&').find_map(|p| p.strip_prefix("assertion=")).unwrap();
    let segments: Vec<&str> = assertion.split('.').collect();
    assert_eq!(segments.len(), 3);
    use base64::Engine;
    let decode = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    let header: Value = serde_json::from_slice(&decode(segments[0])).unwrap();
    let payload: Value = serde_json::from_slice(&decode(segments[1])).unwrap();
    assert_eq!(header, json!({ "alg": "RS256" }));
    assert_eq!(payload["iss"], "sa@test.iam.gserviceaccount.com");
    assert_eq!(payload["aud"], "https://oauth2.googleapis.com/token");
    assert_eq!(payload["scope"], "https://www.googleapis.com/auth/cloud-platform");
    assert_eq!(payload["exp"].as_i64().unwrap() - payload["iat"].as_i64().unwrap(), 3600);
    {
        use rsa::pkcs8::DecodePrivateKey;
        use rsa::signature::Verifier;
        let private = rsa::RsaPrivateKey::from_pkcs8_pem(key).unwrap();
        let verifying = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(rsa::RsaPublicKey::from(&private));
        let signature = rsa::pkcs1v15::Signature::try_from(decode(segments[2]).as_slice()).unwrap();
        verifying.verify(format!("{}.{}", segments[0], segments[1]).as_bytes(), &signature).unwrap();
    }

    // Like TS (a new GoogleGenAI/GoogleAuth per request), every request re-reads the
    // credentials and fetches a new token.
    google_vertex::stream(&model, &context("hello"), options.clone()).result().await;
    assert_eq!(token_server.requests().len(), 2);
    assert_eq!(api_server.request(1).header("authorization"), Some("Bearer token-1"));

    // Token requests retry like Gaxios (`retryConfig` with POST): a 503 is retried after 100 ms.
    let flaky = MockServer::sequence(vec![
        Response::json(503, json!({ "error": "unavailable" })),
        Response::json(200, json!({ "access_token": "retried-token", "expires_in": 3600 })),
    ])
    .await;
    google_auth::set_google_token_url_override(Some(format!("{}/token", flaky.url)));
    let message = google_vertex::stream(&model, &context("hello"), options).result().await;
    assert_eq!(message.raw_stop_reason.as_deref(), Some("SAFETY"), "{:?}", message.error_message);
    assert_eq!(flaky.requests().len(), 2);
    assert_eq!(api_server.request(2).header("authorization"), Some("Bearer retried-token"));

    // Authorized-user (gcloud) credentials use the refresh-token grant.
    let user_server = MockServer::start(|request, _| {
        let body = request.text();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=rt"));
        Response::json(200, json!({ "access_token": "user-token", "expires_in": 3600 }))
    })
    .await;
    google_auth::set_google_token_url_override(Some(format!("{}/token", user_server.url)));
    let user_file = dir.join("user.json");
    std::fs::write(&user_file, json!({ "type": "authorized_user", "client_id": "id", "client_secret": "s", "refresh_token": "rt" }).to_string())
        .unwrap();
    let credentials = google_auth::GoogleCredentials::from_file(&user_file).unwrap();
    let headers = google_auth::get_request_headers(&credentials, google_auth::VERTEX_AI_SCOPE, &reqwest::Client::new()).await.unwrap();
    assert_eq!(headers, vec![("authorization".to_string(), "Bearer user-token".to_string())]);

    // Refresh-token failures surface the Gaxios message (the OAuth `error` code).
    let failing = MockServer::sequence(vec![Response::json(400, json!({ "error": "invalid_grant", "error_description": "Bad Request" }))]).await;
    google_auth::set_google_token_url_override(Some(format!("{}/token", failing.url)));
    let other_user = google_auth::GoogleCredentials::from_json(&json!({ "type": "authorized_user", "client_id": "x", "client_secret": "y", "refresh_token": "z" })).unwrap();
    let error = google_auth::get_request_headers(&other_user, google_auth::VERTEX_AI_SCOPE, &reqwest::Client::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "invalid_grant");

    google_auth::set_google_token_url_override(None);
    let _ = std::fs::remove_dir_all(&dir);
}
