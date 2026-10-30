//! Offline tests for the OpenAI Codex Responses API (port of `openai-codex-stream.test.ts`).

mod responses_mock;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use parking_lot::Mutex;
use pi_ai::api::openai_codex_responses::{self as codex, get_openai_codex_websocket_debug_stats};
use pi_ai::*;
use responses_mock::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn mock_token(account: &str) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(
        json!({ "https://api.openai.com/auth": { "chatgpt_account_id": account } }).to_string(),
    );
    format!("aaa.{payload}.bbb")
}

fn model(base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": "gpt-5.1-codex", "name": "GPT-5.1 Codex", "api": "openai-codex-responses", "provider": "openai-codex",
        "baseUrl": base_url, "reasoning": true, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 400000, "maxTokens": 128000
    }))
    .unwrap()
}

fn context(text: &str) -> TranscriptContext {
    normalize_context(&Context {
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: vec![Message::User(UserMessage::new(text, 1))],
        tools: None,
    })
}

fn sse_events(status: &str, end_turn: Option<bool>) -> Vec<Value> {
    let terminal = if status == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    let mut response = json!({
        "status": status,
        "incomplete_details": if status == "incomplete" { json!({ "reason": "max_output_tokens" }) } else { Value::Null },
        "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8, "input_tokens_details": { "cached_tokens": 0 } }
    });
    if let Some(end_turn) = end_turn {
        response["end_turn"] = json!(end_turn);
    }
    vec![
        json!({ "type": "response.output_item.added", "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] } }),
        json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } }),
        json!({ "type": "response.output_text.delta", "delta": "Hello" }),
        json!({ "type": "response.output_item.done", "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{ "type": "output_text", "text": "Hello" }] } }),
        json!({ "type": terminal, "response": response }),
    ]
}

fn options(token: &str, transport: Transport) -> StreamOptions {
    StreamOptions {
        api_key: Some(token.into()),
        transport: Some(transport),
        ..Default::default()
    }
}

fn text_of(message: &AssistantMessage) -> String {
    message.text()
}

fn no_ws() -> WsHandler {
    Arc::new(|_, _| vec![])
}

fn ws_events(events: Vec<Value>) -> Vec<WsAction> {
    events
        .into_iter()
        .map(|e| WsAction::Send(e.to_string()))
        .collect()
}

#[tokio::test]
async fn streams_sse_and_forwards_provider_events() {
    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    let token = mock_token("acc_test");
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = events.clone();
    let mut opts = options(&token, Transport::Sse);
    opts.on_provider_stream_event = Some(Arc::new(move |event: Value, _m: Model| {
        sink.lock()
            .push(event["type"].as_str().unwrap_or_default().to_string());
        Box::pin(async {})
    }));
    let (all, message) = codex::stream(&model(&server.base_url), &context("Say hello"), opts)
        .collect_all()
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(text_of(&message), "Hello");
    assert!(all.iter().any(|e| e.type_name() == "text_delta"));
    assert_eq!(
        *events.lock(),
        vec![
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed"
        ]
    );
    let request = &server.requests()[0];
    assert_eq!(request.path, "/codex/responses");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(request.header("chatgpt-account-id"), Some("acc_test"));
    assert_eq!(
        request.header("openai-beta"),
        Some("responses=experimental")
    );
    assert_eq!(request.header("originator"), Some("pi"));
    assert_eq!(
        request.header("user-agent").map(str::to_string),
        Some(format_user_agent(None))
    );
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("content-encoding"), Some("zstd"));
    assert!(request.header("x-api-key").is_none());
    let body = request.decoded_json();
    assert_eq!(body["model"], "gpt-5.1-codex");
    assert_eq!(body["instructions"], "You are a helpful assistant.");
    assert_eq!(body["store"], false);
    assert_eq!(body["text"], json!({ "verbosity": "low" }));
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["reasoning"], json!({ "effort": "none" }));
    assert_eq!(
        body["input"],
        json!([{ "role": "user", "content": [{ "type": "input_text", "text": "Say hello" }] }])
    );
}

