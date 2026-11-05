//! Offline tests for the Anthropic Messages API against a local mock server
//! (ports of `anthropic-sse-parsing.test.ts` and request-shape tests).

mod common;

use common::*;
use pi_ai::api::anthropic_messages;
use pi_ai::providers::all::get_builtin_model;
use pi_ai::*;
use serde_json::json;

fn minimal_events() -> String {
    sse_body(&[
        ("message_start", json!({"type": "message_start", "message": {"id": "msg_test", "usage": {"input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}})),
        ("message_stop", json!({"type": "message_stop"})),
    ])
}

fn model_at(base_url: &str, id: &str) -> Model {
    let mut model = get_builtin_model("anthropic", id).unwrap();
    model.base_url = base_url.to_string();
    model
}

fn context(text: &str) -> TranscriptContext {
    normalize_context(&Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![Message::User(UserMessage::new(text, 1))],
        tools: Some(vec![Tool::new("read", "Read a file", json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}))]),
    })
}

#[tokio::test]
async fn streams_text_and_sends_sdk_shaped_request() {
    let server = serve(vec![MockResponse::sse(minimal_events())]).await;
    let model = model_at(&server.base_url, "claude-haiku-4-5");
    let observed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let sink = observed.clone();
    let options = StreamOptions {
        api_key: Some("sk-ant-test".into()),
        on_provider_stream_event: Some(std::sync::Arc::new(move |event: serde_json::Value, _m: Model| {
            sink.lock().push(event["type"].as_str().unwrap_or_default().to_string());
            Box::pin(async {})
        })),
        ..Default::default()
    };
    let (events, result) = anthropic_messages::stream(&model, &context("Hello"), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.text(), "Hello");
    assert_eq!(result.response_id.as_deref(), Some("msg_test"));
    assert_eq!(result.usage.input, 12);
    assert_eq!(result.usage.output, 5);
    assert_eq!(result.usage.total_tokens, 17);
    assert!(result.duration_ms.is_some());
    assert_eq!(
        events.iter().map(|e| e.type_name()).collect::<Vec<_>>(),
        vec!["start", "text_start", "text_delta", "text_end", "done"]
    );
    assert_eq!(
        observed.lock().clone(),
        vec!["message_start", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]
    );

    let request = server.requests.lock()[0].clone();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/messages?beta=true");
    assert_eq!(request.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("anthropic-dangerous-direct-browser-access"), Some("true"));
    assert!(request.header("user-agent").unwrap().starts_with("pi/"));
    let body = request.json();
    assert_eq!(body["model"], "claude-haiku-4-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"], json!([{"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}}]));
    assert_eq!(body["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "Hello", "cache_control": {"type": "ephemeral"}}]}]));
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], "read");
    assert_eq!(tool["eager_input_streaming"], true);
    assert_eq!(tool["input_schema"], json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}));
    assert_eq!(tool["cache_control"], json!({"type": "ephemeral"}));
    assert!(body.get("betas").is_none());
}

#[tokio::test]
async fn tool_calls_thinking_and_oauth_headers() {
    let events = sse_body(&[
        ("message_start", json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-haiku-4-5", "usage": {"input_tokens": 5, "output_tokens": 0}}})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a."}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "ts\"}"}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let server = serve(vec![MockResponse::sse(events)]).await;
    let model = model_at(&server.base_url, "claude-haiku-4-5");
    let options = StreamOptions { api_key: Some("sk-ant-oat01-token".into()), ..Default::default() };
    let (events, result) = anthropic_messages::stream(&model, &context("Read it"), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::ToolUse, "{:?}", result.error_message);
    let call = result.tool_calls().next().unwrap();
    assert_eq!(call.name, "read", "Claude Code names map back to the declared tool");
    assert_eq!(serde_json::Value::Object(call.arguments.clone()), json!({"path": "a.ts"}));
    let thinking = result.content[0].as_thinking().unwrap();
    assert_eq!(thinking.thinking, "hmm");
    assert_eq!(thinking.thinking_signature.as_deref(), Some("sig"));
    assert!(events.iter().any(|e| matches!(e, AssistantMessageEvent::ToolCallDelta { .. })));

    let request = server.requests.lock()[0].clone();
    assert_eq!(request.header("authorization"), Some("Bearer sk-ant-oat01-token"));
    assert!(request.header("x-api-key").is_none());
    assert_eq!(request.header("user-agent"), Some("claude-cli/2.1.280"));
    assert_eq!(request.header("anthropic-beta"), Some("claude-code-20250219,oauth-2025-04-20"));
    let body = request.json();
    assert_eq!(body["system"][0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.");
    assert_eq!(body["tools"][0]["name"], "Read");
}

#[tokio::test]
async fn http_errors_and_missing_stop() {
    let server = serve(vec![MockResponse::json(
        400,
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long: 213462 tokens > 200000 maximum"}}).to_string(),
    )])
    .await;
    let model = model_at(&server.base_url, "claude-haiku-4-5");
    let result =
        anthropic_messages::stream(&model, &context("x"), StreamOptions { api_key: Some("k".into()), ..Default::default() })
            .result()
            .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    let message = result.error_message.clone().unwrap();
    assert!(message.starts_with("400 {"), "{message}");
    assert!(is_context_overflow(&result, None));

    let truncated = sse_body(&[(
        "message_start",
        json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 1}}}),
    )]);
    let server = serve(vec![MockResponse::sse(truncated)]).await;
    let model = model_at(&server.base_url, "claude-haiku-4-5");
    let result =
        anthropic_messages::stream(&model, &context("x"), StreamOptions { api_key: Some("k".into()), ..Default::default() })
            .result()
            .await;
    assert_eq!(result.error_message.as_deref(), Some("Anthropic stream ended before message_stop"));
    assert!(is_retryable_assistant_error(&result));
}

#[tokio::test]
async fn simple_options_map_reasoning() {
    let server = serve(vec![MockResponse::sse(minimal_events()), MockResponse::sse(minimal_events())]).await;
    let adaptive = model_at(&server.base_url, "claude-fable-5");
    let result = anthropic_messages::stream_simple(
        &adaptive,
        &context("x"),
        StreamOptions { api_key: Some("k".into()), reasoning: Some(ThinkingLevel::Xhigh), ..Default::default() },
    )
    .result()
    .await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let body = server.requests.lock()[0].json();
    assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(body["output_config"], json!({"effort": "xhigh"}));

    let mut budget = model_at(&server.base_url, "claude-haiku-4-5");
    budget.compat.get_or_insert_with(Default::default).force_adaptive_thinking = Some(false);
    budget.reasoning = true;
    anthropic_messages::stream_simple(
        &budget,
        &context("x"),
        StreamOptions { api_key: Some("k".into()), reasoning: Some(ThinkingLevel::Low), ..Default::default() },
    )
    .result()
    .await;
    let request = server.requests.lock()[1].clone();
    let body = request.json();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 2048);
    assert!(request.header("anthropic-beta").unwrap().contains("interleaved-thinking-2025-05-14"));
}

#[tokio::test]
async fn missing_api_key_errors() {
    let model = model_at("http://127.0.0.1:9", "claude-haiku-4-5");
    let result = anthropic_messages::stream_simple(&model, &context("x"), StreamOptions::default()).result().await;
    assert_eq!(result.error_message.as_deref(), Some("No API key for provider: anthropic"));
}

/// The federation client (and its token cache) is one process-wide slot, as in the SDK: tests that
/// use different configs would evict each other's cached token when run in parallel.
static FEDERATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn federation_env(token_file: &std::path::Path, workspace: Option<&str>) -> ProviderEnv {
    let mut env = ProviderEnv::new();
    env.insert("ANTHROPIC_FEDERATION_RULE_ID".into(), "fdrl_rule".into());
    env.insert("ANTHROPIC_ORGANIZATION_ID".into(), "org-uuid".into());
    env.insert("ANTHROPIC_IDENTITY_TOKEN_FILE".into(), token_file.to_string_lossy().into_owned());
    env.insert("ANTHROPIC_SERVICE_ACCOUNT_ID".into(), "svac_account".into());
    if let Some(workspace) = workspace {
        env.insert("ANTHROPIC_WORKSPACE_ID".into(), workspace.into());
    }
    env
}

fn identity_token_file(name: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-ai-federation-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("token.jwt");
    std::fs::write(&path, contents).unwrap();
    path
}

/// Workload identity federation: the SDK exchanges the identity token at `/v1/oauth/token`,
/// sends the access token as a bearer with the OAuth beta, and caches it across requests.
#[tokio::test]
async fn workload_identity_federation_exchanges_and_caches_the_token() {
    let _federation = FEDERATION_LOCK.lock().await;
    let token = r#"{"access_token":"fed-access","expires_in":3600,"token_type":"Bearer"}"#;
    let server = serve(vec![
        MockResponse::json(200, token),
        MockResponse::sse(minimal_events()),
        MockResponse::sse(minimal_events()),
    ])
    .await;
    let model = model_at(&server.base_url, "claude-haiku-4-5");
    let token_file = identity_token_file("cache", "  header.payload.sig\n");
    let options = || StreamOptions { env: Some(federation_env(&token_file, Some("wrkspc_1"))), ..Default::default() };
    for _ in 0..2 {
        let result = anthropic_messages::stream(&model, &context("Hello"), options()).result().await;
        assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    }

    let requests = server.requests.lock().clone();
    assert_eq!(requests.len(), 3);
    let exchange = &requests[0];
    assert_eq!(exchange.path, "/v1/oauth/token");
    assert_eq!(exchange.header("anthropic-beta"), Some("oauth-2025-04-20,oidc-federation-2026-04-01"));
    assert_eq!(exchange.header("user-agent"), Some("Anthropic/JS 0.129.0"));
    assert_eq!(exchange.header("content-type"), Some("application/json"));
    assert_eq!(
        exchange.json(),
        json!({
            "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
            "assertion": "header.payload.sig",
            "federation_rule_id": "fdrl_rule",
            "organization_id": "org-uuid",
            "service_account_id": "svac_account",
            "workspace_id": "wrkspc_1"
        })
    );
    for request in &requests[1..] {
        assert_eq!(request.path, "/v1/messages?beta=true");
        assert_eq!(request.header("authorization"), Some("Bearer fed-access"));
        assert_eq!(request.header("x-api-key"), None);
        assert!(request.header("anthropic-beta").unwrap().split(',').any(|b| b == "oauth-2025-04-20"));
    }
}

#[tokio::test]
async fn workload_identity_federation_reexchanges_after_401_and_reports_exchange_errors() {
    let _federation = FEDERATION_LOCK.lock().await;
    let server = serve(vec![
        MockResponse::json(200, r#"{"access_token":"first","expires_in":3600}"#),
        MockResponse::json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid token"}}"#),
        MockResponse::json(200, r#"{"access_token":"second","expires_in":3600}"#),
        MockResponse::sse(minimal_events()),
        MockResponse::json(401, r#"{"error":"invalid_grant","assertion":"leaked"}"#),
    ])
    .await;
    let model = model_at(&server.base_url, "claude-haiku-4-5");
    let token_file = identity_token_file("retry", "jwt");
    let options = || StreamOptions { env: Some(federation_env(&token_file, None)), ..Default::default() };

    let first = anthropic_messages::stream(&model, &context("Hello"), options()).result().await;
    assert_eq!(first.stop_reason, StopReason::Error);
    let second = anthropic_messages::stream(&model, &context("Hello"), options()).result().await;
    assert_eq!(second.stop_reason, StopReason::Stop, "{:?}", second.error_message);
    let requests = server.requests.lock().clone();
    assert_eq!(requests[3].header("authorization"), Some("Bearer second"));
    assert!(requests[2].json().get("workspace_id").is_none());

    // A rejected exchange surfaces the SDK's redacted error with the 401 hint.
    let model = model_at(&server.base_url, "claude-sonnet-4-5");
    let other_file = identity_token_file("reject", "jwt2");
    let rejected = anthropic_messages::stream(
        &model,
        &context("Hello"),
        StreamOptions { env: Some(federation_env(&other_file, None)), ..Default::default() },
    )
    .result()
    .await;
    assert_eq!(rejected.stop_reason, StopReason::Error);
    assert_eq!(
        rejected.error_message.as_deref(),
        Some(
            "Token exchange failed with status 401: {\"error\":\"invalid_grant\"} Ensure your federation rule matches your identity token. If your federation rule is scoped to multiple workspaces, set the ANTHROPIC_WORKSPACE_ID environment variable, the 'workspace_id' config key, or the `workspaceId` option. View your authentication events in the Workload identity page of Claude Console for more details."
        )
    );
}

#[tokio::test]
async fn workload_identity_federation_requires_a_readable_identity_token() {
    let _federation = FEDERATION_LOCK.lock().await;
    let model = model_at("https://api.anthropic.com", "claude-haiku-4-5");
    let missing = std::env::temp_dir().join("pi-ai-federation-missing/token.jwt");
    let result = anthropic_messages::stream(
        &model,
        &context("Hello"),
        StreamOptions { env: Some(federation_env(&missing, None)), ..Default::default() },
    )
    .result()
    .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message,
        Some(format!(
            "Failed to read identity token file at {0}: Error: ENOENT: no such file or directory, open '{0}'",
            missing.display()
        ))
    );
}
