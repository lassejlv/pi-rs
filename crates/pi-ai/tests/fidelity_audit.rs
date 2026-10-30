//! Regression tests for behavior differences found by the line-by-line audit of pi-ai against
//! `packages/ai/src` (abort messages, SDK stream semantics, error formatting, env headers).

mod common;

use std::time::Duration;

use common::*;
use pi_ai::api::{anthropic_messages, google_generative_ai, openai_completions, openai_responses};
use pi_ai::providers::all::get_builtin_model;
use pi_ai::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// A server that answers one request with the response head and `first_body`, then keeps the
/// connection open without sending more (until the test ends). With `hold_headers`, it never
/// answers at all.
async fn stalling_server(content_type: &str, first_body: String, hold_headers: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let content_type = content_type.to_string();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else { return };
        let mut buffer = vec![0u8; 65536];
        let _ = socket.read(&mut buffer).await;
        if !hold_headers {
            let head = format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let chunk = format!("{:x}\r\n{first_body}\r\n", first_body.len());
            let _ = socket.write_all(chunk.as_bytes()).await;
            let _ = socket.flush().await;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });
    format!("http://{addr}")
}

fn completions_model(base_url: &str) -> Model {
    Model {
        id: "local-model".into(),
        name: "Local".into(),
        api: "openai-completions".into(),
        provider: "local".into(),
        base_url: base_url.into(),
        input: vec![ModelInputModality::Text],
        context_window: 32000,
        max_tokens: 4096,
        ..Default::default()
    }
}

fn context() -> TranscriptContext {
    normalize_context(&Context {
        system_prompt: Some("Be brief.".into()),
        messages: vec![Message::User(UserMessage::new("hi", 1))],
        tools: None,
    })
}

fn cancel_after(ms: u64) -> CancellationToken {
    // Build the shared HTTP client first: a cold build under parallel tests can outlast the delay
    // and abort the request before it is sent.
    let _ = pi_ai::utils::http::client_for("http://127.0.0.1", None);
    let signal = CancellationToken::new();
    let trigger = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        trigger.cancel();
    });
    signal
}

// ---------------------------------------------------------------------------
// openai-completions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn completions_abort_mid_stream_finishes_blocks_then_reports_request_was_aborted() {
    // The openai SDK stream ends quietly when the request is aborted; pi then finishes the open
    // blocks and throws "Request was aborted".
    let first = format!("data: {}\n\n", json!({"id": "c1", "choices": [{"index": 0, "delta": {"content": "Hel"}}]}));
    let base = stalling_server("text/event-stream", first, false).await;
    let options = StreamOptions { api_key: Some("k".into()), signal: Some(cancel_after(300)), ..Default::default() };
    let (events, result) = openai_completions::stream(&completions_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request was aborted"));
    let types: Vec<&str> = events.iter().map(|e| e.type_name()).collect();
    assert_eq!(types, vec!["start", "text_start", "text_delta", "text_end", "error"]);
}

#[tokio::test]
async fn completions_abort_before_response_reports_request_aborted() {
    // provider-retry replaces the SDK's APIUserAbortError with `new Error("Request aborted")`.
    let base = stalling_server("text/event-stream", String::new(), true).await;
    let options = StreamOptions { api_key: Some("k".into()), signal: Some(cancel_after(200)), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request aborted"));
}

#[tokio::test]
async fn completions_processes_named_events_like_the_sdk() {
    // `Stream.fromSSEResponse` yields every event whose name does not start with `thread.`.
    let body = format!(
        "event: chunk\ndata: {}\n\nevent: chunk\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id": "c1", "choices": [{"index": 0, "delta": {"content": "Hello"}}]}),
        json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    );
    let server = serve(vec![MockResponse::sse(body)]).await;
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&server.base_url), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.text(), "Hello");
}

#[tokio::test]
async fn completions_stream_errors_follow_the_sdk() {
    // An `error` event raises APIError(data.error ?? data); raw metadata is appended.
    let body = format!(
        "event: error\ndata: {}\n\n",
        json!({"error": {"message": "Provider returned error", "metadata": {"raw": "upstream \"detail\""}}})
    );
    let server = serve(vec![MockResponse::sse(body)]).await;
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&server.base_url), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Provider returned error\nupstream \"detail\""));

    // Malformed JSON surfaces the SDK's SyntaxError message.
    let server = serve(vec![MockResponse::sse("data: {not json\n\n")]).await;
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&server.base_url), &context(), options).collect_all().await;
    assert_eq!(result.error_message.as_deref(), Some("Error reading response: malformed server-sent event JSON."));
}

#[tokio::test]
async fn completions_http_errors_append_openrouter_raw_metadata() {
    // `error.error.metadata.raw` is appended to HTTP status errors too, unless already present.
    let body = json!({"error": {"message": "Provider returned error", "code": 400, "metadata": {"raw": "bad \"input\""}}});
    let server = serve(vec![MockResponse::json(400, body.to_string())]).await;
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&server.base_url), &context(), options).collect_all().await;
    let expected_body = json!({"message": "Provider returned error", "code": 400, "metadata": {"raw": "bad \"input\""}}).to_string();
    assert_eq!(result.error_message, Some(format!("400: {expected_body}\nbad \"input\"")));
}

// ---------------------------------------------------------------------------
// openai-responses
// ---------------------------------------------------------------------------

fn responses_model(base_url: &str) -> Model {
    Model { api: "openai-responses".into(), ..completions_model(base_url) }
}

