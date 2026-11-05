//! OpenAI Codex (ChatGPT backend) Responses API (port of `api/openai-codex-responses.ts`).
//!
//! Two transports: a WebSocket transport (`wss://.../codex/responses`, optionally keeping a
//! per-session connection with `previous_response_id` continuation) and an SSE transport
//! (`POST .../codex/responses` with a zstd-compressed body). `auto` tries the WebSocket first and
//! falls back to SSE when it fails before any output.
//!
//! API-specific options are read from `StreamOptions.extra`: `reasoningEffort`,
//! `reasoningSummary`, `serviceTier`, `textVerbosity`, `toolChoice`.
// wasm32 compiles out the native-only transports and sources below.
#![cfg_attr(target_arch = "wasm32", allow(unused_imports, unused_variables, unused_mut, dead_code))]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

use base64::Engine;
use futures::{FutureExt, SinkExt, StreamExt};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
#[cfg(not(target_arch = "wasm32"))]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::tungstenite::Message as WsMessage;
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::constrained_sampling::create_grammar_tool_input_properties;
use super::openai_prompt_cache::clamp_openai_prompt_cache_key;
use super::openai_responses::{error_message_of, simple_reasoning_effort, thinking_level_entry};
use super::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, DefaultToolStrict,
    OpenAIResponsesStreamOptions, ResponsesStreamProcessor, apply_service_tier_multiplier,
    convert_responses_messages, convert_responses_tools, get_responses_tool_support,
};
use super::simple_options::build_base_options;
use crate::models::ProviderStreams;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, Message, Model, ProviderEnv,
    ProviderHeaders, ProviderResponse, StopReason, StreamOptions, TranscriptContext, Transport,
    Usage,
};
use crate::utils::diagnostics::{
    append_assistant_message_diagnostic, create_assistant_message_diagnostic,
    diagnostic_error_from_message,
};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::headers::headers_to_record;
use crate::utils::http::{HeaderList, client_for};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::node_http_proxy::get_proxy_url_for_target;
use crate::utils::text::get_system_message_text;
use crate::utils::transcript::{
    get_declared_tools, get_initial_system_message, resolve_transcript, resolve_transcript_tools,
    transcript_from_messages,
};
use crate::utils::uuid::uuidv7;

// ============================================================================
// Configuration
// ============================================================================

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
const CODEX_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const CODEX_RESPONSE_STATUSES: [&str; 6] = [
    "completed",
    "incomplete",
    "failed",
    "cancelled",
    "queued",
    "in_progress",
];

const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const SESSION_WEBSOCKET_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const SESSION_WEBSOCKET_MAX_AGE_MS: u64 = 55 * 60 * 1000;

// ============================================================================
// Errors
// ============================================================================

/// An error reported by the Codex backend (`error` / `response.failed` events).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CodexApiError {
    pub message: String,
    pub code: Option<String>,
    pub payload: Option<Value>,
}

/// Malformed data from the Codex backend.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CodexProtocolError {
    pub message: String,
    pub payload: Option<String>,
}

/// The WebSocket closed before the response completed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct WebSocketCloseError {
    pub message: String,
    pub code: Option<u16>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
struct RetryDelayExceededError(String);

fn is_codex_non_transport_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CodexApiError>().is_some()
        || error.downcast_ref::<CodexProtocolError>().is_some()
}

fn codex_error_code(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<CodexApiError>()
        .and_then(|e| e.code.as_deref())
}

fn aborted_error() -> anyhow::Error {
    anyhow::anyhow!("Request was aborted")
}

// ============================================================================
// Options
// ============================================================================

#[derive(Debug, Clone, Default)]
struct CodexOptions {
    reasoning_effort: Option<String>,
    reasoning_summary: Option<Value>,
    service_tier: Option<String>,
    text_verbosity: Option<String>,
    tool_choice: Option<Value>,
}

impl CodexOptions {
    fn from(options: &StreamOptions) -> Self {
        Self {
            reasoning_effort: options.extra_str("reasoningEffort").map(str::to_string),
            reasoning_summary: options.extra_value("reasoningSummary").cloned(),
            service_tier: options.extra_str("serviceTier").map(str::to_string),
            text_verbosity: options
                .extra_str("textVerbosity")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            tool_choice: options
                .extra_value("toolChoice")
                .filter(|v| !v.is_null())
                .cloned(),
        }
    }
}

fn assert_successful_output(output: &AssistantMessage) -> anyhow::Result<()> {
    match output.stop_reason {
        StopReason::Pending => anyhow::bail!("Codex stream ended without a stop reason"),
        StopReason::Error | StopReason::Aborted => {
            anyhow::bail!(
                "{}",
                output
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "An unknown error occurred".into())
            )
        }
        _ => Ok(()),
    }
}

// ============================================================================
// Retry helpers
// ============================================================================

fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    static RE: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing").unwrap()
    });
    RE.is_match(error_text)
}

fn is_retryable_error(status: u16, error_text: &str) -> bool {
    static RE: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"(?i)rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused").unwrap()
    });
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    RE.is_match(error_text)
}

/// `Number(value)` for header values.
fn js_number(value: &str) -> f64 {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

fn parse_http_date_ms(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<i64> = parts[4].split(':').filter_map(|p| p.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2]) * 1000) as f64)
}

fn get_retry_after_delay_ms(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    if let Some(value) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()) {
        let millis = js_number(value);
        if millis.is_finite() {
            return Some(millis.max(0.0));
        }
    }
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())?;
    let seconds = js_number(retry_after);
    if seconds.is_finite() {
        return Some((seconds * 1000.0).max(0.0));
    }
    parse_http_date_ms(retry_after).map(|date| (date - crate::utils::now_ms() as f64).max(0.0))
}

fn validate_retry_delay_ms(delay_ms: f64, options: &StreamOptions) -> anyhow::Result<f64> {
    let max = options
        .max_retry_delay_ms
        .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS) as f64;
    if max > 0.0 && delay_ms > max {
        return Err(RetryDelayExceededError(format!(
            "Server requested {}s retry delay (max: {}s)",
            (delay_ms / 1000.0).ceil(),
            (max / 1000.0).ceil()
        ))
        .into());
    }
    Ok(delay_ms)
}

async fn sleep(ms: f64, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    let duration = Duration::from_millis(ms.max(0.0) as u64);
    match signal {
        Some(signal) => {
            if signal.is_cancelled() {
                return Err(aborted_error());
            }
            tokio::select! {
                _ = signal.cancelled() => Err(aborted_error()),
                _ = crate::utils::wasm::sleep(duration) => Ok(()),
            }
        }
        None => {
            crate::utils::wasm::sleep(duration).await;
            Ok(())
        }
    }
}

// ============================================================================
// Request building
// ============================================================================

fn effort_entry(model: &Model, level: &str) -> Option<Option<String>> {
    thinking_level_entry(model, level)
}

