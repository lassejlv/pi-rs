//! Ports of the Anthropic Messages tests from `packages/ai/test/anthropic-*.test.ts`.

use std::sync::Arc;

use parking_lot::Mutex;
use pi_ai::api::anthropic_messages;
use pi_ai::auth::ApiKeyResolveInput;
use pi_ai::providers::all::{get_builtin_model, get_builtin_models, get_builtin_providers};
use pi_ai::providers::anthropic::anthropic_provider;
use pi_ai::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::mock_http::{MockServer, Response};
use crate::support::*;

fn builtin(provider: &str, id: &str) -> Model {
    get_builtin_model(provider, id).unwrap_or_else(|| panic!("{provider}/{id}"))
}

fn key(key: &str) -> StreamOptions {
    StreamOptions { api_key: Some(key.into()), ..Default::default() }
}

fn hello() -> TranscriptContext {
    normalize_context(&user_ctx("Hello"))
}

async fn anthropic_payload(model: &Model, context: &TranscriptContext, options: StreamOptions) -> (Value, AssistantMessage) {
    let mut model = model.clone();
    model.base_url = "http://127.0.0.1:9".into();
    let options = StreamOptions { api_key: options.api_key.clone().or(Some("test-key".into())), ..options };
    capture_payload_with(|o| anthropic_messages::stream(&model, context, o), options).await
}

async fn sse_server(body: String) -> MockServer {
    MockServer::start(move |_, _| Response::sse(&body)).await
}

// ---------------------------------------------------------------------------
// Port of anthropic-adaptive-thinking-models.test.ts
// ---------------------------------------------------------------------------