#[tokio::test]
async fn sse_terminal_without_trailing_blank_line_and_open_bodies() {
    let mut body = data_sse(&sse_events("completed", None));
    body.truncate(body.len() - 2);
    let server = serve_sequence(vec![Reply::sse(body)]).await;
    let message = codex::stream(
        &model(&server.base_url),
        &context("x"),
        options(&mock_token("a"), Transport::Sse),
    )
    .result()
    .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );

    let mut open = data_sse(&sse_events("completed", Some(false)));
    open.push_str("data: [DONE]\n\n");
    let server = serve_sequence(vec![Reply::sse(open).keep_open()]).await;
    let message = tokio::time::timeout(
        Duration::from_secs(5),
        codex::stream(
            &model(&server.base_url),
            &context("x"),
            options(&mock_token("a"), Transport::Sse),
        )
        .result(),
    )
    .await
    .expect("completes without body end");
    assert_eq!(text_of(&message), "Hello");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.end_turn, Some(false));

    let server = serve_sequence(vec![
        Reply::sse(data_sse(&sse_events("incomplete", None))).keep_open(),
    ])
    .await;
    let message = tokio::time::timeout(
        Duration::from_secs(5),
        codex::stream(
            &model(&server.base_url),
            &context("x"),
            options(&mock_token("a"), Transport::Sse),
        )
        .result(),
    )
    .await
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
}

#[tokio::test]
async fn sse_header_timeout() {
    let server = serve_sequence(vec![
        Reply::sse(data_sse(&sse_events("completed", None))).delay_headers(3000),
    ])
    .await;
    let mut opts = options(&mock_token("a"), Transport::Sse);
    opts.timeout_ms = Some(50);
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Codex SSE response headers timed out after 50ms")
    );
}

#[tokio::test]
async fn aborts_sse_body_reads() {
    let first = data_sse(&sse_events("completed", None)[..3]);
    let later = data_sse(&[json!({ "type": "response.output_text.delta", "delta": "two" })]);
    let server = serve_sequence(vec![Reply::sse(first).chunk(500, later).keep_open()]).await;
    let signal = CancellationToken::new();
    let mut opts = options(&mock_token("a"), Transport::Sse);
    opts.signal = Some(signal.clone());
    let mut stream = codex::stream(&model(&server.base_url), &context("x"), opts);
    let mut deltas = Vec::new();
    while let Some(event) = stream.next_event().await {
        if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
            deltas.push(delta.clone());
            if delta == "Hello" {
                signal.cancel();
            }
        }
    }
    let message = stream.result().await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Request was aborted")
    );
    assert_eq!(deltas, vec!["Hello"]);
}

#[tokio::test]
async fn session_headers_cache_key_and_overrides() {
    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    let token = mock_token("acc_test");
    let mut opts = options(&token, Transport::Sse);
    opts.session_id = Some("session-123".into());
    codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    let request = &server.requests()[0];
    assert_eq!(request.header("session-id"), Some("session-123"));
    assert_eq!(request.header("x-client-request-id"), Some("session-123"));
    assert_eq!(request.decoded_json()["prompt_cache_key"], "session-123");

    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    let mut opts = options(&token, Transport::Sse);
    opts.session_id = Some("session-123".into());
    opts.cache_retention = Some(CacheRetention::None);
    codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    let request = &server.requests()[0];
    assert!(request.header("session-id").is_none());
    assert!(request.header("x-client-request-id").is_none());
    assert!(request.decoded_json().get("prompt_cache_key").is_none());

    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    let mut opts = options(&token, Transport::Sse);
    opts.session_id = Some("s".repeat(70));
    let mut headers = ProviderHeaders::new();
    headers.insert("originator".into(), Some("caller".into()));
    headers.insert("User-Agent".into(), Some("custom-agent".into()));
    opts.headers = Some(headers);
    codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    let request = &server.requests()[0];
    assert_eq!(request.header("session-id"), Some("s".repeat(64).as_str()));
    assert_eq!(
        request.decoded_json()["prompt_cache_key"],
        json!("s".repeat(64))
    );
    assert_eq!(request.header("originator"), Some("caller"));
    assert_eq!(request.header("user-agent"), Some("custom-agent"));
}