/// Build the Codex request body.
pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    cache_session_id: Option<&str>,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    let codex = CodexOptions::from(options);
    let compat = model.compat();
    let supports_strict_mode = compat.supports_strict_mode.unwrap_or(true);
    let supports_openai_grammar_tools = compat.supports_openai_grammar_tools.unwrap_or(false);
    let supports_additional_tools = compat.supports_additional_tools.unwrap_or(false);
    let supports_tool_search = compat.supports_tool_search.unwrap_or(false);
    let supports_mid_convo = compat.supports_mid_convo_system_messages.unwrap_or(false);
    let tool_options = ConvertResponsesToolsOptions {
        strict: DefaultToolStrict::Null,
        supports_strict_mode: Some(supports_strict_mode),
        supports_openai_grammar_tools,
        tool_search_result: false,
    };
    let transcript_tools = resolve_transcript_tools(
        &context.messages,
        get_responses_tool_support(
            supports_mid_convo,
            supports_additional_tools,
            supports_tool_search,
        ),
    );
    let messages = convert_responses_messages(
        model,
        context,
        &CODEX_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            include_system_prompt: Some(false),
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            supports_mid_convo_system_messages: supports_mid_convo,
            supports_additional_tools,
            supports_tool_search,
            tool_options: tool_options.clone(),
            configuration_update_effort: None,
        },
    )?;
    let instructions = get_initial_system_message(&context.messages)
        .map(get_system_message_text)
        .unwrap_or_default();

    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    body.insert(
        "instructions".into(),
        json!(if instructions.is_empty() {
            "You are a helpful assistant.".to_string()
        } else {
            instructions
        }),
    );
    body.insert("input".into(), Value::Array(messages));
    body.insert(
        "text".into(),
        json!({ "verbosity": codex.text_verbosity.clone().unwrap_or_else(|| "low".into()) }),
    );
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(key) = cache_session_id {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    body.insert(
        "tool_choice".into(),
        codex.tool_choice.clone().unwrap_or_else(|| json!("auto")),
    );
    body.insert("parallel_tool_calls".into(), json!(true));
    if let Some(temperature) = options.temperature {
        body.insert(
            "temperature".into(),
            crate::serde_util::js_number(temperature),
        );
    }
    if let Some(tier) = options.extra_value("serviceTier").filter(|v| !v.is_null()) {
        body.insert("service_tier".into(), tier.clone());
    }
    if !transcript_tools.request_tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_responses_tools(
                &transcript_tools.request_tools,
                &tool_options,
            )?),
        );
    }

    let summary = match &codex.reasoning_summary {
        None | Some(Value::Null) => json!("auto"),
        Some(value) => value.clone(),
    };
    if let Some(requested) = &codex.reasoning_effort {
        let effort: Option<String> = if requested == "none" {
            match effort_entry(model, "off") {
                None => Some("none".into()),
                Some(off) => off,
            }
        } else {
            match effort_entry(model, requested) {
                Some(Some(mapped)) => Some(mapped),
                _ => Some(requested.clone()),
            }
        };
        if let Some(effort) = effort {
            body.insert(
                "reasoning".into(),
                json!({ "effort": effort, "summary": summary }),
            );
        }
    } else if model.reasoning {
        match effort_entry(model, "off") {
            Some(None) => {}
            Some(Some(off)) => {
                body.insert("reasoning".into(), json!({ "effort": off }));
            }
            None => {
                body.insert("reasoning".into(), json!({ "effort": "none" }));
            }
        }
    }
    Ok(Value::Object(body))
}

fn get_service_tier_cost_multiplier(model: &Model, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model.id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

fn resolve_codex_service_tier(
    response_tier: Option<&str>,
    request_tier: Option<&str>,
) -> Option<String> {
    if response_tier == Some("default") && matches!(request_tier, Some("flex") | Some("priority")) {
        return request_tier.map(str::to_string);
    }
    response_tier.or(request_tier).map(str::to_string)
}

/// The SSE endpoint for a base URL.
pub fn resolve_codex_url(base_url: Option<&str>) -> String {
    let raw = base_url
        .filter(|b| !b.trim().is_empty())
        .unwrap_or(DEFAULT_CODEX_BASE_URL);
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// The WebSocket endpoint for a base URL.
pub fn resolve_codex_websocket_url(base_url: Option<&str>) -> String {
    let url = resolve_codex_url(base_url);
    match url::Url::parse(&url) {
        Ok(mut parsed) => {
            let scheme = match parsed.scheme() {
                "https" => Some("wss"),
                "http" => Some("ws"),
                _ => None,
            };
            if let Some(scheme) = scheme {
                let _ = parsed.set_scheme(scheme);
            }
            parsed.to_string()
        }
        Err(_) => url,
    }
}

// ============================================================================
// Auth & headers
// ============================================================================

/// `atob` (forgiving base64) decoded to a Latin-1 string.
fn atob(input: &str) -> Option<String> {
    let cleaned: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let bytes = engine.decode(cleaned).ok()?;
    Some(bytes.into_iter().map(|b| b as char).collect())
}

/// The ChatGPT account id from a Codex access token.
pub fn extract_account_id(token: &str) -> anyhow::Result<String> {
    let extract = || -> Option<String> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let payload: Value = serde_json::from_str(&atob(parts[1])?).ok()?;
        let account = payload.get(JWT_CLAIM_PATH)?.get("chatgpt_account_id")?;
        match account {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Null | Value::Bool(false) => None,
            Value::String(_) => None,
            other => Some(other.to_string()),
        }
    };
    extract().ok_or_else(|| anyhow::anyhow!("Failed to extract accountId from token"))
}

fn build_base_codex_headers(
    default_headers: &[(String, String)],
    init_headers: Option<&indexmap::IndexMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
) -> HeaderList {
    let mut headers = HeaderList::new();
    for (name, value) in default_headers {
        headers.set(name, Some(value));
    }
    if let Some(init) = init_headers {
        headers.extend_strings(init);
    }
    if let Some(additional) = additional_headers {
        headers.extend_optional(additional);
    }
    headers.set("Authorization", Some(&format!("Bearer {token}")));
    headers.set("chatgpt-account-id", Some(account_id));
    headers
}

fn build_sse_headers(
    default_headers: &[(String, String)],
    init_headers: Option<&indexmap::IndexMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> HeaderList {
    let mut headers = build_base_codex_headers(
        default_headers,
        init_headers,
        additional_headers,
        account_id,
        token,
    );
    headers.set("OpenAI-Beta", Some("responses=experimental"));
    headers.set("accept", Some("text/event-stream"));
    headers.set("content-type", Some("application/json"));
    if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
        headers.set("session-id", Some(session_id));
        headers.set("x-client-request-id", Some(session_id));
    }
    headers
}

fn build_websocket_headers(
    default_headers: &[(String, String)],
    init_headers: Option<&indexmap::IndexMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> HeaderList {
    let mut headers = build_base_codex_headers(
        default_headers,
        init_headers,
        additional_headers,
        account_id,
        token,
    );
    headers.set("accept", None);
    headers.set("content-type", None);
    headers.set("OpenAI-Beta", Some(OPENAI_BETA_RESPONSES_WEBSOCKETS));
    headers.set("x-client-request-id", Some(request_id));
    headers.set("session-id", Some(request_id));
    headers
}

// ============================================================================
// Error handling
// ============================================================================

struct ErrorInfo {
    message: String,
    friendly_message: Option<String>,
}

fn parse_error_response(status: u16, status_text: &str, raw: &str) -> ErrorInfo {
    static CODE_RE: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"(?i)usage_limit_reached|usage_not_included|rate_limit_exceeded")
            .unwrap()
    });
    let mut message = if !raw.is_empty() {
        raw.to_string()
    } else if !status_text.is_empty() {
        status_text.to_string()
    } else {
        "Request failed".into()
    };
    let mut friendly_message = None;
    if let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && let Some(err) = parsed.get("error").filter(|e| !e.is_null())
    {
        let code = err
            .get("code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| err.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        if CODE_RE.is_match(code) || status == 429 {
            let plan = err
                .get("plan_type")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .map(|p| format!(" ({} plan)", p.to_lowercase()))
                .unwrap_or_default();
            let when = err
                .get("resets_at")
                .and_then(Value::as_f64)
                .filter(|r| *r != 0.0)
                .map(|resets| {
                    let mins = ((resets * 1000.0 - crate::utils::now_ms() as f64) / 60000.0)
                        .round()
                        .max(0.0);
                    format!(" Try again in ~{mins} min.")
                })
                .unwrap_or_default();
            friendly_message = Some(
                format!("You have hit your ChatGPT usage limit{plan}.{when}")
                    .trim()
                    .to_string(),
            );
        }
        if let Some(m) = err
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            message = m.to_string();
        } else if let Some(f) = &friendly_message {
            message = f.clone();
        }
    }
    ErrorInfo {
        message,
        friendly_message,
    }
}