#[tokio::test]
async fn responses_abort_messages_follow_the_sdk_paths() {
    // Mid-stream: the SDK stream ends quietly, so processResponsesStream's terminal-event check
    // fails before the abort check runs.
    let first = format!("data: {}\n\n", json!({"type": "response.created", "response": {"id": "r1"}}));
    let base = stalling_server("text/event-stream", first, false).await;
    let options = StreamOptions { api_key: Some("k".into()), signal: Some(cancel_after(300)), ..Default::default() };
    let (_, result) = openai_responses::stream(&responses_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("OpenAI Responses stream ended before a terminal response event"));

    let base = stalling_server("text/event-stream", String::new(), true).await;
    let options = StreamOptions { api_key: Some("k".into()), signal: Some(cancel_after(200)), ..Default::default() };
    let (_, result) = openai_responses::stream(&responses_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.error_message.as_deref(), Some("Request aborted"));
}

#[tokio::test]
async fn openai_sdk_env_headers() {
    // One test owns these process env vars (tests in this binary run concurrently).
    unsafe {
        std::env::set_var("OPENAI_ORG_ID", " org-1 ");
        std::env::set_var("OPENAI_PROJECT_ID", "proj-1");
        std::env::set_var("OPENAI_CUSTOM_HEADERS", "X-Custom: one\nx-other:two\nno colon here");
    }
    let body = openai_sse_body(&[json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})]);
    let server = serve(vec![MockResponse::sse(body)]).await;
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&server.base_url), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let request = server.requests.lock()[0].clone();
    assert_eq!(request.header("openai-organization"), Some("org-1"));
    assert_eq!(request.header("openai-project"), Some("proj-1"));
    assert_eq!(request.header("x-custom"), Some("one"));
    assert_eq!(request.header("x-other"), Some("two"));

    // An invalid header name fails like the SDK's buildHeaders.
    unsafe { std::env::set_var("OPENAI_CUSTOM_HEADERS", "bad name: x") };
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = openai_completions::stream(&completions_model(&server.base_url), &context(), options).collect_all().await;
    assert_eq!(result.error_message.as_deref(), Some("Header name must be a valid HTTP token [\"bad name\"]"));
    unsafe {
        std::env::remove_var("OPENAI_ORG_ID");
        std::env::remove_var("OPENAI_PROJECT_ID");
        std::env::remove_var("OPENAI_CUSTOM_HEADERS");
    }
}

// ---------------------------------------------------------------------------
// anthropic-messages
// ---------------------------------------------------------------------------

fn anthropic_model(base_url: &str) -> Model {
    let mut model = get_builtin_model("anthropic", "claude-haiku-4-5").unwrap();
    model.base_url = base_url.to_string();
    model
}

#[tokio::test]
async fn anthropic_abort_messages_follow_each_ts_path() {
    // Mid-stream: the pending body read rejects with undici's DOMException.
    let first = format!(
        "event: message_start\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"id": "m1", "usage": {"input_tokens": 1, "output_tokens": 0}}})
    );
    let base = stalling_server("text/event-stream", first, false).await;
    let options = StreamOptions { api_key: Some("sk-ant-x".into()), signal: Some(cancel_after(300)), ..Default::default() };
    let (_, result) = anthropic_messages::stream(&anthropic_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("This operation was aborted"));

    // Before the response: provider-retry's "Request aborted".
    let base = stalling_server("text/event-stream", String::new(), true).await;
    let options = StreamOptions { api_key: Some("sk-ant-x".into()), signal: Some(cancel_after(200)), ..Default::default() };
    let (_, result) = anthropic_messages::stream(&anthropic_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.error_message.as_deref(), Some("Request aborted"));

    // Already aborted: `signal.throwIfAborted()` throws the default abort reason.
    let signal = CancellationToken::new();
    signal.cancel();
    let options = StreamOptions { api_key: Some("sk-ant-x".into()), signal: Some(signal), ..Default::default() };
    let (_, result) = anthropic_messages::stream(&anthropic_model(&base), &context(), options).collect_all().await;
    assert_eq!(result.error_message.as_deref(), Some("This operation was aborted"));
}

// ---------------------------------------------------------------------------
// google-generative-ai
// ---------------------------------------------------------------------------

#[tokio::test]
async fn google_generated_tool_call_ids_use_the_js_name_text() {
    // `${functionCall.name}_${Date.now()}_${++counter}`: "" stays empty, a missing name is "undefined".
    let chunk = json!({
        "candidates": [{ "finishReason": "STOP", "content": { "parts": [
            { "functionCall": { "name": "", "args": {} } },
            { "functionCall": { "args": {} } }
        ] } }]
    });
    let server = serve(vec![MockResponse::sse(format!("data: {chunk}\r\n\r\n"))]).await;
    let mut model = get_builtin_model("google", "gemini-2.5-flash").unwrap();
    model.base_url = format!("{}/v1beta", server.base_url);
    let options = StreamOptions { api_key: Some("k".into()), ..Default::default() };
    let (_, result) = google_generative_ai::stream(&model, &context(), options).collect_all().await;
    let ids: Vec<String> = result.tool_calls().map(|c| c.id.clone()).collect();
    assert_eq!(ids.len(), 2, "{:?}", result.error_message);
    assert!(ids[0].starts_with('_'), "{}", ids[0]);
    assert!(ids[1].starts_with("undefined_"), "{}", ids[1]);
}