#[test]
fn marks_builtin_anthropic_messages_models_that_use_adaptive_thinking() {
    let expected = [
        "anthropic/claude-fable-5",
        "anthropic/claude-haiku-5-5",
        "anthropic/claude-opus-4-8",
        "anthropic/claude-opus-5",
        "anthropic/claude-sonnet-5",
        "cloudflare-ai-gateway/claude-fable-5",
        "fireworks/accounts/fireworks/models/deepseek-v4p1-flash",
        "fireworks/accounts/fireworks/models/gpt-oss-120b",
        "fireworks/accounts/fireworks/models/qwen3p8-max",
        "minimax-cn/MiniMax-M3.1-Flash",
        "minimax/MiniMax-M3.1-Flash",
        "opencode/claude-opus-4-8",
        "opencode/claude-opus-5",
        "vercel-ai-gateway/anthropic/claude-opus-4.8",
        "vercel-ai-gateway/anthropic/claude-opus-5",
        "vercel-ai-gateway/anthropic/claude-sonnet-5",
    ];
    let mut flagged: Vec<String> = get_builtin_providers()
        .iter()
        .flat_map(|p| get_builtin_models(p))
        .filter(|m| m.api == "anthropic-messages")
        .filter(|m| m.compat.as_ref().and_then(|c| c.force_adaptive_thinking) == Some(true))
        .map(|m| format!("{}/{}", m.provider, m.id))
        .collect();
    flagged.sort();
    // The checked-in catalog (identical to the TypeScript one) does not yet contain
    // MiniMax-M3.1-Flash, which only `scripts/generate-models.ts` produces; check the rest.
    let all: Vec<String> = get_builtin_providers()
        .iter()
        .flat_map(|p| get_builtin_models(p).into_iter().map(move |m| format!("{p}/{}", m.id)))
        .collect();
    for id in expected {
        if id.contains("MiniMax-M3.1-Flash") && !all.iter().any(|m| m == id) {
            continue;
        }
        assert!(flagged.iter().any(|f| f == id), "missing {id}");
    }
    let pattern = regex::Regex::new(r"(opus[-.](4[-.][678]|5)|sonnet[-.]4[-.]6|sonnet[-.]5|haiku[-.]5[-.]5|fable[-.]5)").unwrap();
    for id in &flagged {
        assert!(
            id.starts_with("fireworks/") || id.starts_with("minimax/") || id.starts_with("minimax-cn/") || pattern.is_match(id),
            "unexpected adaptive model {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// Port of anthropic-auth-token.test.ts
// ---------------------------------------------------------------------------

async fn resolve_anthropic(pairs: &[(&str, &str)]) -> Option<pi_ai::auth::AuthResult> {
    let provider = anthropic_provider();
    let auth = provider.auth().api_key.clone().unwrap();
    auth.resolve(ApiKeyResolveInput { ctx: MapAuthContext::new(pairs), credential: None, signal: CancellationToken::new() })
        .await
        .unwrap()
}

fn auth_header(value: &str) -> ProviderHeaders {
    let mut headers = ProviderHeaders::new();
    headers.insert("Authorization".into(), Some(value.into()));
    headers
}

fn test_anthropic_model(base_url: &str) -> Model {
    model(json!({
        "id": "claude-test", "name": "Claude Test", "api": "anthropic-messages", "provider": "anthropic",
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 100000, "maxTokens": 4096
    }))
}

fn system_hello() -> Context {
    ctx(json!({"systemPrompt": "System prompt.", "messages": [{"role": "user", "content": "Hello"}]}))
}

fn models_with_env(pairs: &[(&str, &str)]) -> Models {
    let models = create_models(CreateModelsOptions { auth_context: Some(MapAuthContext::new(pairs)), ..Default::default() });
    models.set_provider(anthropic_provider());
    models
}

fn betas(request: &crate::mock_http::Request) -> Vec<String> {
    request.header("anthropic-beta").map(|b| b.split(',').map(|s| s.trim().to_string()).collect()).unwrap_or_default()
}

#[tokio::test]
async fn auth_token_env_resolves_as_bearer_authorization_header() {
    let auth = resolve_anthropic(&[
        ("ANTHROPIC_AUTH_TOKEN", "auth-token"),
        ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
        ("ANTHROPIC_API_KEY", "api-key"),
    ])
    .await
    .unwrap();
    assert_eq!(auth.auth.headers, Some(auth_header("Bearer auth-token")));
    assert_eq!(auth.auth.api_key, None);
    assert_eq!(auth.env, None);
    assert_eq!(auth.source.as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));
}

#[tokio::test]
async fn oauth_token_env_is_preserved_as_api_key_auth() {
    let auth = resolve_anthropic(&[("ANTHROPIC_OAUTH_TOKEN", "oauth-token"), ("ANTHROPIC_API_KEY", "api-key")]).await.unwrap();
    assert_eq!(auth.auth.api_key.as_deref(), Some("oauth-token"));
    assert_eq!(auth.auth.headers, None);
    assert_eq!(auth.source.as_deref(), Some("ANTHROPIC_OAUTH_TOKEN"));
}

#[tokio::test]
async fn authorization_headers_skip_oauth_request_shaping() {
    let server = sse_server(anthropic_ok_sse()).await;
    let model = test_anthropic_model(&server.url);
    let options = StreamOptions { headers: Some(auth_header("Bearer gateway-token")), ..Default::default() };
    let result = anthropic_messages::stream(&model, &normalize_context(&system_hello()), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let request = server.request(0);
    assert_eq!(request.header("authorization"), Some("Bearer gateway-token"));
    assert_eq!(request.header("x-api-key"), None);
    assert!(!betas(&request).contains(&"oauth-2025-04-20".to_string()));
    let body = request.json();
    assert_eq!(body["system"].as_array().unwrap().len(), 1);
    assert_eq!(body["system"][0]["text"], "System prompt.");
}

#[tokio::test]
async fn auth_context_auth_token_threads_through_request_headers() {
    let server = sse_server(anthropic_ok_sse()).await;
    let models = models_with_env(&[("ANTHROPIC_AUTH_TOKEN", "ctx-token")]);
    let result = models.stream_simple(&test_anthropic_model(&server.url), &system_hello(), StreamOptions::default()).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let request = server.request(0);
    assert_eq!(request.header("authorization"), Some("Bearer ctx-token"));
    assert_eq!(request.header("x-api-key"), None);
    assert!(!betas(&request).contains(&"oauth-2025-04-20".to_string()));
    let body = request.json();
    assert_eq!(body["system"].as_array().unwrap().len(), 1);
    assert_eq!(body["system"][0]["text"], "System prompt.");
}

#[tokio::test]
async fn auth_context_oauth_token_keeps_oauth_request_shaping() {
    let server = sse_server(anthropic_ok_sse()).await;
    let models = models_with_env(&[("ANTHROPIC_OAUTH_TOKEN", "sk-ant-oat-test")]);
    let result = models.stream_simple(&test_anthropic_model(&server.url), &system_hello(), StreamOptions::default()).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let request = server.request(0);
    assert_eq!(request.header("authorization"), Some("Bearer sk-ant-oat-test"));
    assert_eq!(request.header("x-api-key"), None);
    assert!(betas(&request).contains(&"oauth-2025-04-20".to_string()));
}

#[tokio::test]
async fn explicit_request_headers_override_auth_token_env() {
    let server = sse_server(anthropic_ok_sse()).await;
    let models = models_with_env(&[("ANTHROPIC_AUTH_TOKEN", "ctx-token")]);
    let options = StreamOptions { headers: Some(auth_header("Bearer explicit-token")), ..Default::default() };
    models.stream_simple(&test_anthropic_model(&server.url), &system_hello(), options).result().await;
    assert_eq!(server.request(0).header("authorization"), Some("Bearer explicit-token"));
}

#[tokio::test]
async fn uses_pi_user_agent_by_default() {
    let server = sse_server(anthropic_ok_sse()).await;
    anthropic_messages::stream(&test_anthropic_model(&server.url), &normalize_context(&system_hello()), key("anthropic-key")).result().await;
    assert_eq!(server.request(0).header("user-agent"), Some(format_user_agent(None).as_str()));
}

#[tokio::test]
async fn explicit_headers_override_the_default_user_agent() {
    let server = sse_server(anthropic_ok_sse()).await;
    let mut model = test_anthropic_model(&server.url);
    model.id = "kimi-for-coding".into();
    model.provider = "kimi-coding".into();
    let mut headers = ProviderHeaders::new();
    headers.insert("User-Agent".into(), Some("custom-client".into()));
    let options = StreamOptions { headers: Some(headers), ..key("kimi-key") };
    anthropic_messages::stream(&model, &normalize_context(&system_hello()), options).result().await;
    let request = server.request(0);
    assert_eq!(request.header("user-agent"), Some("custom-client"));
    assert_eq!(request.header_count("user-agent"), 1);
}

#[tokio::test]
async fn explicit_beta_header_replaces_and_suppresses_betas() {
    let server = sse_server(anthropic_ok_sse()).await;
    let model = test_anthropic_model(&server.url);
    let mut headers = ProviderHeaders::new();
    headers.insert("anthropic-beta".into(), Some("custom-beta".into()));
    let options = StreamOptions { headers: Some(headers), ..key("anthropic-key") };
    anthropic_messages::stream(&model, &normalize_context(&system_hello()), options).result().await;
    assert_eq!(server.request(0).header("anthropic-beta"), Some("custom-beta"));

    let mut headers = ProviderHeaders::new();
    headers.insert("anthropic-beta".into(), None);
    let options = StreamOptions { headers: Some(headers), ..key("anthropic-key") };
    anthropic_messages::stream(&model, &normalize_context(&system_hello()), options).result().await;
    assert_eq!(server.request(1).header("anthropic-beta"), None);
}

// ---------------------------------------------------------------------------
// Port of anthropic-cache-write-1h-cost.test.ts
// ---------------------------------------------------------------------------

fn cache_creation_events(cache_creation: Option<Value>) -> String {
    let mut start_usage = json!({"input_tokens": 100, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 1_000_000});
    if let Some(breakdown) = cache_creation {
        start_usage["cache_creation"] = breakdown;
    }
    anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_test", "usage": start_usage}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 100, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 1_000_000}}),
        json!({"type": "message_stop"}),
    ])
}

async fn stream_against(model: &Model, body: String) -> AssistantMessage {
    let server = sse_server(body).await;
    let mut model = model.clone();
    model.base_url = server.url.clone();
    anthropic_messages::stream(&model, &normalize_context(&user_ctx("hi")), key("k")).result().await
}

#[tokio::test]
async fn prices_1h_cache_writes_at_twice_input() {
    let model = builtin("anthropic", "claude-opus-4-8");
    let result = stream_against(
        &model,
        cache_creation_events(Some(json!({"ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000}))),
    )
    .await;
    assert_eq!(result.usage.cache_write, 1_000_000);
    assert_eq!(result.usage.cache_write_1h, Some(400_000));
    assert!((result.usage.cost.cache_write - 7.75).abs() < 1e-10, "{}", result.usage.cost.cache_write);
}

#[tokio::test]
async fn prices_1h_cache_writes_reported_only_in_message_delta() {
    // Regression for #9210: Vercel AI Gateway sends cache usage in message_delta.
    let model = builtin("vercel-ai-gateway", "anthropic/claude-haiku-4.5");
    let body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_test", "usage": {"input_tokens": 0, "output_tokens": 0}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 3, "output_tokens": 4, "cache_creation_input_tokens": 6535, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 6535}}}),
        json!({"type": "message_stop"}),
    ]);
    let result = stream_against(&model, body).await;
    assert_eq!(result.usage.cache_write, 6535);
    assert_eq!(result.usage.cache_write_1h, Some(6535));
    let expected = 6535.0 * model.cost.input * 2.0 / 1_000_000.0;
    assert!((result.usage.cost.cache_write - expected).abs() < 1e-10);
}

#[tokio::test]
async fn falls_back_to_5m_rate_without_a_breakdown() {
    let model = builtin("anthropic", "claude-opus-4-8");
    let result = stream_against(&model, cache_creation_events(None)).await;
    assert_eq!(result.usage.cache_write, 1_000_000);
    assert_eq!(result.usage.cache_write_1h.unwrap_or(0), 0);
    assert!((result.usage.cost.cache_write - 6.25).abs() < 1e-10);
}

// ---------------------------------------------------------------------------
// Port of anthropic-eager-tool-input-compat.test.ts
// ---------------------------------------------------------------------------

fn eager_model(base_url: &str, compat: Value) -> Model {
    let mut compat_obj = json!({"forceAdaptiveThinking": true});
    for (k, v) in compat.as_object().unwrap() {
        compat_obj[k] = v.clone();
    }
    model(json!({
        "id": "claude-opus-4-8", "name": "Claude Opus 4.8", "api": "anthropic-messages", "provider": "test-anthropic",
        "baseUrl": base_url, "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 32000,
        "compat": compat_obj
    }))
}

fn lookup_tool() -> Value {
    json!({"name": "lookup", "description": "Look up a value", "parameters": {"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}})
}

async fn capture_eager(compat: Value, tools: bool) -> crate::mock_http::Request {
    let server = MockServer::start(|_, _| Response::sse("")).await;
    let mut context = json!({"messages": [{"role": "user", "content": "Use the tool"}]});
    if tools {
        context["tools"] = json!([lookup_tool()]);
    }
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..key("test-key") };
    anthropic_messages::stream(&eager_model(&server.url, compat), &normalize_context(&ctx(context)), options).result().await;
    server.request(0)
}

#[tokio::test]
async fn sends_per_tool_eager_input_streaming_by_default() {
    let request = capture_eager(json!({}), true).await;
    assert_eq!(request.json()["tools"][0]["eager_input_streaming"], true);
    assert_eq!(request.header("anthropic-beta"), None);
}

#[tokio::test]
async fn uses_legacy_fine_grained_beta_when_eager_streaming_is_disabled() {
    let request = capture_eager(json!({"supportsEagerToolInputStreaming": false}), true).await;
    assert!(request.json()["tools"][0].get("eager_input_streaming").is_none());
    assert_eq!(request.header("anthropic-beta"), Some("fine-grained-tool-streaming-2025-05-14"));
}

#[tokio::test]
async fn omits_the_legacy_beta_without_tools() {
    let request = capture_eager(json!({"supportsEagerToolInputStreaming": false}), false).await;
    assert!(request.json().get("tools").is_none());
    assert_eq!(request.header("anthropic-beta"), None);
}

// ---------------------------------------------------------------------------
// Port of anthropic-empty-thinking-signature-compat.test.ts
// ---------------------------------------------------------------------------

fn mimo_model(allow_empty_signature: Option<bool>) -> Model {
    let mut value = json!({
        "id": "mimo-v2.6-pro", "name": "MiMo-V2.5-Pro", "api": "anthropic-messages", "provider": "xiaomi-token-plan-ams",
        "baseUrl": "http://127.0.0.1:9/anthropic", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1048576, "maxTokens": 1024
    });
    if let Some(allow) = allow_empty_signature {
        value["compat"] = json!({"allowEmptySignature": allow});
    }
    model(value)
}

fn signature_context(signature: &str, thinking: &str, provider: &str, model_id: &str, extra: Option<Value>) -> Context {
    let mut content = vec![json!({"type": "thinking", "thinking": thinking, "thinkingSignature": signature})];
    if let Some(extra) = extra {
        content.push(extra);
    }
    ctx(json!({"messages": [
        {"role": "user", "content": "first"},
        {"role": "assistant", "content": content, "provider": provider, "api": "anthropic-messages", "model": model_id},
        {"role": "user", "content": "second"}
    ]}))
}

fn assistant_content(payload: &Value) -> Value {
    payload["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap()["content"].clone()
}

#[tokio::test]
async fn converts_empty_signature_thinking_to_text_by_default() {
    let payload = simple_payload(
        &mimo_model(None),
        &signature_context("", "internal reasoning", "xiaomi-token-plan-ams", "mimo-v2.6-pro", None),
        StreamOptions::default(),
    )
    .await;
    assert_eq!(assistant_content(&payload), json!([{"type": "text", "text": "internal reasoning"}]));
}

#[tokio::test]
async fn preserves_empty_thinking_text_with_a_signature() {
    let payload = simple_payload(
        &mimo_model(None),
        &signature_context("signed-thinking", "", "xiaomi-token-plan-ams", "mimo-v2.6-pro", None),
        StreamOptions::default(),
    )
    .await;
    assert_eq!(assistant_content(&payload), json!([{"type": "thinking", "thinking": "", "signature": "signed-thinking"}]));
}

#[tokio::test]
async fn preserves_empty_signature_thinking_when_allowed() {
    let payload = simple_payload(
        &mimo_model(Some(true)),
        &signature_context(" ", "internal reasoning", "xiaomi-token-plan-ams", "mimo-v2.6-pro", None),
        StreamOptions::default(),
    )
    .await;
    assert_eq!(assistant_content(&payload), json!([{"type": "thinking", "thinking": "internal reasoning", "signature": ""}]));
}

#[test]
fn allows_empty_thinking_signatures_for_every_vercel_ai_gateway_model() {
    // Regression for #9676.
    let models = get_builtin_models("vercel-ai-gateway");
    assert!(!models.is_empty());
    for model in models {
        assert_eq!(model.compat.as_ref().and_then(|c| c.allow_empty_signature), Some(true), "{}", model.id);
    }
}

#[tokio::test]
async fn preserves_unsigned_thinking_for_fireworks_models() {
    // Regression for #9323.
    for id in [
        "accounts/fireworks/models/deepseek-v4p1-flash",
        "accounts/fireworks/models/qwen3p8-max",
        "accounts/fireworks/models/qwen3p8-2p4t-a95b",
        "accounts/fireworks/models/nemotron-3-ultra-nvfp4",
    ] {
        let model = builtin("fireworks", id);
        assert_eq!(model.compat.as_ref().and_then(|c| c.allow_empty_signature), Some(true), "{id}");
        let context = signature_context("", "internal reasoning", "fireworks", id, Some(json!({"type": "text", "text": "answer"})));
        let payload = simple_payload(&model, &context, StreamOptions::default()).await;
        assert_eq!(
            assistant_content(&payload),
            json!([
                {"type": "thinking", "thinking": "internal reasoning", "signature": ""},
                {"type": "text", "text": "answer"}
            ]),
            "{id}"
        );
    }
}

// ---------------------------------------------------------------------------
// Port of anthropic-error-tool-result-images.test.ts
// ---------------------------------------------------------------------------

fn image_test_model() -> Model {
    model(json!({
        "id": "claude-test", "name": "Claude Test", "api": "anthropic-messages", "provider": "anthropic",
        "baseUrl": "http://127.0.0.1:9", "reasoning": false, "input": ["text", "image"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 1024
    }))
}

fn two_call_assistant() -> Value {
    json!({"role": "assistant", "content": [
        {"type": "toolCall", "id": "call-1", "name": "codemode", "arguments": {}},
        {"type": "toolCall", "id": "call-2", "name": "read", "arguments": {}}
    ], "stopReason": "toolUse"})
}

fn png(data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}})
}

#[tokio::test]
async fn moves_error_tool_result_images_after_the_turns_tool_results() {
    // Regression for #10794.
    let context = ctx(json!({"messages": [
        {"role": "user", "content": "Show the screenshots"},
        two_call_assistant(),
        {"role": "toolResult", "toolCallId": "call-1", "toolName": "codemode", "isError": true, "content": [
            {"type": "text", "text": "Script failed"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "image", "data": "CCCC", "mimeType": "image/png"},
            {"type": "text", "text": "Script error"}
        ]},
        {"role": "toolResult", "toolCallId": "call-2", "toolName": "read", "isError": false, "content": [
            {"type": "image", "data": "BBBB", "mimeType": "image/png"}
        ]}
    ]}));
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..Default::default() };
    let payload = simple_payload(&image_test_model(), &context, options).await;
    assert_eq!(
        payload["messages"][2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "call-1", "content": "Script failed\n(image attached after the tool results)\nScript error", "is_error": true},
            {"type": "tool_result", "tool_use_id": "call-2", "content": [{"type": "text", "text": "(see attached image)"}, png("BBBB")], "is_error": false},
            {"type": "text", "text": "Images from the error result of tool call call-1 (codemode):"},
            png("AAAA"),
            png("CCCC")
        ]})
    );
}