// ============================================================================
// Event mapping (mapCodexEvents)
// ============================================================================

fn extract_codex_event_error(event: &Value) -> (Option<String>, Option<String>) {
    let nested = event.get("error").filter(|e| e.is_object());
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| nested.and_then(|n| n.get("code")).and_then(Value::as_str))
        .map(str::to_string);
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            nested
                .and_then(|n| n.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    (code, message)
}

enum MappedEvent {
    /// A normal event for the Responses processor.
    Event(Value),
    /// The terminal event, normalized to `response.completed`. Stop reading after it.
    Terminal(Value),
    /// Ignore (no `type`).
    Skip,
}

fn map_codex_event(event: Value, output: &mut AssistantMessage) -> anyhow::Result<MappedEvent> {
    let Some(event_type) = event
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
    else {
        return Ok(MappedEvent::Skip);
    };
    match event_type.as_str() {
        "error" => {
            let (code, message) = extract_codex_event_error(&event);
            let detail = message
                .filter(|m| !m.is_empty())
                .or_else(|| code.clone().filter(|c| !c.is_empty()))
                .unwrap_or_else(|| event.to_string());
            Err(CodexApiError {
                message: format!("Codex error: {detail}"),
                code,
                payload: Some(event),
            }
            .into())
        }
        "response.failed" => {
            let error = event.get("response").and_then(|r| r.get("error"));
            let code = error
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let message = error
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("Codex response failed")
                .to_string();
            Err(CodexApiError {
                message,
                code,
                payload: Some(event),
            }
            .into())
        }
        "response.done" | "response.completed" | "response.incomplete" => {
            let mut event = event;
            if let Some(Value::Object(response)) = event.get_mut("response") {
                if let Some(Value::Bool(end_turn)) = response.get("end_turn") {
                    output.end_turn = Some(*end_turn);
                }
                let status = response
                    .get("status")
                    .and_then(Value::as_str)
                    .filter(|s| CODEX_RESPONSE_STATUSES.contains(s))
                    .map(str::to_string);
                match status {
                    Some(status) => {
                        response.insert("status".into(), json!(status));
                    }
                    None => {
                        response.remove("status");
                    }
                }
            }
            if let Value::Object(map) = &mut event {
                map.insert("type".into(), json!("response.completed"));
            }
            Ok(MappedEvent::Terminal(event))
        }
        _ => Ok(MappedEvent::Event(event)),
    }
}

/// Feeds Codex events through `mapCodexEvents` semantics into the Responses processor.
struct CodexEventPipeline<'a> {
    processor: ResponsesStreamProcessor<'a>,
    model: &'a Model,
    on_provider_stream_event: Option<crate::types::OnProviderStreamEvent>,
    completed: bool,
}

impl<'a> CodexEventPipeline<'a> {
    fn new(
        model: &'a Model,
        options: &StreamOptions,
        grammar: &'a HashMap<String, String>,
        apply: crate::api::openai_responses_shared::ApplyServiceTierPricing<'a>,
    ) -> Self {
        let codex = CodexOptions::from(options);
        Self {
            processor: ResponsesStreamProcessor::new(
                model,
                OpenAIResponsesStreamOptions {
                    on_provider_stream_event: None,
                    service_tier: codex.service_tier,
                    grammar_tool_input_properties: Some(grammar),
                    resolve_service_tier: Some(resolve_codex_service_tier),
                    apply_service_tier_pricing: Some(apply),
                },
            ),
            model,
            on_provider_stream_event: options.on_provider_stream_event.clone(),
            completed: false,
        }
    }

    /// Observe and map one raw event (`mapCodexEvents`).
    async fn map(&self, event: Value, output: &mut AssistantMessage) -> anyhow::Result<MappedEvent> {
        if let Some(observer) = &self.on_provider_stream_event {
            observer(event.clone(), self.model.clone()).await;
        }
        map_codex_event(event, output)
    }

    /// Process a mapped event. Returns true once the terminal event was processed.
    fn process(
        &mut self,
        mapped: MappedEvent,
        output: &mut AssistantMessage,
        sender: &AssistantMessageEventSender,
    ) -> anyhow::Result<bool> {
        match mapped {
            MappedEvent::Skip => Ok(false),
            MappedEvent::Event(event) => {
                self.processor.handle_event(&event, output, sender)?;
                Ok(false)
            }
            MappedEvent::Terminal(event) => {
                self.processor.handle_event(&event, output, sender)?;
                self.completed = true;
                Ok(true)
            }
        }
    }

    /// Returns true once the terminal event was processed.
    async fn handle(
        &mut self,
        event: Value,
        output: &mut AssistantMessage,
        sender: &AssistantMessageEventSender,
    ) -> anyhow::Result<bool> {
        let mapped = self.map(event, output).await?;
        self.process(mapped, output, sender)
    }

    fn finish(&self, output: &AssistantMessage) -> anyhow::Result<()> {
        self.processor.finish(output)
    }
}

// ============================================================================
// SSE parsing
// ============================================================================

/// Codex's SSE frame parser: frames split on blank lines, `data:` lines joined.
#[derive(Default)]
struct CodexSseDecoder {
    buffer: String,
    pending: Vec<u8>,
}

impl CodexSseDecoder {
    fn push(&mut self, bytes: &[u8], done: bool) -> anyhow::Result<Vec<Value>> {
        self.pending.extend_from_slice(bytes);
        let text = if done {
            String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
        } else {
            let valid = match std::str::from_utf8(&self.pending) {
                Ok(_) => self.pending.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => self.pending.len(),
            };
            let rest = self.pending.split_off(valid);
            let chunk = std::mem::replace(&mut self.pending, rest);
            String::from_utf8_lossy(&chunk).into_owned()
        };
        self.buffer.push_str(&text);
        if done && !self.buffer.trim().is_empty() {
            self.buffer.push_str("\n\n");
        }
        let mut events = Vec::new();
        while let Some(idx) = self.buffer.find("\n\n") {
            let chunk = self.buffer[..idx].to_string();
            self.buffer = self.buffer[idx + 2..].to_string();
            let data_lines: Vec<&str> = chunk
                .split('\n')
                .filter(|l| l.starts_with("data:"))
                .map(|l| l[5..].trim())
                .collect();
            if data_lines.is_empty() {
                continue;
            }
            let data = data_lines.join("\n");
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            match crate::utils::js_json::js_json_parse(data) {
                Ok(value) => events.push(value),
                Err(cause) => {
                    return Err(CodexProtocolError {
                        message: format!("Invalid Codex SSE JSON: {cause}"),
                        payload: Some(data.to_string()),
                    }
                    .into());
                }
            }
        }
        Ok(events)
    }
}

async fn process_sse_stream(
    response: reqwest::Response,
    output: &mut AssistantMessage,
    sender: &AssistantMessageEventSender,
    pipeline: &mut CodexEventPipeline<'_>,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<()> {
    let mut body = response.bytes_stream();
    let mut decoder = CodexSseDecoder::default();
    'outer: loop {
        if signal.is_some_and(|s| s.is_cancelled()) {
            return Err(aborted_error());
        }
        let chunk = match signal {
            Some(signal) => tokio::select! {
                chunk = body.next() => chunk,
                _ = signal.cancelled() => return Err(aborted_error()),
            },
            None => body.next().await,
        };
        if signal.is_some_and(|s| s.is_cancelled()) {
            return Err(aborted_error());
        }
        let (events, done) = match chunk {
            Some(Ok(bytes)) => (decoder.push(&bytes, false)?, false),
            Some(Err(error)) => return Err(error.into()),
            None => (decoder.push(&[], true)?, true),
        };
        for event in events {
            if pipeline.handle(event, output, sender).await? {
                break 'outer;
            }
        }
        if done {
            break;
        }
    }
    pipeline.finish(output)
}