#[tokio::test]
async fn simple_options_reasoning_and_tool_choice() {
    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    // gpt-5.5 left the openai-codex catalog; TS uses an inline model that maps xhigh.
    let gpt55: Model = serde_json::from_value(json!({
        "id": "gpt-5.5", "name": "GPT-5.5", "api": "openai-codex-responses", "provider": "openai-codex",
        "baseUrl": server.base_url, "reasoning": true, "thinkingLevelMap": { "xhigh": "xhigh" }, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 400000, "maxTokens": 128000
    }))
    .unwrap();
    let mut opts = options(&mock_token("a"), Transport::Sse);
    opts.reasoning = Some(ThinkingLevel::Xhigh);
    opts.tool_choice = Some(ToolChoice::None);
    codex::stream_simple(&gpt55, &context("x"), opts)
        .result()
        .await;
    let body = server.requests()[0].decoded_json();
    assert_eq!(
        body["reasoning"],
        json!({ "effort": "xhigh", "summary": "auto" })
    );
    assert_eq!(body["tool_choice"], "none");

    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    let opts = options(&mock_token("a"), Transport::Sse).with_extra("toolChoice", "required");
    codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(
        server.requests()[0].decoded_json()["tool_choice"],
        "required"
    );
}

#[tokio::test]
async fn strict_mode_is_explicit_for_codex_tools() {
    let server = serve_sequence(vec![Reply::sse(data_sse(&sse_events("completed", None)))]).await;
    let mut constrained = Tool::new(
        "constrained",
        "c",
        json!({ "type": "object", "properties": { "value": { "type": "string" } }, "required": ["value"] }),
    );
    constrained.constrained_sampling =
        Some(serde_json::from_value(json!({ "type": "json_schema", "strict": "prefer" })).unwrap());
    let tools = vec![
        Tool::new(
            "ordinary",
            "o",
            json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        ),
        constrained,
    ];
    let context = normalize_context(&Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage::new("x", 1))],
        tools: Some(tools),
    });
    codex::stream(
        &model(&server.base_url),
        &context,
        options(&mock_token("a"), Transport::Sse),
    )
    .result()
    .await;
    let body = server.requests()[0].decoded_json();
    assert_eq!(body["tools"][0]["strict"], Value::Null);
    assert_eq!(body["tools"][1]["strict"], true);
    assert_eq!(body["instructions"], "You are a helpful assistant.");
}

#[tokio::test]
async fn retries_retryable_sse_failures() {
    let limited = Reply::json(
        429,
        json!({ "error": { "code": "rate_limit_exceeded", "message": "rate limited" } })
            .to_string(),
    )
    .header("retry-after-ms", "10");
    let server = serve_sequence(vec![
        limited.clone(),
        limited,
        Reply::sse(data_sse(&sse_events("completed", None))),
    ])
    .await;
    let mut opts = options(&mock_token("a"), Transport::Sse);
    opts.max_retries = Some(3);
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(server.requests().len(), 3);

    let server = serve_sequence(vec![Reply::json(
        429,
        json!({ "error": { "code": "usage_limit_reached", "plan_type": "PLUS" } }).to_string(),
    )])
    .await;
    let message = codex::stream(
        &model(&server.base_url),
        &context("x"),
        options(&mock_token("a"), Transport::Sse),
    )
    .result()
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some("You have hit your ChatGPT usage limit (plus plan).")
    );
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

fn http_fail() -> Handler {
    Arc::new(|_| Reply::text(500, "unexpected fetch"))
}

#[tokio::test]
async fn websocket_auto_transport_from_stream_simple() {
    let ws: WsHandler = Arc::new(|_, _| {
        let mut events = sse_events("completed", Some(false));
        events[4]["type"] = json!("response.done");
        ws_events(events)
    });
    let server = serve_codex(http_fail(), ws, 0).await;
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = events.clone();
    let mut opts = options(&mock_token("a"), Transport::Auto);
    opts.session_id = Some("session-auto-ws".into());
    opts.on_provider_stream_event = Some(Arc::new(move |event: Value, _m: Model| {
        sink.lock()
            .push(event["type"].as_str().unwrap_or_default().to_string());
        Box::pin(async {})
    }));
    let message = codex::stream_simple(&model(&server.base_url), &context("Say hello"), opts)
        .result()
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(message.end_turn, Some(false));
    assert_eq!(text_of(&message), "Hello");
    assert_eq!(
        events.lock().last().map(String::as_str),
        Some("response.done")
    );
    let connections = server.connections();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0].messages.len(), 1);
    assert_eq!(connections[0].messages[0]["type"], "response.create");
    assert_eq!(connections[0].header("session-id"), Some("session-auto-ws"));
    assert_eq!(
        connections[0].header("x-client-request-id"),
        Some("session-auto-ws")
    );
    assert_eq!(
        connections[0].header("openai-beta"),
        Some("responses_websockets=2026-02-06")
    );
    assert!(connections[0].header("session_id").is_none());
    assert!(server.requests().is_empty());
    let stats = get_openai_codex_websocket_debug_stats("session-auto-ws").unwrap();
    assert_eq!(
        (stats.cached_context_requests, stats.full_context_requests),
        (1, 1)
    );
}