#[tokio::test]
async fn keeps_error_tool_results_without_images_unchanged() {
    let mut assistant = two_call_assistant();
    assistant["content"] = json!([assistant["content"][0].clone()]);
    let context = ctx(json!({"messages": [
        {"role": "user", "content": "Run it"},
        assistant,
        {"role": "toolResult", "toolCallId": "call-1", "toolName": "codemode", "isError": true, "content": [{"type": "text", "text": "Script failed"}]}
    ]}));
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..Default::default() };
    let payload = simple_payload(&image_test_model(), &context, options).await;
    assert_eq!(
        payload["messages"][2],
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": "Script failed", "is_error": true}]})
    );
}

// ---------------------------------------------------------------------------
// Port of anthropic-federation.test.ts and anthropic-federation-sdk.test.ts
// ---------------------------------------------------------------------------

const FEDERATION: [(&str, &str); 4] = [
    ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_test"),
    ("ANTHROPIC_ORGANIZATION_ID", "org-test"),
    ("ANTHROPIC_SERVICE_ACCOUNT_ID", "svac_test"),
    ("ANTHROPIC_IDENTITY_TOKEN_FILE", "/tmp/identity.jwt"),
];

fn env_of(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
async fn federation_variables_resolve_as_provider_env_without_request_auth() {
    let result = resolve_anthropic(&FEDERATION).await.unwrap();
    assert_eq!(result.auth, Default::default());
    assert_eq!(result.env, Some(env_of(&FEDERATION)));
    assert_eq!(result.source.as_deref(), Some("workload identity federation"));

    let mut with_workspace = FEDERATION.to_vec();
    with_workspace.push(("ANTHROPIC_WORKSPACE_ID", "wrkspc_test"));
    let result = resolve_anthropic(&with_workspace).await.unwrap();
    assert_eq!(result.env.unwrap().get("ANTHROPIC_WORKSPACE_ID").map(String::as_str), Some("wrkspc_test"));

    let partial: Vec<_> = FEDERATION.iter().copied().filter(|(k, _)| *k != "ANTHROPIC_IDENTITY_TOKEN_FILE").collect();
    assert!(resolve_anthropic(&partial).await.is_none());

    let no_account: Vec<_> = FEDERATION.iter().copied().filter(|(k, _)| *k != "ANTHROPIC_SERVICE_ACCOUNT_ID").collect();
    let result = resolve_anthropic(&no_account).await.unwrap();
    assert_eq!(result.env, Some(env_of(&no_account)));
    assert_eq!(result.source.as_deref(), Some("workload identity federation"));
}

#[tokio::test]
async fn api_key_and_auth_token_take_precedence_over_federation() {
    let mut with_key = FEDERATION.to_vec();
    with_key.push(("ANTHROPIC_API_KEY", "api-key"));
    let result = resolve_anthropic(&with_key).await.unwrap();
    assert_eq!(result.auth.api_key.as_deref(), Some("api-key"));
    assert_eq!(result.env, None);
    assert_eq!(result.source.as_deref(), Some("ANTHROPIC_API_KEY"));

    let mut with_token = FEDERATION.to_vec();
    with_token.push(("ANTHROPIC_AUTH_TOKEN", "auth-token"));
    let result = resolve_anthropic(&with_token).await.unwrap();
    assert_eq!(result.auth.headers, Some(auth_header("Bearer auth-token")));
    assert_eq!(result.source.as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));
}

/// The federation client is a single module-level slot (like TS); exchanges from parallel tests
/// with different configs would replace each other's cached token.
static FEDERATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn identity_file(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("pi-ai-ts-federation-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("identity.jwt");
    std::fs::write(&path, "header.payload.signature").unwrap();
    path.to_string_lossy().into_owned()
}

fn federation_server() -> impl Fn(&crate::mock_http::Request, usize) -> Response + Send + Sync + 'static {
    |request, _| {
        if request.path_only() == "/v1/oauth/token" {
            Response::json(200, json!({"access_token": "federated-token", "expires_in": 3600}))
        } else {
            Response::sse(&anthropic_ok_sse())
        }
    }
}