// ============================================================================
// Request compression
// ============================================================================

fn compress_request_body_zstd(body_json: &str) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(
        body_json.as_bytes(),
        ruzstd::encoding::CompressionLevel::Fastest,
    )
}

// ============================================================================
// WebSocket transport
// ============================================================================

#[cfg(not(target_arch = "wasm32"))]
trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncIo for T {}

#[cfg(not(target_arch = "wasm32"))]
type WsStream = WebSocketStream<MaybeTlsStream<Box<dyn AsyncIo>>>;

/// WebSocket transport statistics for one session (`OpenAICodexWebSocketDebugStats`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenAICodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: usize,
    pub last_delta_input_items: Option<usize>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: Option<bool>,
    pub last_websocket_error: Option<String>,
}

#[derive(Debug, Clone)]
struct ContinuationState {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

#[cfg(not(target_arch = "wasm32"))]
struct CachedConnection {
    socket: Option<WsStream>,
    busy: bool,
    created_at: Instant,
    idle_generation: u64,
    continuation: Option<ContinuationState>,
}

#[cfg(not(target_arch = "wasm32"))]
type CachedEntry = Arc<Mutex<CachedConnection>>;

#[cfg(not(target_arch = "wasm32"))]
static WEBSOCKET_SESSION_CACHE: Lazy<Mutex<HashMap<String, HashMap<String, CachedEntry>>>> =
    Lazy::new(Default::default);
static WEBSOCKET_DEBUG_STATS: Lazy<Mutex<HashMap<String, OpenAICodexWebSocketDebugStats>>> =
    Lazy::new(Default::default);
static WEBSOCKET_SSE_FALLBACK_SESSIONS: Lazy<Mutex<HashSet<String>>> = Lazy::new(Default::default);
static CLEANUP_REGISTERED: Lazy<()> = Lazy::new(|| {
    let unregister = crate::session_resources::register_session_resource_cleanup(Arc::new(
        |session_id: Option<&str>| {
            close_openai_codex_websocket_sessions(session_id);
            Ok(())
        },
    ));
    // The cleanup stays registered for the life of the process.
    std::mem::forget(unregister);
});

fn with_debug_stats<R>(
    session_id: &str,
    f: impl FnOnce(&mut OpenAICodexWebSocketDebugStats) -> R,
) -> R {
    let mut stats = WEBSOCKET_DEBUG_STATS.lock();
    f(stats.entry(session_id.to_string()).or_default())
}

/// WebSocket statistics for a session, if any request used it.
pub fn get_openai_codex_websocket_debug_stats(
    session_id: &str,
) -> Option<OpenAICodexWebSocketDebugStats> {
    WEBSOCKET_DEBUG_STATS.lock().get(session_id).cloned()
}

/// Reset statistics (and the SSE fallback) for one session, or all sessions.
pub fn reset_openai_codex_websocket_debug_stats(session_id: Option<&str>) {
    match session_id {
        Some(id) => {
            WEBSOCKET_DEBUG_STATS.lock().remove(id);
            WEBSOCKET_SSE_FALLBACK_SESSIONS.lock().remove(id);
        }
        None => {
            WEBSOCKET_DEBUG_STATS.lock().clear();
            WEBSOCKET_SSE_FALLBACK_SESSIONS.lock().clear();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn close_websocket_silently(socket: WsStream, code: u16, reason: &str) {
    let reason = reason.to_string();
    let close = async move {
        let mut socket = socket;
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        };
        let _ = crate::utils::wasm::timeout(Duration::from_secs(5), socket.close(Some(frame))).await;
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(close);
    }
}

/// Close cached WebSocket connections: wasm32 has no WebSocket transport.
#[cfg(target_arch = "wasm32")]
pub fn close_openai_codex_websocket_sessions(_session_id: Option<&str>) {}
#[cfg(not(target_arch = "wasm32"))]
/// Close cached WebSocket connections for one session, or all sessions.
pub fn close_openai_codex_websocket_sessions(session_id: Option<&str>) {
    let entries: Vec<CachedEntry> = {
        let mut cache = WEBSOCKET_SESSION_CACHE.lock();
        match session_id {
            Some(id) => cache
                .remove(id)
                .map(|m| m.into_values().collect())
                .unwrap_or_default(),
            None => cache.drain().flat_map(|(_, m)| m.into_values()).collect(),
        }
    };
    for entry in entries {
        let mut entry = entry.lock();
        entry.idle_generation += 1;
        if let Some(socket) = entry.socket.take() {
            close_websocket_silently(socket, 1000, "debug_close");
        }
    }
}

fn is_websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    session_id.is_some_and(|id| WEBSOCKET_SSE_FALLBACK_SESSIONS.lock().contains(id))
}

fn record_websocket_sse_fallback(session_id: Option<&str>) {
    let Some(id) = session_id else { return };
    let active = is_websocket_sse_fallback_active(Some(id));
    with_debug_stats(id, |stats| {
        stats.sse_fallbacks += 1;
        stats.websocket_fallback_active = Some(active);
    });
}

fn record_websocket_failure(session_id: Option<&str>, error: &anyhow::Error) {
    let Some(id) = session_id else { return };
    WEBSOCKET_SSE_FALLBACK_SESSIONS
        .lock()
        .insert(id.to_string());
    with_debug_stats(id, |stats| {
        stats.websocket_failures += 1;
        stats.last_websocket_error = Some(error.to_string());
        stats.websocket_fallback_active = Some(true);
    });
}

#[cfg(not(target_arch = "wasm32"))]
/// Whether an idle socket is still open (no pending close or error).
fn is_websocket_reusable(socket: &mut WsStream) -> bool {
    loop {
        match socket.next().now_or_never() {
            None => return true,
            Some(None) | Some(Some(Err(_))) | Some(Some(Ok(WsMessage::Close(_)))) => return false,
            Some(Some(Ok(_))) => continue,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn schedule_session_websocket_expiry(session_id: String, account_id: String, entry: CachedEntry) {
    let generation = {
        let mut guard = entry.lock();
        guard.idle_generation += 1;
        guard.idle_generation
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        crate::utils::wasm::sleep(Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS)).await;
        let socket = {
            let mut guard = entry.lock();
            if guard.busy || guard.idle_generation != generation {
                return;
            }
            guard.socket.take()
        };
        if let Some(socket) = socket {
            close_websocket_silently(socket, 1000, "idle_timeout");
        }
        remove_cached_entry(&session_id, &account_id, &entry);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn remove_cached_entry(session_id: &str, account_id: &str, entry: &CachedEntry) {
    let mut cache = WEBSOCKET_SESSION_CACHE.lock();
    if let Some(accounts) = cache.get_mut(session_id) {
        if accounts
            .get(account_id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            accounts.remove(account_id);
        }
        if accounts.is_empty() {
            cache.remove(session_id);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn websocket_error_message(error: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) => {
            format!("Unexpected server response: {}", response.status().as_u16())
        }
        other => {
            let text = other.to_string();
            if text.is_empty() {
                "WebSocket error".to_string()
            } else {
                text
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn close_error(frame: Option<&CloseFrame>) -> WebSocketCloseError {
    let code = frame.map(|f| u16::from(f.code));
    let reason = frame
        .map(|f| f.reason.to_string())
        .filter(|r| !r.is_empty());
    let code_text = code.map(|c| format!(" {c}")).unwrap_or_default();
    let mut reason_text = reason.as_ref().map(|r| format!(" {r}")).unwrap_or_default();
    if reason_text.is_empty() && code == Some(WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE) {
        reason_text = " message too big".to_string();
    }
    WebSocketCloseError {
        message: format!("WebSocket closed{code_text}{reason_text}")
            .trim()
            .to_string(),
        code,
        reason,
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Open a TCP connection to `host:port`, tunneling through an HTTP proxy when configured.
async fn open_tcp(
    target_url: &url::Url,
    env: Option<&ProviderEnv>,
) -> anyhow::Result<Box<dyn AsyncIo>> {
    let host = target_url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid WebSocket URL"))?
        .to_string();
    let port = target_url.port_or_known_default().unwrap_or(443);
    let http_target = target_url
        .as_str()
        .replacen("wss:", "https:", 1)
        .replacen("ws:", "http:", 1);
    if let Some(proxy) =
        get_proxy_url_for_target(&http_target, env).and_then(|p| url::Url::parse(&p).ok())
        && matches!(proxy.scheme(), "http" | "https")
    {
        let proxy_host = proxy.host_str().unwrap_or_default().to_string();
        let proxy_port = proxy.port_or_known_default().unwrap_or(80);
        let mut stream = tokio::net::TcpStream::connect((proxy_host.as_str(), proxy_port)).await?;
        let host_port = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let mut request = format!("CONNECT {host_port} HTTP/1.1\r\nHost: {host_port}\r\n");
        if !proxy.username().is_empty() {
            let user = percent_decode(proxy.username());
            let pass = percent_decode(proxy.password().unwrap_or_default());
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await? == 0 {
                anyhow::bail!("Proxy closed the connection during CONNECT");
            }
            response.push(byte[0]);
            if response.len() > 64 * 1024 {
                anyhow::bail!("Proxy CONNECT response too large");
            }
        }
        let head = String::from_utf8_lossy(&response);
        let status = head.split_whitespace().nth(1).unwrap_or_default();
        if status != "200" {
            anyhow::bail!(
                "Proxy CONNECT failed: {}",
                head.lines().next().unwrap_or_default()
            );
        }
        return Ok(Box::new(stream));
    }
    let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
    let _ = stream.set_nodelay(true);
    Ok(Box::new(stream))
}

#[cfg(not(target_arch = "wasm32"))]
fn percent_decode(value: &str) -> String {
    url::form_urlencoded::parse(format!("x={value}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| value.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
async fn connect_websocket(
    url: &str,
    headers: &HeaderList,
    signal: Option<&CancellationToken>,
    connect_timeout_ms: u64,
    env: Option<&ProviderEnv>,
) -> anyhow::Result<WsStream> {
    let connect = async {
        let parsed = url::Url::parse(url)?;
        let mut request = url.into_client_request()?;
        for (name, value) in headers.entries() {
            let name = reqwest::header::HeaderName::from_bytes(name.to_lowercase().as_bytes())?;
            request
                .headers_mut()
                .insert(name, reqwest::header::HeaderValue::from_str(value)?);
        }
        let stream = open_tcp(&parsed, env).await?;
        let (socket, _) =
            tokio_tungstenite::client_async_tls_with_config(request, stream, None, None)
                .await
                .map_err(|e| anyhow::anyhow!(websocket_error_message(&e)))?;
        Ok::<WsStream, anyhow::Error>(socket)
    };
    let timeout = async {
        if connect_timeout_ms > 0 {
            crate::utils::wasm::sleep(Duration::from_millis(connect_timeout_ms)).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let cancelled = async {
        match signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    if signal.is_some_and(|s| s.is_cancelled()) {
        return Err(aborted_error());
    }
    tokio::select! {
        result = connect => result,
        _ = timeout => Err(anyhow::anyhow!("WebSocket connect timeout after {connect_timeout_ms}ms")),
        _ = cancelled => Err(aborted_error()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum Release {
    /// Close the socket when done.
    OneShot,
    /// Return the socket to (or drop it from) the session cache.
    Cached {
        session_id: String,
        account_id: String,
        entry: CachedEntry,
    },
}

#[cfg(not(target_arch = "wasm32"))]
struct AcquiredSocket {
    socket: WsStream,
    entry: Option<CachedEntry>,
    reused: bool,
    release: Release,
}

#[cfg(not(target_arch = "wasm32"))]
impl AcquiredSocket {
    fn release(self, keep: bool) {
        match self.release {
            Release::OneShot => close_websocket_silently(self.socket, 1000, "done"),
            Release::Cached {
                session_id,
                account_id,
                entry,
            } => {
                let mut socket = self.socket;
                if !keep || !is_websocket_reusable(&mut socket) {
                    close_websocket_silently(socket, 1000, "done");
                    entry.lock().idle_generation += 1;
                    remove_cached_entry(&session_id, &account_id, &entry);
                    return;
                }
                {
                    let mut guard = entry.lock();
                    guard.socket = Some(socket);
                    guard.busy = false;
                }
                schedule_session_websocket_expiry(session_id, account_id, entry);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn acquire_websocket(
    url: &str,
    headers: &HeaderList,
    session_id: Option<&str>,
    account_id: &str,
    signal: Option<&CancellationToken>,
    connect_timeout_ms: u64,
    env: Option<&ProviderEnv>,
) -> anyhow::Result<AcquiredSocket> {
    let Some(session_id) = session_id else {
        let socket = connect_websocket(url, headers, signal, connect_timeout_ms, env).await?;
        return Ok(AcquiredSocket {
            socket,
            entry: None,
            reused: false,
            release: Release::OneShot,
        });
    };

    let cached = WEBSOCKET_SESSION_CACHE
        .lock()
        .get(session_id)
        .and_then(|m| m.get(account_id))
        .cloned();
    if let Some(cached) = cached {
        let mut connect_one_shot = false;
        {
            let mut guard = cached.lock();
            guard.idle_generation += 1;
            let expired =
                guard.created_at.elapsed() >= Duration::from_millis(SESSION_WEBSOCKET_MAX_AGE_MS);
            if !guard.busy && expired {
                if let Some(socket) = guard.socket.take() {
                    close_websocket_silently(socket, 1000, "connection_age_limit");
                }
                drop(guard);
                remove_cached_entry(session_id, account_id, &cached);
            } else if !guard.busy {
                let reusable = guard.socket.as_mut().is_some_and(is_websocket_reusable);
                if reusable {
                    guard.busy = true;
                    let socket = guard.socket.take().expect("reusable socket");
                    drop(guard);
                    return Ok(AcquiredSocket {
                        socket,
                        entry: Some(cached.clone()),
                        reused: true,
                        release: Release::Cached {
                            session_id: session_id.to_string(),
                            account_id: account_id.to_string(),
                            entry: cached,
                        },
                    });
                }
                if let Some(socket) = guard.socket.take() {
                    close_websocket_silently(socket, 1000, "done");
                }
                drop(guard);
                remove_cached_entry(session_id, account_id, &cached);
            } else {
                connect_one_shot = true;
            }
        }
        if connect_one_shot {
            let socket = connect_websocket(url, headers, signal, connect_timeout_ms, env).await?;
            return Ok(AcquiredSocket {
                socket,
                entry: None,
                reused: false,
                release: Release::OneShot,
            });
        }
    }

    let socket = connect_websocket(url, headers, signal, connect_timeout_ms, env).await?;
    let entry: CachedEntry = Arc::new(Mutex::new(CachedConnection {
        socket: None,
        busy: true,
        created_at: Instant::now(),
        idle_generation: 0,
        continuation: None,
    }));
    WEBSOCKET_SESSION_CACHE
        .lock()
        .entry(session_id.to_string())
        .or_default()
        .insert(account_id.to_string(), entry.clone());
    Ok(AcquiredSocket {
        socket,
        entry: Some(entry.clone()),
        reused: false,
        release: Release::Cached {
            session_id: session_id.to_string(),
            account_id: account_id.to_string(),
            entry,
        },
    })
}

fn request_body_without_input(body: &Value) -> String {
    let mut map = body.as_object().cloned().unwrap_or_default();
    map.shift_remove("input");
    map.shift_remove("previous_response_id");
    Value::Object(map).to_string()
}

fn input_of(body: &Value) -> Vec<Value> {
    body.get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn get_cached_websocket_input_delta(
    body: &Value,
    continuation: &ContinuationState,
) -> Option<Vec<Value>> {
    if request_body_without_input(body)
        != request_body_without_input(&continuation.last_request_body)
    {
        return None;
    }
    let current = input_of(body);
    let mut baseline = input_of(&continuation.last_request_body);
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current.len() < baseline.len() {
        return None;
    }
    let prefix = serde_json::to_string(&current[..baseline.len()]).unwrap_or_default();
    if prefix != serde_json::to_string(&baseline).unwrap_or_default() {
        return None;
    }
    Some(current[baseline.len()..].to_vec())
}

#[cfg(not(target_arch = "wasm32"))]
fn build_cached_websocket_request_body(entry: &CachedEntry, body: &Value) -> Value {
    let mut guard = entry.lock();
    let Some(continuation) = guard.continuation.clone() else {
        return body.clone();
    };
    let delta = get_cached_websocket_input_delta(body, &continuation);
    match delta {
        Some(delta) if !continuation.last_response_id.is_empty() => {
            let mut map = body.as_object().cloned().unwrap_or_default();
            map.insert(
                "previous_response_id".into(),
                json!(continuation.last_response_id),
            );
            map.insert("input".into(), Value::Array(delta));
            Value::Object(map)
        }
        _ => {
            guard.continuation = None;
            body.clone()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Read one parsed message from the socket, honoring abort and idle timeout.
enum SocketRead {
    Event(Value),
    Closed(Option<CloseFrame>),
}

#[cfg(not(target_arch = "wasm32"))]
async fn read_websocket_event(
    socket: &mut WsStream,
    signal: Option<&CancellationToken>,
    idle_timeout_ms: Option<u64>,
) -> anyhow::Result<SocketRead> {
    loop {
        let next = socket.next();
        let timeout = async {
            match idle_timeout_ms.filter(|t| *t > 0) {
                Some(ms) => crate::utils::wasm::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending::<()>().await,
            }
        };
        let cancelled = async {
            match signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let message = tokio::select! {
            message = next => message,
            _ = timeout => {
                let ms = idle_timeout_ms.unwrap_or_default();
                return Err(anyhow::anyhow!("WebSocket idle timeout after {ms}ms"));
            }
            _ = cancelled => return Err(aborted_error()),
        };
        let text = match message {
            None => return Ok(SocketRead::Closed(None)),
            Some(Err(error)) => return Err(anyhow::anyhow!(websocket_error_message(&error))),
            Some(Ok(WsMessage::Close(frame))) => return Ok(SocketRead::Closed(frame)),
            Some(Ok(WsMessage::Text(text))) => text.to_string(),
            Some(Ok(WsMessage::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
            Some(Ok(_)) => continue,
        };
        if text.is_empty() {
            continue;
        }
        return match crate::utils::js_json::js_json_parse(&text) {
            Ok(value) => Ok(SocketRead::Event(value)),
            Err(cause) => Err(CodexProtocolError {
                message: format!("Invalid Codex WebSocket JSON: {cause}"),
                payload: Some(text),
            }
            .into()),
        };
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn process_websocket_stream(
    url: &str,
    body: &Value,
    headers: &HeaderList,
    output: &mut AssistantMessage,
    sender: &AssistantMessageEventSender,
    model: &Model,
    on_start: &mut (dyn FnMut(&AssistantMessage) + Send),
    idle_timeout_ms: Option<u64>,
    connect_timeout_ms: u64,
    cache_session_id: Option<&str>,
    account_id: &str,
    grammar: &HashMap<String, String>,
    options: &StreamOptions,
) -> anyhow::Result<()> {
    let signal = options.signal.as_ref();
    let mut acquired = acquire_websocket(
        url,
        headers,
        cache_session_id,
        account_id,
        signal,
        connect_timeout_ms,
        options.env.as_ref(),
    )
    .await?;
    let transport = options.transport.unwrap_or(Transport::Auto);
    let use_cached_context = matches!(transport, Transport::WebsocketCached | Transport::Auto);
    let request_body = match (&acquired.entry, use_cached_context) {
        (Some(entry), true) => build_cached_websocket_request_body(entry, body),
        _ => body.clone(),
    };
    if let Some(session_id) = cache_session_id {
        let reused = acquired.reused;
        with_debug_stats(session_id, |stats| {
            stats.requests += 1;
            if reused {
                stats.connections_reused += 1;
            } else {
                stats.connections_created += 1;
            }
            if use_cached_context {
                stats.cached_context_requests += 1;
            }
            if request_body.get("store") == Some(&json!(true)) {
                stats.store_true_requests += 1;
            }
            let items = request_body
                .get("input")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            stats.last_input_items = items;
            match request_body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(previous) => {
                    stats.delta_requests += 1;
                    stats.last_delta_input_items = Some(items);
                    stats.last_previous_response_id = Some(previous.to_string());
                }
                None => {
                    stats.full_context_requests += 1;
                    stats.last_delta_input_items = None;
                    stats.last_previous_response_id = None;
                }
            }
        });
    }

    let pricing_model = model.clone();
    let apply = move |usage: &mut Usage, tier: Option<&str>| {
        apply_service_tier_multiplier(
            usage,
            get_service_tier_cost_multiplier(&pricing_model, tier),
        );
    };
    let result: anyhow::Result<()> = async {
        let mut frame = Map::new();
        frame.insert("type".into(), json!("response.create"));
        if let Value::Object(map) = &request_body {
            for (k, v) in map {
                frame.insert(k.clone(), v.clone());
            }
        }
        acquired
            .socket
            .send(WsMessage::text(Value::Object(frame).to_string()))
            .await
            .map_err(|e| anyhow::anyhow!(websocket_error_message(&e)))?;
        let mut pipeline = CodexEventPipeline::new(model, options, grammar, &apply);
        let mut started = false;
        loop {
            if signal.is_some_and(|s| s.is_cancelled()) {
                return Err(aborted_error());
            }
            match read_websocket_event(&mut acquired.socket, signal, idle_timeout_ms).await? {
                SocketRead::Event(event) => {
                    let mapped = pipeline.map(event, output).await?;
                    if matches!(mapped, MappedEvent::Skip) {
                        continue;
                    }
                    if !started {
                        started = true;
                        on_start(output);
                    }
                    if pipeline.process(mapped, output, sender)? {
                        break;
                    }
                }
                SocketRead::Closed(frame) => return Err(close_error(frame.as_ref()).into()),
            }
        }
        pipeline.finish(output)
    }
    .await;

    match result {
        Ok(()) => {
            let mut keep = true;
            if signal.is_some_and(|s| s.is_cancelled()) {
                keep = false;
            } else if let (true, Some(entry), Some(response_id)) = (
                use_cached_context,
                &acquired.entry,
                output.response_id.clone(),
            ) {
                let replay = transcript_from_messages(vec![Message::Assistant(output.clone())]);
                let items = convert_responses_messages(
                    model,
                    &replay,
                    &CODEX_TOOL_CALL_PROVIDERS,
                    &ConvertResponsesMessagesOptions {
                        include_system_prompt: Some(false),
                        grammar_tool_input_properties: Some(grammar),
                        ..Default::default()
                    },
                )?
                .into_iter()
                .filter(|item| {
                    !matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call_output" | "custom_tool_call_output")
                    )
                })
                .collect();
                entry.lock().continuation = Some(ContinuationState {
                    last_request_body: body.clone(),
                    last_response_id: response_id,
                    last_response_items: items,
                });
            }
            acquired.release(keep);
            Ok(())
        }
        Err(error) => {
            if let Some(entry) = &acquired.entry {
                entry.lock().continuation = None;
            }
            acquired.release(false);
            Err(error)
        }
    }
}

// ============================================================================
// Main stream function
// ============================================================================

async fn send_sse_request(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderList,
    body: &[u8],
    http_timeout_ms: Option<u64>,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<reqwest::Response> {
    let request = client
        .post(url)
        .headers(headers.to_header_map()?)
        .body(body.to_vec())
        .send();
    let timeout = async {
        match http_timeout_ms.filter(|t| *t > 0) {
            Some(ms) => crate::utils::wasm::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending::<()>().await,
        }
    };
    let cancelled = async {
        match signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        response = request => Ok(response?),
        _ = timeout => Err(anyhow::anyhow!("Codex SSE response headers timed out after {}ms", http_timeout_ms.unwrap_or_default())),
        _ = cancelled => Err(aborted_error()),
    }
}

async fn run_stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    Lazy::force(&CLEANUP_REGISTERED);
    let compat = model.compat();
    let normalized = resolve_transcript(&context, compat.supports_mid_convo_system_messages);
    let mut output = AssistantMessage {
        api: "openai-codex-responses".into(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let signal = options.signal.clone();
    let aborted = || signal.as_ref().is_some_and(|s| s.is_cancelled());

    let result: anyhow::Result<()> = async {
        let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
            anyhow::bail!("No API key for provider: {}", model.provider);
        };
        let account_id = extract_account_id(&api_key)?;
        let grammar = create_grammar_tool_input_properties(
            &get_declared_tools(&normalized.messages),
            compat.supports_openai_grammar_tools.unwrap_or(false),
        )?;
        let cache_session_id = if options.cache_retention == Some(CacheRetention::None) {
            None
        } else {
            options.session_id.clone()
        };
        let codex_session_id = clamp_openai_prompt_cache_key(cache_session_id.as_deref());
        let mut body = build_request_body(
            &model,
            &normalized,
            &options,
            codex_session_id.as_deref(),
            &grammar,
        )?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(body.clone(), model.clone()).await
        {
            body = next;
        }
        let websocket_request_id = codex_session_id.clone().unwrap_or_else(uuidv7);
        let target = ClientIdentityTarget {
            provider: &model.provider,
            base_url: &model.base_url,
        };
        let mut identity_headers: Vec<(String, String)> = vec![("originator".into(), "pi".into())];
        for (name, value) in get_identity_headers(target, options.identity.as_ref()) {
            identity_headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
            identity_headers.push((name, value));
        }
        let mut sse_headers = build_sse_headers(
            &identity_headers,
            model.headers.as_ref(),
            options.headers.as_ref(),
            &account_id,
            &api_key,
            codex_session_id.as_deref(),
        );
        let websocket_headers = build_websocket_headers(
            &identity_headers,
            model.headers.as_ref(),
            options.headers.as_ref(),
            &account_id,
            &api_key,
            &websocket_request_id,
        );
        let body_json = body.to_string();
        let http_timeout_ms = options.timeout_ms;
        let websocket_connect_timeout_ms = options
            .websocket_connect_timeout_ms
            .unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);
        let transport = options.transport.unwrap_or(Transport::Auto);
        let mut start_emitted = false;
        let websocket_disabled = transport != Transport::Sse
            && is_websocket_sse_fallback_active(cache_session_id.as_deref());
        if websocket_disabled {
            record_websocket_sse_fallback(cache_session_id.as_deref());
        }

#[cfg(not(target_arch = "wasm32"))]
        if transport != Transport::Sse && !websocket_disabled {
            let mut retried_connection_limit = false;
            let mut retried_missing_continuation = false;
            let ws_url = resolve_codex_websocket_url(Some(&model.base_url));
            loop {
                let mut websocket_started = false;
                let attempt = {
                    let sender_ref = &sender;
                    let started_flag = &mut websocket_started;
                    let emitted_flag = &mut start_emitted;
                    let mut on_start = |partial: &AssistantMessage| {
                        *started_flag = true;
                        if !*emitted_flag {
                            *emitted_flag = true;
                            sender_ref.push(AssistantMessageEvent::Start {
                                partial: partial.clone(),
                            });
                        }
                    };
                    process_websocket_stream(
                        &ws_url,
                        &body,
                        &websocket_headers,
                        &mut output,
                        &sender,
                        &model,
                        &mut on_start,
                        http_timeout_ms,
                        websocket_connect_timeout_ms,
                        cache_session_id.as_deref(),
                        &account_id,
                        &grammar,
                        &options,
                    )
                    .await
                };
                match attempt {
                    Ok(()) => {
                        if aborted() {
                            return Err(aborted_error());
                        }
                        assert_successful_output(&output)?;
                        sender.push(AssistantMessageEvent::Done {
                            reason: output.stop_reason,
                            message: output.clone(),
                        });
                        sender.end(None);
                        return Ok(());
                    }
                    Err(error) => {
                        let is_aborted = aborted();
                        let code = codex_error_code(&error);
                        let connection_limit_before_start = !websocket_started
                            && code == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE);
                        let previous_response_not_found =
                            code == Some(PREVIOUS_RESPONSE_NOT_FOUND_CODE);
                        if !is_aborted
                            && previous_response_not_found
                            && !retried_missing_continuation
                        {
                            retried_missing_continuation = true;
                            continue;
                        }
                        if !is_aborted && connection_limit_before_start && !retried_connection_limit
                        {
                            retried_connection_limit = true;
                            continue;
                        }
                        if is_aborted
                            || (is_codex_non_transport_error(&error)
                                && !connection_limit_before_start)
                        {
                            return Err(error);
                        }
                        let mut details = Map::new();
                        details.insert(
                            "configuredTransport".into(),
                            serde_json::to_value(transport)?,
                        );
                        if !websocket_started {
                            details.insert("fallbackTransport".into(), json!("sse"));
                        }
                        details.insert("eventsEmitted".into(), json!(websocket_started));
                        details.insert(
                            "phase".into(),
                            json!(if websocket_started {
                                "after_message_stream_start"
                            } else {
                                "before_message_stream_start"
                            }),
                        );
                        details.insert("requestBytes".into(), json!(body_json.len()));
                        append_assistant_message_diagnostic(
                            &mut output.diagnostics,
                            create_assistant_message_diagnostic(
                                "provider_transport_failure",
                                diagnostic_error_from_message(Some("Error"), error.to_string()),
                                Some(details),
                            ),
                        );
                        record_websocket_failure(cache_session_id.as_deref(), &error);
                        if websocket_started {
                            return Err(error);
                        }
                        record_websocket_sse_fallback(cache_session_id.as_deref());
                        break;
                    }
                }
            }
        }

        // The Codex backend decodes `Content-Encoding: zstd` on the SSE endpoint.
        let sse_body = compress_request_body_zstd(&body_json);
        sse_headers.set("content-encoding", Some("zstd"));
        let url = resolve_codex_url(Some(&model.base_url));
        let client = match &options.http_client {
            Some(client) => client.clone(),
            None => client_for(&url, options.env.as_ref())?,
        };
        let max_retries = options.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
        let mut response: Option<reqwest::Response> = None;
        let mut last_error: Option<anyhow::Error> = None;
        let mut attempt: u32 = 0;
        while attempt <= max_retries {
            if aborted() {
                return Err(aborted_error());
            }
            let outcome: anyhow::Result<Option<reqwest::Response>> = async {
                let resp = send_sse_request(
                    &client,
                    &url,
                    &sse_headers,
                    &sse_body,
                    http_timeout_ms,
                    signal.as_ref(),
                )
                .await?;
                if let Some(on_response) = &options.on_response {
                    on_response(
                        ProviderResponse {
                            status: resp.status().as_u16(),
                            headers: headers_to_record(resp.headers()),
                        },
                        model.clone(),
                    )
                    .await;
                }
                if resp.status().is_success() {
                    return Ok(Some(resp));
                }
                let status = resp.status();
                let resp_headers = resp.headers().clone();
                let error_text = resp.text().await.unwrap_or_default();
                if attempt < max_retries && is_retryable_error(status.as_u16(), &error_text) {
                    let delay = match get_retry_after_delay_ms(&resp_headers) {
                        None => (BASE_DELAY_MS * 2u64.pow(attempt)) as f64,
                        Some(delay) => validate_retry_delay_ms(delay, &options)?,
                    };
                    sleep(delay, signal.as_ref()).await?;
                    return Ok(None);
                }
                let info = parse_error_response(
                    status.as_u16(),
                    status.canonical_reason().unwrap_or_default(),
                    &error_text,
                );
                anyhow::bail!("{}", info.friendly_message.unwrap_or(info.message))
            }
            .await;
            match outcome {
                Ok(Some(resp)) => {
                    response = Some(resp);
                    break;
                }
                Ok(None) => {
                    attempt += 1;
                    continue;
                }
                Err(error) => {
                    if error.to_string() == "Request was aborted" || aborted() {
                        return Err(aborted_error());
                    }
                    let retryable = attempt < max_retries
                        && error.downcast_ref::<RetryDelayExceededError>().is_none()
                        && !error.to_string().contains("usage limit");
                    if retryable {
                        last_error = Some(error);
                        sleep((BASE_DELAY_MS * 2u64.pow(attempt)) as f64, signal.as_ref()).await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        let Some(response) = response else {
            return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Failed after retries")));
        };

        if !start_emitted {
            sender.push(AssistantMessageEvent::Start {
                partial: output.clone(),
            });
        }
        let pricing_model = model.clone();
        let apply = move |usage: &mut Usage, tier: Option<&str>| {
            apply_service_tier_multiplier(
                usage,
                get_service_tier_cost_multiplier(&pricing_model, tier),
            );
        };
        let mut pipeline = CodexEventPipeline::new(&model, &options, &grammar, &apply);
        process_sse_stream(
            response,
            &mut output,
            &sender,
            &mut pipeline,
            signal.as_ref(),
        )
        .await?;
        if aborted() {
            return Err(aborted_error());
        }
        assert_successful_output(&output)?;
        sender.push(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output.clone(),
        });
        sender.end(None);
        Ok(())
    }
    .await;

    if let Err(error) = result {
        output.stop_reason = if aborted() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(error_message_of(&error, None));
        sender.push(AssistantMessageEvent::Error {
            reason: output.stop_reason,
            error: output.clone(),
        });
        sender.end(None);
    }
}

/// Stream an OpenAI Codex response.
pub fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
        return super::lazy::error_stream(
            model,
            anyhow::anyhow!("No API key for provider: {}", model.provider),
        );
    };
    let mut base = build_base_options(model, context, &options, Some(&api_key));
    if let Some(choice) = options.tool_choice {
        base.extra.insert(
            "toolChoice".into(),
            serde_json::to_value(choice).unwrap_or(Value::Null),
        );
    }
    if let Some(effort) = simple_reasoning_effort(model, &options) {
        base.extra.insert("reasoningEffort".into(), json!(effort));
    }
    stream(model, context, base)
}

/// The `openai-codex-responses` API implementation.
pub struct OpenAICodexResponsesApi;

impl ProviderStreams for OpenAICodexResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_urls() {
        assert_eq!(
            resolve_codex_url(None),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://x.test/codex/")),
            "https://x.test/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://x.test/codex/responses")),
            "https://x.test/codex/responses"
        );
        assert_eq!(
            resolve_codex_websocket_url(Some("http://127.0.0.1:9")),
            "ws://127.0.0.1:9/codex/responses"
        );
        assert_eq!(
            resolve_codex_websocket_url(None),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn extracts_account_ids() {
        let payload = base64::engine::general_purpose::STANDARD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_test"}}"#);
        assert_eq!(
            extract_account_id(&format!("aaa.{payload}.bbb")).unwrap(),
            "acc_test"
        );
        let unpadded = payload.trim_end_matches('=');
        assert_eq!(
            extract_account_id(&format!("aaa.{unpadded}.bbb")).unwrap(),
            "acc_test"
        );
        assert_eq!(
            extract_account_id("nope").unwrap_err().to_string(),
            "Failed to extract accountId from token"
        );
    }

    #[test]
    fn retryable_errors() {
        assert!(is_retryable_error(429, "slow down"));
        assert!(!is_retryable_error(429, "insufficient_quota"));
        assert!(is_retryable_error(400, "upstream connect error"));
        assert!(!is_retryable_error(400, "bad request"));
    }

    #[test]
    fn friendly_usage_limit_messages() {
        let info = parse_error_response(
            429,
            "Too Many Requests",
            r#"{"error":{"code":"usage_limit_reached","plan_type":"PLUS","message":"Limit"}}"#,
        );
        assert_eq!(
            info.friendly_message.as_deref(),
            Some("You have hit your ChatGPT usage limit (plus plan).")
        );
        assert_eq!(info.message, "Limit");
        let plain = parse_error_response(500, "Internal Server Error", "");
        assert_eq!(plain.message, "Internal Server Error");
        assert!(plain.friendly_message.is_none());
    }

    #[test]
    fn codex_sse_decoder_handles_eof_without_blank_line() {
        let mut decoder = CodexSseDecoder::default();
        let first = decoder
            .push(b"data: {\"type\":\"a\"}\n\ndata: {\"type\":", false)
            .unwrap();
        assert_eq!(first.len(), 1);
        let rest = decoder.push(b"\"b\"}", true).unwrap();
        assert_eq!(rest[0]["type"], "b");
        let mut bad = CodexSseDecoder::default();
        let error = bad.push(b"data: {oops\n\n", false).unwrap_err();
        assert!(error.downcast_ref::<CodexProtocolError>().is_some());
    }

    #[test]
    fn zstd_round_trip() {
        let compressed = compress_request_body_zstd("{\"a\":1}");
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut out = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut out).unwrap();
        assert_eq!(out, "{\"a\":1}");
    }

    #[test]
    fn continuation_delta() {
        let continuation = ContinuationState {
            last_request_body: json!({ "model": "m", "input": [{ "role": "user" }] }),
            last_response_id: "resp_1".into(),
            last_response_items: vec![json!({ "type": "message" })],
        };
        let body = json!({ "model": "m", "input": [{ "role": "user" }, { "type": "message" }, { "role": "user", "n": 2 }] });
        assert_eq!(
            get_cached_websocket_input_delta(&body, &continuation),
            Some(vec![json!({ "role": "user", "n": 2 })])
        );
        let changed = json!({ "model": "other", "input": [] });
        assert_eq!(
            get_cached_websocket_input_delta(&changed, &continuation),
            None
        );
    }
}