fn completed_ws() -> WsHandler {
    let counter = Arc::new(Mutex::new(0));
    Arc::new(move |_, _| {
        let mut n = counter.lock();
        *n += 1;
        ws_events(vec![
            json!({ "type": "response.completed", "response": { "id": format!("resp_{n}"), "status": "completed", "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 } } }),
        ])
    })
}

#[tokio::test]
async fn cached_websockets_are_scoped_to_accounts() {
    let server = serve_codex(http_fail(), completed_ws(), 0).await;
    let empty = normalize_context(&Context::default());
    for account in ["account-a", "account-b", "account-a"] {
        let mut opts = options(&mock_token(account), Transport::WebsocketCached);
        opts.session_id = Some("shared-session-accounts".into());
        let message = codex::stream(&model(&server.base_url), &empty, opts)
            .result()
            .await;
        assert_eq!(
            message.stop_reason,
            StopReason::Stop,
            "{:?}",
            message.error_message
        );
    }
    let connections = server.connections();
    let accounts: Vec<Option<&str>> = connections
        .iter()
        .map(|c| c.header("chatgpt-account-id"))
        .collect();
    assert_eq!(accounts, vec![Some("account-a"), Some("account-b")]);
    assert_eq!(
        connections[1].header("authorization"),
        Some(format!("Bearer {}", mock_token("account-b")).as_str())
    );
    assert!(server.requests().is_empty());
    let stats = get_openai_codex_websocket_debug_stats("shared-session-accounts").unwrap();
    assert_eq!(
        (stats.connections_created, stats.connections_reused),
        (2, 1)
    );
}

#[tokio::test]
async fn one_shot_websockets_without_cache_affinity() {
    let server = serve_codex(http_fail(), completed_ws(), 0).await;
    let empty = normalize_context(&Context::default());
    for _ in 0..2 {
        let mut opts = options(&mock_token("a"), Transport::Websocket);
        opts.session_id = Some("one-shot-session".into());
        opts.cache_retention = Some(CacheRetention::None);
        let message = codex::stream(&model(&server.base_url), &empty, opts)
            .result()
            .await;
        assert_eq!(
            message.stop_reason,
            StopReason::Stop,
            "{:?}",
            message.error_message
        );
    }
    assert_eq!(server.connections().len(), 2);
    assert!(get_openai_codex_websocket_debug_stats("one-shot-session").is_none());
}

#[tokio::test]
async fn falls_back_to_sse_on_connect_timeout() {
    let http: Handler = Arc::new(|_| Reply::sse(data_sse(&sse_events("completed", None))));
    let server = serve_codex(http, no_ws(), 2000).await;
    let mut opts = options(&mock_token("a"), Transport::Auto);
    opts.session_id = Some("ws-connect-timeout".into());
    opts.timeout_ms = Some(300_000);
    opts.websocket_connect_timeout_ms = Some(50);
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(text_of(&message), "Hello", "{:?}", message.error_message);
    assert_eq!(server.requests().len(), 1);
    let stats = get_openai_codex_websocket_debug_stats("ws-connect-timeout").unwrap();
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 1);
    assert_eq!(stats.websocket_fallback_active, Some(true));
    assert_eq!(
        stats.last_websocket_error.as_deref(),
        Some("WebSocket connect timeout after 50ms")
    );
    let diagnostics = message.diagnostics.unwrap();
    assert_eq!(diagnostics[0].diagnostic_type, "provider_transport_failure");
    assert_eq!(
        diagnostics[0].details.as_ref().unwrap()["fallbackTransport"],
        "sse"
    );

    // The session now uses SSE directly.
    let mut opts = options(&mock_token("a"), Transport::Auto);
    opts.session_id = Some("ws-connect-timeout".into());
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(text_of(&message), "Hello");
    assert_eq!(
        get_openai_codex_websocket_debug_stats("ws-connect-timeout")
            .unwrap()
            .sse_fallbacks,
        2
    );
}

#[tokio::test]
async fn reconnects_once_on_connection_limit() {
    let ws: WsHandler = Arc::new(|connection, _| {
        if connection == 0 {
            ws_events(vec![
                json!({ "type": "error", "error": { "code": "websocket_connection_limit_reached" } }),
            ])
        } else {
            ws_events(vec![
                json!({ "type": "response.completed", "response": { "id": "resp_1", "status": "completed", "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8 } } }),
            ])
        }
    });
    let server = serve_codex(http_fail(), ws, 0).await;
    let message = codex::stream(
        &model(&server.base_url),
        &normalize_context(&Context::default()),
        options(&mock_token("a"), Transport::Auto),
    )
    .result()
    .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(server.connections().len(), 2);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn codex_api_errors_do_not_fall_back() {
    let ws: WsHandler = Arc::new(|_, _| {
        ws_events(vec![
            json!({ "type": "error", "code": "invalid_request", "message": "nope" }),
        ])
    });
    let server = serve_codex(http_fail(), ws, 0).await;
    let message = codex::stream(
        &model(&server.base_url),
        &context("x"),
        options(&mock_token("a"), Transport::Auto),
    )
    .result()
    .await;
    assert_eq!(message.error_message.as_deref(), Some("Codex error: nope"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn idle_websocket_before_first_event_falls_back() {
    let http: Handler = Arc::new(|_| Reply::sse(data_sse(&sse_events("completed", None))));
    let server = serve_codex(http, no_ws(), 0).await;
    let mut opts = options(&mock_token("a"), Transport::Auto);
    opts.session_id = Some("ws-idle-before-start".into());
    opts.timeout_ms = Some(100);
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(text_of(&message), "Hello", "{:?}", message.error_message);
    assert_eq!(server.connections()[0].messages.len(), 1);
    assert_eq!(server.requests().len(), 1);
    let stats = get_openai_codex_websocket_debug_stats("ws-idle-before-start").unwrap();
    assert_eq!(
        stats.last_websocket_error.as_deref(),
        Some("WebSocket idle timeout after 100ms")
    );
}

#[tokio::test]
async fn idle_websocket_after_start_errors() {
    let ws: WsHandler = Arc::new(|_, _| ws_events(sse_events("completed", None)[..3].to_vec()));
    let server = serve_codex(
        Arc::new(|_| Reply::sse(data_sse(&sse_events("completed", None)))),
        ws,
        0,
    )
    .await;
    let mut opts = options(&mock_token("a"), Transport::Auto);
    opts.session_id = Some("ws-idle-after-start".into());
    opts.timeout_ms = Some(100);
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("WebSocket idle timeout after 100ms")
    );
    assert!(server.requests().is_empty());
    let diagnostics = message.diagnostics.unwrap();
    assert_eq!(
        diagnostics[0].details.as_ref().unwrap()["phase"],
        "after_message_stream_start"
    );
}

#[tokio::test]
async fn websocket_cached_mode_sends_input_deltas() {
    let ws: WsHandler = Arc::new(|_, body| {
        let first = body.get("previous_response_id").is_none();
        let response_id = if first { "resp_1" } else { "resp_2" };
        let mut events =
            vec![json!({ "type": "response.created", "response": { "id": response_id } })];
        if first {
            let item = json!({ "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "sample_tool", "input": "" });
            events.push(json!({ "type": "response.output_item.added", "item": item }));
            events.push(json!({ "type": "response.custom_tool_call_input.delta", "item_id": "ctc_1", "delta": "abc" }));
            events.push(json!({ "type": "response.custom_tool_call_input.done", "item_id": "ctc_1", "input": "abc" }));
            let mut done = item.clone();
            done["input"] = json!("abc");
            events.push(json!({ "type": "response.output_item.done", "item": done }));
        }
        events.push(json!({ "type": "response.completed", "response": { "id": response_id, "status": "completed", "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8, "input_tokens_details": { "cached_tokens": 0 } } } }));
        ws_events(events)
    });
    let server = serve_codex(http_fail(), ws, 0).await;
    let mut model = model(&server.base_url);
    model.compat =
        Some(serde_json::from_value(json!({ "supportsOpenAIGrammarTools": true })).unwrap());
    let mut tool = Tool::new(
        "sample_tool",
        "Sample tool",
        json!({ "type": "object", "properties": { "payload": { "type": "string" } }, "required": ["payload"] }),
    );
    tool.constrained_sampling = Some(
        serde_json::from_value(
            json!({ "type": "grammar", "variants": { "openai_lark": "start: /[a-z]+/" } }),
        )
        .unwrap(),
    );
    let first_context = Context {
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: vec![Message::User(UserMessage::new("Use the tool", 1))],
        tools: Some(vec![tool]),
    };
    let session = "session-deltas";
    let mut opts = options(&mock_token("a"), Transport::WebsocketCached);
    opts.session_id = Some(session.into());
    let first = codex::stream(&model, &normalize_context(&first_context), opts.clone())
        .result()
        .await;
    assert_eq!(
        first.stop_reason,
        StopReason::ToolUse,
        "{:?}",
        first.error_message
    );
    assert_eq!(
        Value::Object(first.content[0].as_tool_call().unwrap().arguments.clone()),
        json!({ "payload": "abc" })
    );

    let mut second_context = first_context.clone();
    second_context.messages.push(first.into());
    second_context.messages.push(
        ToolResultMessage {
            tool_call_id: "call_1|ctc_1".into(),
            tool_name: "sample_tool".into(),
            content: vec![UserContentBlock::text("real result")],
            timestamp: 2,
            ..Default::default()
        }
        .into(),
    );
    second_context
        .messages
        .push(Message::User(UserMessage::new("Now finish", 3)));
    let second = codex::stream(&model, &normalize_context(&second_context), opts)
        .result()
        .await;
    assert_eq!(
        second.stop_reason,
        StopReason::Stop,
        "{:?}",
        second.error_message
    );

    let connections = server.connections();
    assert_eq!(connections.len(), 1);
    let sent = &connections[0].messages;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["store"], false);
    assert!(sent[0].get("previous_response_id").is_none());
    assert_eq!(
        sent[0]["input"],
        json!([{ "role": "user", "content": [{ "type": "input_text", "text": "Use the tool" }] }])
    );
    assert_eq!(sent[1]["previous_response_id"], "resp_1");
    assert_eq!(
        sent[1]["input"],
        json!([
            { "type": "custom_tool_call_output", "call_id": "call_1", "output": "real result" },
            { "role": "user", "content": [{ "type": "input_text", "text": "Now finish" }] }
        ])
    );
    let stats = get_openai_codex_websocket_debug_stats(session).unwrap();
    assert_eq!(stats.requests, 2);
    assert_eq!(
        (stats.connections_created, stats.connections_reused),
        (1, 1)
    );
    assert_eq!((stats.full_context_requests, stats.delta_requests), (1, 1));
    assert_eq!(stats.last_delta_input_items, Some(2));
    assert_eq!(stats.last_previous_response_id.as_deref(), Some("resp_1"));
}

#[tokio::test]
async fn websocket_close_before_completion_falls_back() {
    let ws: WsHandler = Arc::new(|_, _| vec![WsAction::Close(1009, String::new())]);
    let http: Handler = Arc::new(|_| Reply::sse(data_sse(&sse_events("completed", None))));
    let server = serve_codex(http, ws, 0).await;
    let mut opts = options(&mock_token("a"), Transport::Auto);
    opts.session_id = Some("ws-too-big".into());
    let message = codex::stream(&model(&server.base_url), &context("x"), opts)
        .result()
        .await;
    assert_eq!(text_of(&message), "Hello", "{:?}", message.error_message);
    let stats = get_openai_codex_websocket_debug_stats("ws-too-big").unwrap();
    assert_eq!(
        stats.last_websocket_error.as_deref(),
        Some("WebSocket closed 1009 message too big")
    );
}

#[tokio::test]
async fn missing_api_key_and_bad_tokens() {
    let message = codex::stream(
        &model("http://127.0.0.1:1"),
        &context("x"),
        StreamOptions::default(),
    )
    .result()
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some("No API key for provider: openai-codex")
    );
    let message = codex::stream(
        &model("http://127.0.0.1:1"),
        &context("x"),
        options("not-a-jwt", Transport::Sse),
    )
    .result()
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some("Failed to extract accountId from token")
    );
}