#[tokio::test]
async fn federation_without_service_account_and_through_models() {
    let _federation = FEDERATION_LOCK.lock().await;
    // Optional service account (the exchange omits it) and authContext federation through Models.
    let file = identity_file("optional");
    let server = MockServer::start(federation_server()).await;
    let model = test_anthropic_model(&server.url);
    let env: Vec<(&str, &str)> = vec![
        ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_test"),
        ("ANTHROPIC_ORGANIZATION_ID", "org-test"),
        ("ANTHROPIC_IDENTITY_TOKEN_FILE", file.as_str()),
    ];
    let options = StreamOptions { env: Some(env_of(&env)), ..Default::default() };
    let result = anthropic_messages::stream(&model, &normalize_context(&system_hello()), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let exchange = server.request(0).json();
    assert_eq!(exchange["federation_rule_id"], "fdrl_test");
    assert_eq!(exchange["organization_id"], "org-test");
    assert!(exchange.get("service_account_id").is_none());
    assert!(exchange.get("workspace_id").is_none());

    let file = identity_file("models");
    let server = MockServer::start(federation_server()).await;
    let mut env = FEDERATION.to_vec();
    env[3] = ("ANTHROPIC_IDENTITY_TOKEN_FILE", file.as_str());
    let models = models_with_env(&env);
    let result = models.stream_simple(&test_anthropic_model(&server.url), &system_hello(), StreamOptions::default()).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let requests = server.requests();
    assert_eq!(requests[0].path_only(), "/v1/oauth/token");
    assert_eq!(requests[0].json()["service_account_id"], "svac_test");
    assert_eq!(requests[1].header("authorization"), Some("Bearer federated-token"));
    assert_eq!(requests[1].header("x-api-key"), None);
}

#[tokio::test]
async fn explicit_api_key_wins_over_federation_env() {
    let _federation = FEDERATION_LOCK.lock().await;
    let server = MockServer::start(federation_server()).await;
    let options = StreamOptions { env: Some(env_of(&FEDERATION)), ..key("explicit-key") };
    let result = anthropic_messages::stream(&test_anthropic_model(&server.url), &hello(), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("x-api-key"), Some("explicit-key"));
}

#[tokio::test]
async fn does_not_federate_other_anthropic_messages_providers() {
    let server = MockServer::start(federation_server()).await;
    let mut kimi = test_anthropic_model(&server.url);
    kimi.provider = "kimi-coding".into();
    let options = StreamOptions { env: Some(env_of(&FEDERATION)), ..Default::default() };
    let result = anthropic_messages::stream(&kimi, &hello(), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn federation_exchanges_the_identity_token_once_across_requests() {
    let _federation = FEDERATION_LOCK.lock().await;
    let file = identity_file("once");
    let server = MockServer::start(federation_server()).await;
    let model = test_anthropic_model(&server.url);
    let mut env = FEDERATION.to_vec();
    env[3] = ("ANTHROPIC_IDENTITY_TOKEN_FILE", file.as_str());
    for _ in 0..3 {
        let options = StreamOptions { env: Some(env_of(&env)), ..Default::default() };
        let message = anthropic_messages::stream(&model, &normalize_context(&system_hello()), options).result().await;
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    }
    let requests = server.requests();
    assert_eq!(requests.iter().filter(|r| r.path_only() == "/v1/oauth/token").count(), 1);
    let messages: Vec<_> = requests.iter().filter(|r| r.path_only() == "/v1/messages").collect();
    assert_eq!(messages.len(), 3);
    for request in messages {
        assert_eq!(request.header("authorization"), Some("Bearer federated-token"));
    }
}

#[tokio::test]
async fn header_owned_auth_skips_the_federation_credential_chain() {
    let _federation = FEDERATION_LOCK.lock().await;
    let _lock = ENV_LOCK.lock().await;
    let file = identity_file("header");
    let mut env = FEDERATION.to_vec();
    env[3] = ("ANTHROPIC_IDENTITY_TOKEN_FILE", file.as_str());
    let _guard = EnvGuard::set(&env.iter().map(|(k, v)| (*k, Some(*v))).collect::<Vec<_>>());
    let server = MockServer::start(federation_server()).await;
    let options = StreamOptions { headers: Some(auth_header("Bearer auth-token")), ..Default::default() };
    let message = anthropic_messages::stream(&test_anthropic_model(&server.url), &normalize_context(&system_hello()), options).result().await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path_only(), "/v1/messages");
    assert_eq!(requests[0].header("authorization"), Some("Bearer auth-token"));
}

// ---------------------------------------------------------------------------
// Port of anthropic-force-adaptive-thinking.test.ts
// ---------------------------------------------------------------------------

fn vendor_model(id: &str, compat: Option<Value>) -> Model {
    let mut value = json!({
        "id": id, "name": "Vendor Proxy", "api": "anthropic-messages", "provider": "vendor-proxy",
        "baseUrl": "http://127.0.0.1:9", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 32000
    });
    if let Some(compat) = compat {
        value["compat"] = compat;
    }
    model(value)
}

fn reasoning(level: ThinkingLevel) -> StreamOptions {
    StreamOptions { reasoning: Some(level), ..Default::default() }
}

#[tokio::test]
async fn force_adaptive_thinking_compat_override() {
    let payload = simple_payload(&vendor_model("vendor--claude-opus-latest", None), &user_ctx("Hello"), reasoning(ThinkingLevel::Medium)).await;
    assert_eq!(payload["thinking"]["type"], "enabled");
    assert!(payload.get("output_config").is_none());

    let forced = vendor_model("vendor--claude-opus-latest", Some(json!({"forceAdaptiveThinking": true})));
    let payload = simple_payload(&forced, &user_ctx("Hello"), reasoning(ThinkingLevel::Medium)).await;
    assert_eq!(payload["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(payload["output_config"], json!({"effort": "medium"}));

    let payload = simple_payload(&builtin("anthropic", "claude-fable-5"), &user_ctx("Hello"), reasoning(ThinkingLevel::Xhigh)).await;
    assert_eq!(payload["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(payload["output_config"], json!({"effort": "xhigh"}));

    let mut opted_out = builtin("anthropic", "claude-opus-4-8");
    opted_out.compat = Some(serde_json::from_value(json!({"forceAdaptiveThinking": false})).unwrap());
    let payload = simple_payload(&opted_out, &user_ctx("Hello"), reasoning(ThinkingLevel::Medium)).await;
    assert_eq!(payload["thinking"]["type"], "enabled");
    assert!(payload.get("output_config").is_none());

    let payload = simple_payload(&forced, &user_ctx("Hello"), StreamOptions::default()).await;
    assert_eq!(payload["thinking"], json!({"type": "disabled"}));
    assert!(payload.get("output_config").is_none());
}

// ---------------------------------------------------------------------------
// Port of anthropic-temperature-compat.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn temperature_compatibility() {
    let temperature = |t: f64| StreamOptions { temperature: Some(t), ..Default::default() };
    for (id, t, kept) in [
        ("claude-opus-4-7", 0.0, false),
        ("claude-opus-4-8", 0.0, false),
        ("claude-opus-4-7", 1.0, false),
        ("claude-opus-4-6", 0.0, true),
        ("claude-sonnet-4-6", 0.0, true),
    ] {
        let payload = simple_payload(&builtin("anthropic", id), &user_ctx("Hello"), temperature(t)).await;
        if kept {
            assert_eq!(payload["temperature"], json!(0), "{id}");
        } else {
            assert!(payload.get("temperature").is_none(), "{id}");
        }
    }
    let custom = vendor_model("vendor--claude-opus-4-7", Some(json!({"supportsTemperature": false})));
    let payload = simple_payload(&custom, &user_ctx("Hello"), temperature(0.0)).await;
    assert!(payload.get("temperature").is_none());
}

// ---------------------------------------------------------------------------
// Port of anthropic-mid-conversation-effort.test.ts
// ---------------------------------------------------------------------------

fn managed_model(provider: &str) -> Model {
    model(json!({
        "id": "claude-fable-5-1", "name": "Claude Fable 5.1", "api": "anthropic-messages", "provider": provider,
        "baseUrl": "http://127.0.0.1:9", "reasoning": true,
        "thinkingLevelMap": {"off": null, "minimal": "low", "low": "low", "medium": "medium", "high": "high", "max": "max"},
        "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200000, "maxTokens": 32000,
        "compat": {"forceAdaptiveThinking": true, "supportsMidConvoEffort": true}
    }))
}

fn effort_assistant(model: &Model, level: Option<&str>) -> Value {
    let mut value = json!({"role": "assistant", "content": [
        {"type": "thinking", "thinking": "reasoning", "thinkingSignature": "signature"},
        {"type": "text", "text": "answer"}
    ], "provider": model.provider, "model": model.id});
    if let Some(level) = level {
        value["providerThinkingLevel"] = json!(level);
    }
    value
}

fn u(text: &str, timestamp: i64) -> Value {
    json!({"role": "user", "content": text, "timestamp": timestamp})
}

async fn capture_effort(model: &Model, messages: Vec<Value>, extra: Value) -> (Value, AssistantMessage) {
    let mut options = StreamOptions { cache_retention: Some(CacheRetention::None), ..Default::default() }.with_extra("thinkingEnabled", true);
    for (k, v) in extra.as_object().unwrap() {
        options = options.with_extra(k.clone(), v.clone());
    }
    anthropic_payload(model, &normalize_context(&ctx(json!({"messages": messages}))), options).await
}

fn effort_messages(payload: &Value) -> Vec<Value> {
    payload["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "system").cloned().collect()
}

fn marker(effort: &str) -> Value {
    json!({"role": "system", "content": [], "output_config": {"effort": effort}})
}

#[tokio::test]
async fn mid_conversation_effort_reconstructs_historical_markers() {
    let model = managed_model("anthropic");
    let (first, first_message) = capture_effort(&model, vec![u("one", 1)], json!({"effort": "low"})).await;
    let (second, _) = capture_effort(
        &model,
        vec![u("one", 1), effort_assistant(&model, Some("low")), u("two", 2)],
        json!({"effort": "high"}),
    )
    .await;
    assert_eq!(first["messages"], json!([{"role": "user", "content": "one"}, marker("low")]));
    let first_len = first["messages"].as_array().unwrap().len();
    assert_eq!(second["messages"].as_array().unwrap()[..first_len], first["messages"].as_array().unwrap()[..]);
    assert_eq!(second["messages"].as_array().unwrap().last().unwrap(), &marker("high"));
    assert_eq!(first["output_config"], json!({"effort": "low"}));
    assert_eq!(second["output_config"], json!({"effort": "low"}));
    assert_eq!(
        second["thinking"],
        json!({"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
    );
    assert_eq!(first_message.provider_thinking_level.as_deref(), Some("low"));
}

#[tokio::test]
async fn mid_conversation_effort_preserves_native_efforts() {
    let model = managed_model("anthropic");
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let (payload, message) = capture_effort(&model, vec![u("one", 1)], json!({"effort": effort})).await;
        assert_eq!(payload["output_config"], json!({"effort": "low"}), "{effort}");
        assert_eq!(effort_messages(&payload), vec![marker(effort)]);
        assert_eq!(message.provider_thinking_level.as_deref(), Some(effort));
    }
}

#[tokio::test]
async fn mid_conversation_effort_defaults_to_high() {
    let (payload, message) = capture_effort(&managed_model("anthropic"), vec![u("one", 1)], json!({})).await;
    assert_eq!(payload["messages"].as_array().unwrap().last().unwrap(), &marker("high"));
    assert_eq!(payload["output_config"], json!({"effort": "low"}));
    assert_eq!(payload["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block");
    assert_eq!(message.provider_thinking_level.as_deref(), Some("high"));
}

#[tokio::test]
async fn mid_conversation_effort_ignores_legacy_and_foreign_assistants() {
    let model = managed_model("anthropic");
    let mut other = effort_assistant(&model, Some("low"));
    other["provider"] = json!("other-provider");
    let (payload, _) = capture_effort(
        &model,
        vec![u("one", 1), effort_assistant(&model, None), u("two", 2), other, u("three", 3)],
        json!({"effort": "medium"}),
    )
    .await;
    assert_eq!(effort_messages(&payload), vec![marker("medium")]);
}

#[tokio::test]
async fn mid_conversation_effort_disabled_thinking() {
    let mut model = managed_model("anthropic");
    model.thinking_level_map = Some(
        serde_json::from_value(json!({"minimal": null, "low": "low", "medium": "medium", "high": "high", "max": "max"})).unwrap(),
    );
    let (payload, message) = capture_effort(
        &model,
        vec![
            u("one", 1),
            effort_assistant(&model, Some("low")),
            u("two", 2),
            effort_assistant(&model, Some("medium")),
            u("three", 3),
        ],
        json!({"thinkingEnabled": false}),
    )
    .await;
    assert_eq!(payload["thinking"], json!({"type": "disabled"}));
    assert!(payload.get("output_config").is_none());
    assert!(effort_messages(&payload).is_empty());
    assert_eq!(message.provider_thinking_level, None);

    // Off unsupported: adaptive thinking stays on.
    let (payload, message) = capture_effort(&managed_model("anthropic"), vec![u("one", 1)], json!({"thinkingEnabled": false})).await;
    assert_eq!(payload["thinking"]["type"], "adaptive");
    assert_eq!(effort_messages(&payload), vec![marker("high")]);
    assert_eq!(message.provider_thinking_level.as_deref(), Some("high"));
}

#[tokio::test]
async fn mid_conversation_effort_unsupported_models_use_top_level_effort() {
    let mut model = managed_model("anthropic");
    model.compat = Some(serde_json::from_value(json!({"forceAdaptiveThinking": true})).unwrap());
    let (payload, message) = capture_effort(&model, vec![u("one", 1)], json!({"effort": "low"})).await;
    assert_eq!(payload["messages"], json!([{"role": "user", "content": "one"}]));
    assert_eq!(payload["output_config"], json!({"effort": "low"}));
    assert_eq!(payload["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(message.provider_thinking_level, None);
}

#[tokio::test]
async fn mid_conversation_effort_sends_effort_and_binding_betas() {
    let body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_test", "model": "claude-fable-5-1", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ]);
    let server = sse_server(body).await;
    let mut model = managed_model("anthropic");
    model.base_url = server.url.clone();
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..key("test-key") };
    let result = anthropic_messages::stream(&model, &normalize_context(&user_ctx("one")), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let beta = server.request(0).header("anthropic-beta").unwrap().to_string();
    assert!(beta.contains("mid-conversation-output-config-2026-07-01"), "{beta}");
    assert!(beta.contains("thinking-binding-controls-2026-08-01"), "{beta}");
}

#[test]
fn mid_conversation_effort_catalog_gates() {
    let direct = builtin("anthropic", "claude-fable-5-1");
    let open_router = builtin("openrouter", "anthropic/claude-fable-5.1");
    let unsupported = builtin("anthropic", "claude-opus-4-8");
    assert_eq!(direct.compat().supports_mid_convo_effort, Some(true));
    let direct_map = serde_json::to_value(&direct.thinking_level_map).unwrap();
    assert!(direct_map.get("off").is_some_and(Value::is_null), "{direct_map}");
    assert_eq!(open_router.api, "anthropic-messages");
    assert_eq!(open_router.base_url, "https://openrouter.ai/api");
    assert_eq!(open_router.compat().supports_mid_convo_effort, Some(true));
    assert_eq!(unsupported.compat().supports_mid_convo_effort, None);
    assert_eq!(builtin("openrouter", "anthropic/claude-opus-5").compat().supports_mid_convo_effort, None);
    assert_eq!(builtin("anthropic", "claude-opus-5").compat().allowed_fallback_models, None);

    for id in ["claude-opus-5", "claude-haiku-5-5"] {
        let model = builtin("anthropic", id);
        assert_eq!(model.compat().supports_mid_convo_effort, Some(true), "{id}");
        let map = serde_json::to_value(&model.thinking_level_map).unwrap();
        assert!(!map.get("off").is_some_and(Value::is_null), "{id}: {map}");
    }
    for id in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1"] {
        let model = builtin("anthropic", id);
        let map = serde_json::to_value(&model.thinking_level_map).unwrap();
        assert!(map.get("off").is_some_and(Value::is_null), "{id}: {map}");
    }
}

// ---------------------------------------------------------------------------
// Port of anthropic-strict-tool-schema.test.ts
// ---------------------------------------------------------------------------

fn strict_schema_model() -> Model {
    model(json!({
        "id": "claude-opus-4-8", "name": "Claude Opus 4.8", "api": "anthropic-messages", "provider": "test-anthropic",
        "baseUrl": "http://127.0.0.1:9", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 32000,
        "compat": {"forceAdaptiveThinking": true, "supportsStrictTools": true}
    }))
}

async fn capture_first_tool(tool_value: Value) -> Value {
    let context = normalize_context(&ctx(json!({"messages": [{"role": "user", "content": "Use the tool"}], "tools": [tool_value]})));
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..Default::default() };
    let (payload, _) = anthropic_payload(&strict_schema_model(), &context, options).await;
    payload["tools"][0].clone()
}

fn prefer_tool(parameters: Value) -> Value {
    json!({"name": "lookup", "description": "Look up a value", "parameters": parameters, "constrainedSampling": {"type": "json_schema", "strict": "prefer"}})
}

#[tokio::test]
async fn only_strict_json_schema_tools_send_the_full_input_schema() {
    let legacy = json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"], "additionalProperties": false, "title": "LookupInput"});
    let tool = capture_first_tool(json!({"name": "lookup", "description": "Look up a value", "parameters": legacy})).await;
    assert!(tool.get("strict").is_none());
    assert_eq!(tool["input_schema"], json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}));

    let strict = capture_first_tool(prefer_tool(json!({
        "type": "object", "title": "StrictLookupInput",
        "properties": {"value": {"type": "string"}, "optional": {"type": "number"}}, "required": ["value"]
    })))
    .await;
    assert_eq!(strict["strict"], true);
    let schema = &strict["input_schema"];
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["value", "optional"]));
    assert_eq!(schema["properties"]["optional"], json!({"anyOf": [{"type": "number"}, {"type": "null"}]}));
    assert_eq!(schema["title"], "StrictLookupInput");
}

#[tokio::test]
async fn prefer_tools_with_unsupported_keywords_go_non_strict() {
    // https://github.com/earendil-works/pi/issues/9953
    for parameters in [
        json!({"type": "object", "properties": {"timeoutMs": {"type": "integer", "minimum": 1, "maximum": 300000}}}),
        json!({"type": "object", "properties": {"options": {"type": "object", "properties": {"tags": {"type": "array", "items": {"type": "string"}, "minItems": 2}}, "required": ["tags"]}}, "required": ["options"]}),
        json!({"type": "object", "properties": {"expression": {"type": "string", "format": "regex"}}, "required": ["expression"]}),
    ] {
        let tool = capture_first_tool(prefer_tool(parameters.clone())).await;
        assert!(tool.get("strict").is_none(), "{parameters}");
    }
    let supported = capture_first_tool(prefer_tool(json!({"type": "object", "properties": {
        "code": {"type": "string", "minLength": 1, "maxLength": 1000, "pattern": "^[a-z]+$"},
        "url": {"type": "string", "format": "uri"},
        "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1}
    }, "required": ["code", "url", "tags"]})))
    .await;
    assert_eq!(supported["strict"], true);
}

// ---------------------------------------------------------------------------
// Port of anthropic-strict-tool-fallback.test.ts
// ---------------------------------------------------------------------------

const GRAMMAR_ERROR: &str =
    "The compiled grammar is too large, which would cause performance issues. Simplify your tool schemas or reduce the number of strict tools.";

fn fallback_model(base_url: &str) -> Model {
    model(json!({
        "id": "claude-haiku-4-5", "name": "Test Claude", "api": "anthropic-messages", "provider": "anthropic",
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 100,
        "compat": {"supportsStrictTools": true}
    }))
}

fn strict_tool(name: &str, strict: Option<&str>) -> Value {
    let mut value = json!({"name": name, "description": "Look up a value", "parameters": {
        "type": "object", "properties": {"value": {"type": "string"}, "optional": {"type": "number"}}, "required": ["value"]
    }});
    if let Some(strict) = strict {
        value["constrainedSampling"] = json!({"type": "json_schema", "strict": strict});
    }
    value
}

fn tools_context(tools: Vec<Value>) -> TranscriptContext {
    normalize_context(&ctx(json!({"tools": tools, "messages": [{"role": "user", "content": "Hello"}]})))
}

fn error_response(message: &str, status: u16, error_type: &str) -> Response {
    Response::json(status, json!({"type": "error", "error": {"type": error_type, "message": message}}))
}

fn success_response() -> Response {
    Response::sse(&anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_test", "model": "claude-haiku-4-5", "usage": {"input_tokens": 10, "output_tokens": 0}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ]))
}

fn no_retries(key_value: &str) -> StreamOptions {
    StreamOptions { max_retries: Some(0), ..key(key_value) }
}

fn payloads(server: &MockServer) -> Vec<Value> {
    server.requests().iter().map(|r| r.json()).collect()
}

#[tokio::test]
async fn preferred_strict_tools_fall_back_to_original_schemas() {
    // Regression tests for https://github.com/earendil-works/pi/issues/10409.
    for message in [
        GRAMMAR_ERROR,
        "Too many strict tools (32). The maximum number of strict tools supported is 20. Try reducing the number of tools marked as strict.",
        "Schemas contains too many parameters with union types (480 parameters with type arrays or anyOf). This causes exponential compilation cost. Reduce the number of nullable or union-typed parameters (limit: 16 parameters with unions).",
    ] {
        let error_message = message.to_string();
        let server = MockServer::start(move |_, index| {
            if index == 0 { error_response(&error_message, 400, "invalid_request_error") } else { success_response() }
        })
        .await;
        let transcript = tools_context(vec![
            strict_tool("preferred", Some("prefer")),
            strict_tool("required", Some("require")),
            strict_tool("ordinary", None),
        ]);
        let original = serde_json::to_string(&transcript).unwrap();
        let payload_calls = Arc::new(Mutex::new(0usize));
        let response_calls = Arc::new(Mutex::new(0usize));
        let (pc, rc) = (payload_calls.clone(), response_calls.clone());
        let options = StreamOptions {
            on_payload: Some(Arc::new(move |payload: Value, _m: Model| {
                *pc.lock() += 1;
                let mut payload = payload;
                payload["inference_geo"] = json!("us");
                payload["stream"] = json!(false);
                Box::pin(async move { Some(payload) })
            })),
            on_response: Some(Arc::new(move |_r: ProviderResponse, _m: Model| {
                *rc.lock() += 1;
                Box::pin(async {})
            })),
            cache_retention: Some(CacheRetention::Long),
            ..no_retries("test-key")
        };
        let (events, result) = anthropic_messages::stream(&fallback_model(&server.url), &transcript, options).collect_all().await;
        assert_eq!(result.stop_reason, StopReason::Stop, "{message}: {:?}", result.error_message);
        assert_eq!(events.iter().map(|e| e.type_name()).collect::<Vec<_>>(), vec!["start", "done"]);
        let sent = payloads(&server);
        assert_eq!(sent.len(), 2);
        assert_eq!(*payload_calls.lock(), 2);
        assert_eq!(*response_calls.lock(), 1);
        let (first, second) = (&sent[0]["tools"], &sent[1]["tools"]);
        assert_eq!(first[0]["strict"], true);
        assert_eq!(first[0]["input_schema"]["required"], json!(["value", "optional"]));
        assert_eq!(first[0]["input_schema"]["properties"]["optional"], json!({"anyOf": [{"type": "number"}, {"type": "null"}]}));
        assert!(second[0].get("strict").is_none());
        assert_eq!(
            second[0]["input_schema"],
            json!({"type": "object", "properties": {"value": {"type": "string"}, "optional": {"type": "number"}}, "required": ["value"]})
        );
        assert_eq!(second[1], first[1]);
        assert_eq!(second[1]["strict"], true);
        assert_eq!(second[2], first[2]);
        assert_eq!(second[2]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(sent.iter().map(|p| p["stream"].clone()).collect::<Vec<_>>(), vec![json!(true), json!(true)]);
        assert_eq!(sent[1]["inference_geo"], "us");
        assert_eq!(serde_json::to_string(&transcript).unwrap(), original);
        let diagnostic = serde_json::to_value(&result.diagnostics.as_ref().unwrap()[0]).unwrap();
        assert_eq!(diagnostic["type"], "anthropic_strict_tools_fallback");
        assert_eq!(diagnostic["details"]["reason"], "strict_tool_limits_exceeded");
    }
}

#[tokio::test]
async fn strict_fallback_downgrades_inline_additions_but_not_require() {
    let server = MockServer::start(|_, index| {
        if index == 0 { error_response(GRAMMAR_ERROR, 400, "invalid_request_error") } else { success_response() }
    })
    .await;
    let context = ctx(json!({"tools": [strict_tool("lookup", Some("require"))], "messages": [
        {"role": "user", "content": "Hello"},
        {"role": "system", "content": "", "toolsAdded": [strict_tool("lookup", Some("prefer"))], "timestamp": 2},
        {"role": "system", "content": "", "toolsAdded": [strict_tool("lookup", Some("require"))], "timestamp": 3}
    ]}));
    let mut model = fallback_model(&server.url);
    model.compat = Some(
        serde_json::from_value(json!({"supportsStrictTools": true, "supportsMidConvoSystemMessages": true, "supportsMidConvoToolChanges": true})).unwrap(),
    );
    let result = anthropic_messages::stream(&model, &normalize_context(&context), no_retries("sk-ant-oat-test")).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let sent = payloads(&server);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["tools"], sent[0]["tools"]);
    let additions: Vec<Vec<Value>> = sent
        .iter()
        .map(|payload| {
            payload["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .filter(|b| b["type"] == "tool_addition" && b["tool"]["type"] == "tool_definition")
                .map(|b| b["tool"]["definition"].clone())
                .collect()
        })
        .collect();
    assert_eq!(additions[0].iter().map(|d| d.get("strict").cloned()).collect::<Vec<_>>(), vec![Some(json!(true)), Some(json!(true))]);
    assert_eq!(additions[1].iter().map(|d| d.get("strict").cloned()).collect::<Vec<_>>(), vec![None, Some(json!(true))]);
    assert_eq!(additions[1][0]["input_schema"]["required"], json!(["value"]));
    assert_eq!(additions[1][1], additions[0][1]);
}

#[tokio::test]
async fn strict_fallback_does_not_retry_required_ordinary_or_unsupported_prefer_sets() {
    for tools in [
        vec![strict_tool("lookup", Some("require"))],
        vec![strict_tool("lookup", None)],
        vec![json!({"name": "lookup", "description": "d", "constrainedSampling": {"type": "json_schema", "strict": "prefer"},
            "parameters": {"type": "object", "properties": {"value": {"type": "number", "minimum": 1}}, "required": ["value"]}})],
    ] {
        let server = MockServer::start(|_, _| error_response(GRAMMAR_ERROR, 400, "invalid_request_error")).await;
        let result = anthropic_messages::stream(&fallback_model(&server.url), &tools_context(tools), no_retries("test-key")).result().await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error_message.as_deref().unwrap().contains(GRAMMAR_ERROR));
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn strict_fallback_requires_the_exact_rejection() {
    for (message, status, error_type) in [
        ("Invalid tool schema", 400, "invalid_request_error"),
        (GRAMMAR_ERROR, 400, "authentication_error"),
        (GRAMMAR_ERROR, 500, "invalid_request_error"),
    ] {
        let server = MockServer::start(move |_, _| error_response(message, status, error_type)).await;
        let result = anthropic_messages::stream(&fallback_model(&server.url), &tools_context(vec![strict_tool("lookup", Some("prefer"))]), no_retries("test-key"))
            .result()
            .await;
        assert_eq!(result.stop_reason, StopReason::Error, "{message} {status} {error_type}");
        assert_eq!(server.requests().len(), 1, "{message} {status} {error_type}");
    }
}

#[tokio::test]
async fn strict_fallback_surfaces_a_second_rejection() {
    let server = MockServer::start(|_, index| {
        error_response(if index == 0 { GRAMMAR_ERROR } else { "Required grammar is still too large" }, 400, "invalid_request_error")
    })
    .await;
    let transcript = tools_context(vec![strict_tool("lookup", Some("prefer")), strict_tool("required", Some("require"))]);
    let result = anthropic_messages::stream(&fallback_model(&server.url), &transcript, no_retries("test-key")).result().await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error_message.as_deref().unwrap().contains("Required grammar is still too large"));
    let sent = payloads(&server);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["tools"][1]["strict"], true);
}

#[tokio::test]
async fn strict_fallback_honors_cancellation_before_the_retry() {
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let server = MockServer::start(move |_, _| {
        cancel.cancel();
        error_response(GRAMMAR_ERROR, 400, "invalid_request_error")
    })
    .await;
    let options = StreamOptions { signal: Some(signal), ..no_retries("test-key") };
    let result = anthropic_messages::stream(&fallback_model(&server.url), &tools_context(vec![strict_tool("lookup", Some("prefer"))]), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn strict_fallback_does_not_retry_sse_errors() {
    let body = format!(
        "event: error\ndata: {}\n\n",
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": GRAMMAR_ERROR}})
    );
    let server = sse_server(body).await;
    let result = anthropic_messages::stream(&fallback_model(&server.url), &tools_context(vec![strict_tool("lookup", Some("prefer"))]), no_retries("test-key"))
        .result()
        .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn strict_fallback_is_per_request() {
    let server = MockServer::start(|_, index| {
        if index == 0 { error_response(GRAMMAR_ERROR, 400, "invalid_request_error") } else { success_response() }
    })
    .await;
    let transcript = tools_context(vec![strict_tool("lookup", Some("prefer"))]);
    anthropic_messages::stream(&fallback_model(&server.url), &transcript, no_retries("test-key")).result().await;
    anthropic_messages::stream(&fallback_model(&server.url), &transcript, no_retries("test-key")).result().await;
    let stricts: Vec<Option<Value>> = payloads(&server).iter().map(|p| p["tools"][0].get("strict").cloned()).collect();
    assert_eq!(stricts, vec![Some(json!(true)), None, Some(json!(true))]);
}

#[tokio::test]
async fn strict_prefer_survives_pi_messages_for_the_gateway_fallback() {
    // pi-messages forwards the transcript (with `prefer`) unchanged; the gateway's Anthropic adapter
    // then falls back on its own.
    let gateway = MockServer::start(|_, _| {
        Response::sse(&format!(
            "data: {}\n\n",
            json!({"type": "done", "reason": "stop", "usage": empty_usage()})
        ))
    })
    .await;
    let transcript = tools_context(vec![strict_tool("lookup", Some("prefer"))]);
    let mut gateway_model = fallback_model(&format!("{}/v1", gateway.url));
    gateway_model.api = "pi-messages".into();
    gateway_model.provider = "radius".into();
    gateway_model.compat = None;
    let result = pi_ai::api::pi_messages::stream(&gateway_model, &transcript, key("radius-test")).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let forwarded = gateway.request(0).json()["context"].clone();
    assert_eq!(forwarded, serde_json::to_value(&transcript).unwrap());

    let upstream = MockServer::start(|_, index| {
        if index == 0 { error_response(GRAMMAR_ERROR, 400, "invalid_request_error") } else { success_response() }
    })
    .await;
    let messages: Vec<Message> = serde_json::from_value(forwarded["messages"].clone()).unwrap();
    let replay = normalize_context(&Context { system_prompt: None, messages, tools: None });
    let upstream_result = anthropic_messages::stream(&fallback_model(&upstream.url), &replay, no_retries("test-key")).result().await;
    assert_eq!(upstream_result.stop_reason, StopReason::Stop, "{:?}", upstream_result.error_message);
    let stricts: Vec<Option<Value>> = payloads(&upstream).iter().map(|p| p["tools"][0].get("strict").cloned()).collect();
    assert_eq!(stricts, vec![Some(json!(true)), None]);
}

// ---------------------------------------------------------------------------
// Port of anthropic-sse-parsing.test.ts (cases not in tests/anthropic_messages.rs)
// ---------------------------------------------------------------------------

fn response_model_events(model_id: &str, block: Value) -> String {
    anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_response_model", "model": model_id, "usage": {"input_tokens": 100, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": block}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 100, "output_tokens": 20}}),
        json!({"type": "message_stop"}),
    ])
}

fn minimal_events() -> Vec<Value> {
    vec![
        json!({"type": "message_start", "message": {"id": "msg_test", "usage": {"input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}),
        json!({"type": "message_stop"}),
    ]
}

#[tokio::test]
async fn provider_stream_events_carry_the_request_model() {
    let server = sse_server(anthropic_sse(&minimal_events())).await;
    let mut model = builtin("anthropic", "claude-haiku-4-5");
    model.base_url = server.url.clone();
    let seen = Arc::new(Mutex::new(Vec::<(String, Model)>::new()));
    let sink = seen.clone();
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event: Value, event_model: Model| {
            let sink = sink.clone();
            Box::pin(async move {
                tokio::task::yield_now().await;
                sink.lock().push((event["type"].as_str().unwrap_or_default().to_string(), event_model));
            })
        })),
        ..key("k")
    };
    let result = anthropic_messages::stream(&model, &hello(), options).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    let seen = seen.lock().clone();
    assert_eq!(
        seen.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
        vec!["message_start", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]
    );
    assert!(seen.iter().all(|(_, m)| *m == model));
}

#[tokio::test]
async fn signed_thinking_stays_replayable_when_a_proxy_relabels_the_model() {
    // Regression test for earendil-works/pi#9188.
    let model = builtin("anthropic", "claude-opus-5");
    let first = stream_against(
        &model,
        response_model_events("kimi-for-coding", json!({"type": "thinking", "thinking": "reasoning", "signature": "signature"})),
    )
    .await;
    assert_eq!(first.model, model.id);
    assert_eq!(first.response_model.as_deref(), Some("kimi-for-coding"));
    let mut messages = normalize_context(&user_ctx("hi")).messages;
    messages.push(Message::Assistant(first));
    let transformed = pi_ai::api::transform_messages::transform_messages(&messages, &model, None);
    let replayed = transformed.iter().find_map(Message::as_assistant).unwrap();
    assert_eq!(
        serde_json::to_value(&replayed.content).unwrap(),
        json!([{"type": "thinking", "thinking": "reasoning", "thinkingSignature": "signature"}])
    );
}

#[tokio::test]
async fn returned_fallback_models_drive_cost_attribution() {
    let mut model = builtin("anthropic", "claude-opus-5");
    model.compat = Some(
        serde_json::from_value(json!({"allowedFallbackModels": [
            {"provider": "anthropic", "model": "fallback-model", "cost": {"input": 3, "output": 5, "cacheRead": 0, "cacheWrite": 0}}
        ]}))
        .unwrap(),
    );
    let result = stream_against(&model, response_model_events("fallback-model", json!({"type": "text", "text": "done"}))).await;
    assert_eq!(result.model, model.id);
    assert_eq!(result.response_model.as_deref(), Some("fallback-model"));
    assert!((result.usage.cost.input - 0.0003).abs() < 1e-10, "{}", result.usage.cost.input);
    assert!((result.usage.cost.output - 0.0001).abs() < 1e-10, "{}", result.usage.cost.output);
}

#[tokio::test]
async fn fails_safely_on_mid_output_fallback() {
    let body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_fallback", "model": "claude-opus-5", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "partial"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "fallback", "from": {"model": "claude-opus-5"}, "to": {"model": "claude-opus-4-8"}}}),
    ]);
    let result = stream_against(&builtin("anthropic", "claude-opus-5"), body).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error_message.as_deref().unwrap().contains("unsupported mid-output model fallback"), "{:?}", result.error_message);
}

#[tokio::test]
async fn forces_streaming_after_an_on_payload_replacement() {
    let server = sse_server(anthropic_sse(&minimal_events())).await;
    let mut model = builtin("anthropic", "claude-fable-5-1");
    model.base_url = server.url.clone();
    let options = StreamOptions {
        on_payload: Some(Arc::new(|payload: Value, _m: Model| {
            let mut payload = payload;
            payload["stream"] = json!(false);
            Box::pin(async move { Some(payload) })
        })),
        ..key("k")
    };
    anthropic_messages::stream(&model, &hello(), options).result().await;
    assert_eq!(server.request(0).json()["stream"], true);
}

#[tokio::test]
async fn beta_features_follow_thinking_and_managed_models() {
    let server = sse_server(anthropic_sse(&minimal_events())).await;
    let mut model = builtin("openrouter", "anthropic/claude-haiku-4.5");
    model.base_url = server.url.clone();
    anthropic_messages::stream(&model, &hello(), key("k").with_extra("thinkingEnabled", false)).result().await;
    assert!(!betas(&server.request(0)).contains(&"interleaved-thinking-2025-05-14".to_string()));

    let mut model = builtin("anthropic", "claude-fable-5-1");
    model.base_url = server.url.clone();
    let result = anthropic_messages::stream(&model, &hello(), key("k")).result().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let betas = betas(&server.request(1));
    assert!(betas.contains(&"mid-conversation-output-config-2026-07-01".to_string()), "{betas:?}");
    assert!(betas.contains(&"thinking-binding-controls-2026-08-01".to_string()), "{betas:?}");
}

#[tokio::test]
async fn reports_the_final_input_transformations() {
    let mut events = minimal_events();
    events[0] = json!({"type": "message_start", "message": {"id": "msg_transformations", "model": "claude-fable-5-1",
        "usage": {"input_tokens": 12, "output_tokens": 0},
        "input_transformations": [{"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"}]}});
    events[4]["input_transformations"] = json!([{"type": "thinking_dropped", "path": "messages.3.content.0", "reason": "model_binding_mismatch"}]);
    let result = stream_against(&builtin("anthropic", "claude-fable-5-1"), anthropic_sse(&events)).await;
    let diagnostics = serde_json::to_value(result.diagnostics.as_ref().unwrap()).unwrap();
    assert_eq!(diagnostics.as_array().unwrap().len(), 1);
    assert_eq!(diagnostics[0]["type"], "anthropic_input_transformations");
    assert!(diagnostics[0]["timestamp"].is_number());
    assert_eq!(
        diagnostics[0]["details"],
        json!({"transformations": [{"type": "thinking_dropped", "path": "messages.3.content.0", "reason": "model_binding_mismatch"}]})
    );
}

#[tokio::test]
async fn repairs_malformed_sse_json_and_tool_json() {
    let malformed = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#;
    let mut body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_test", "usage": {"input_tokens": 12, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_test", "name": "edit", "input": {}}}),
    ]);
    body.push_str(&format!("event: content_block_delta\ndata: {malformed}\n\n"));
    body.push_str(&anthropic_sse(&[
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"input_tokens": 12, "output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ]));
    let server = sse_server(body).await;
    let mut model = builtin("anthropic", "claude-haiku-4-5");
    model.base_url = server.url.clone();
    let context = normalize_context(&ctx(json!({"messages": [{"role": "user", "content": "Use the edit tool."}], "tools": [
        {"name": "edit", "description": "Edit a file.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "text": {"type": "string"}}, "required": ["path", "text"]}}
    ]})));
    let result = anthropic_messages::stream(&model, &context, key("k")).result().await;
    assert_eq!(result.stop_reason, StopReason::ToolUse, "{:?}", result.error_message);
    assert_eq!(result.error_message, None);
    let call = result.tool_calls().next().unwrap();
    assert_eq!(Value::Object(call.arguments.clone()), json!({"path": "A\\H", "text": "col1\tcol2"}));
}

#[tokio::test]
async fn preserves_content_from_content_block_start() {
    let body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_initial_content", "usage": {"input_tokens": 12, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "Initial text"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": " plus delta"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "thinking", "thinking": "Initial thinking", "signature": "initial signature"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "thinking_delta", "thinking": " plus delta"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "signature_delta", "signature": " plus delta"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 12, "output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ]);
    let result = stream_against(&builtin("anthropic", "claude-haiku-4-5"), body).await;
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        json!([
            {"type": "text", "text": "Initial text plus delta"},
            {"type": "thinking", "thinking": "Initial thinking plus delta", "thinkingSignature": "initial signature plus delta"}
        ])
    );
}

#[tokio::test]
async fn refusal_and_sensitive_stop_reasons_become_errors() {
    let explanation = "This request triggered restrictions on violative cyber content and was blocked under Anthropic's Usage Policy. To learn more, provide feedback, or request an exemption based on how you use Claude, visit our help center: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude.";
    let body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_01XFUDYJgAACzvnptvVoYEL", "usage": {"input_tokens": 412, "output_tokens": 0}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber", "explanation": explanation}}, "usage": {"input_tokens": 412, "output_tokens": 0}}),
        json!({"type": "message_stop"}),
    ]);
    let result = stream_against(&builtin("anthropic", "claude-fable-5"), body).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("refusal"));
    assert_eq!(result.error_message.as_deref(), Some(explanation));

    let body = anthropic_sse(&[
        json!({"type": "message_start", "message": {"id": "msg_sensitive", "usage": {"input_tokens": 12, "output_tokens": 0}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "sensitive"}, "usage": {"input_tokens": 12, "output_tokens": 0}}),
        json!({"type": "message_stop"}),
    ]);
    let result = stream_against(&builtin("anthropic", "claude-haiku-4-5"), body).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("sensitive"));
    assert_eq!(result.error_message.as_deref(), Some("Provider stopped with: sensitive"));
}

#[tokio::test]
async fn message_delta_without_usage_and_trailing_unknown_events() {
    let events: Vec<Value> = minimal_events()
        .into_iter()
        .map(|e| if e["type"] == "message_delta" { json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}) } else { e })
        .collect();
    let result = stream_against(&builtin("anthropic", "claude-haiku-4-5"), anthropic_sse(&events)).await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(result.text(), "Hello");
    assert_eq!(result.usage.input, 12);
    assert_eq!(result.usage.total_tokens, 12);

    let mut body = anthropic_sse(&minimal_events());
    body.push_str("event: done\ndata: [DONE]\n\nevent: proxy.stats\ndata: not json\n\n");
    let result = stream_against(&builtin("anthropic", "claude-haiku-4-5"), body).await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(result.text(), "Hello");
}
