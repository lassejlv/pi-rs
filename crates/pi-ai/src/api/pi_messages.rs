//! pi-messages API implementation (port of `api/pi-messages.ts`).
//!
//! Streams pi's own message protocol directly to a backend: the request is a single POST of
//! `{ model, context, options }` to `<baseUrl>/messages`, the response is an SSE stream of
//! serialized assistant-message events plus a terminal `done`/`error` event. This is the wire
//! protocol spoken by the Radius gateway, but any backend implementing it can be used.
//!
//! API-specific options are read from `StreamOptions.extra`: `toolChoice` (any JSON value) and
//! `debug` (bool).

use std::collections::HashMap;

use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::models::ProviderStreams;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, JsonObject, Model,
    ProviderEnv, ProviderHeaders, ProviderResponse, StopReason, StreamOptions, TextContent,
    ThinkingContent, ToolCall, TranscriptContext, Usage,
};
use crate::utils::diagnostics::{
    AssistantMessageDiagnostic, DiagnosticErrorInfo, append_assistant_message_diagnostic,
    create_assistant_message_diagnostic,
};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::headers::{headers_to_record, provider_headers_to_record, to_provider_headers};
use crate::utils::http::client_for;
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::json_parse::parse_streaming_json_object;
use crate::utils::provider_env::get_provider_env_value;

/// Error for a non-2xx pi-messages response.
#[derive(Debug, Clone)]
pub struct PiMessagesResponseError {
    pub message: String,
    pub code: Option<String>,
    pub diagnostic_details: JsonObject,
}

impl std::fmt::Display for PiMessagesResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PiMessagesResponseError {}

/// Error raised when the request is aborted (`DOMException` `AbortError`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("This operation was aborted")]
struct FetchAborted;

fn parse_pi_messages_error_body(body: &str) -> Option<Map<String, Value>> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    match parsed {
        Value::Object(map) if matches!(map.get("error"), Some(Value::Object(_))) => Some(map),
        _ => None,
    }
}

fn truncate_diagnostic_string(value: &str) -> String {
    const MAX: usize = 8192;
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() > MAX {
        format!("{}…", String::from_utf16_lossy(&units[..MAX]))
    } else {
        value.to_string()
    }
}

fn status_text(status: reqwest::StatusCode) -> String {
    status.canonical_reason().unwrap_or_default().to_string()
}

fn create_pi_messages_response_error(
    model: &Model,
    url: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> PiMessagesResponseError {
    let error_body = parse_pi_messages_error_body(body);
    let error = error_body.as_ref().and_then(|b| b.get("error"));
    let message = error.and_then(|e| e.get("message")).and_then(Value::as_str);
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let suffix = message.unwrap_or(body);
    let code_suffix = code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default();
    let text = format!(
        "{} {}: {suffix}{code_suffix}",
        status.as_u16(),
        status_text(status)
    );
    let mut details = Map::new();
    details.insert("version".into(), json!(1));
    details.insert("provider".into(), json!(model.provider));
    details.insert("model".into(), json!(model.id));
    details.insert("url".into(), json!(url));
    details.insert("status".into(), json!(status.as_u16()));
    details.insert("statusText".into(), json!(status_text(status)));
    if let Some(error) = error {
        details.insert("error".into(), error.clone());
    }
    if error_body.is_none() {
        details.insert("body".into(), json!(truncate_diagnostic_string(body)));
    }
    details.insert("timestampMs".into(), json!(crate::utils::now_ms()));
    PiMessagesResponseError {
        message: text,
        code,
        diagnostic_details: details,
    }
}

fn append_rewrite_diagnostic(message: &mut AssistantMessage, rewrite: Option<&Value>) {
    let Some(Value::Object(rewrite)) = rewrite else {
        return;
    };
    append_assistant_message_diagnostic(
        &mut message.diagnostics,
        AssistantMessageDiagnostic {
            diagnostic_type: "pi_messages_rewrite".into(),
            timestamp: crate::utils::now_ms(),
            error: None,
            details: Some(rewrite.clone()),
        },
    );
}

/// Converts serialized pi-messages events into assistant message events.
struct EventConverter {
    partial: AssistantMessage,
    tool_json: HashMap<usize, String>,
}

fn index_of(event: &Value) -> usize {
    event
        .get("contentIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

fn str_field(event: &Value, key: &str) -> String {
    event
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_str(event: &Value, key: &str) -> Option<String> {
    event.get(key).and_then(Value::as_str).map(str::to_string)
}

impl EventConverter {
    fn new(model: &Model) -> Self {
        Self {
            partial: AssistantMessage {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                stop_reason: StopReason::Pending,
                timestamp: crate::utils::now_ms(),
                ..Default::default()
            },
            tool_json: HashMap::new(),
        }
    }

    fn set_block(&mut self, index: usize, block: AssistantContent) {
        if index < self.partial.content.len() {
            self.partial.content[index] = block;
        } else {
            self.partial.content.push(block);
        }
    }

    fn apply_terminal(&mut self, event: &Value) -> StopReason {
        let reason: StopReason = event
            .get("reason")
            .cloned()
            .and_then(|r| serde_json::from_value(r).ok())
            .unwrap_or(StopReason::Error);
        self.partial.stop_reason = reason;
        self.partial.usage = event
            .get("usage")
            .cloned()
            .and_then(|u| serde_json::from_value::<Usage>(u).ok())
            .unwrap_or_default();
        self.partial.response_id = opt_str(event, "responseId");
        if let Some(level) = opt_str(event, "providerThinkingLevel") {
            self.partial.provider_thinking_level = Some(level);
        }
        reason
    }

    fn convert(&mut self, event: &Value) -> Option<AssistantMessageEvent> {
        let index = index_of(event);
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "done" => {
                let reason = self.apply_terminal(event);
                append_rewrite_diagnostic(&mut self.partial, event.get("rewrite"));
                Some(AssistantMessageEvent::Done {
                    reason,
                    message: self.partial.clone(),
                })
            }
            "error" => {
                let reason = self.apply_terminal(event);
                self.partial.error_message = opt_str(event, "errorMessage");
                append_rewrite_diagnostic(&mut self.partial, event.get("rewrite"));
                Some(AssistantMessageEvent::Error {
                    reason,
                    error: self.partial.clone(),
                })
            }
            "start" => Some(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            }),
            "text_start" => {
                self.set_block(index, AssistantContent::Text(TextContent::new("")));
                Some(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: self.partial.clone(),
                })
            }
            "text_delta" => {
                let delta = str_field(event, "delta");
                if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                    block.text.push_str(&delta);
                }
                Some(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                })
            }
            "text_end" => {
                let content = str_field(event, "content");
                if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                    block.text = content.clone();
                    block.text_signature = opt_str(event, "contentSignature");
                }
                Some(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.partial.clone(),
                })
            }
            "thinking_start" => {
                self.set_block(index, AssistantContent::Thinking(ThinkingContent::new("")));
                Some(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.partial.clone(),
                })
            }
            "thinking_delta" => {
                let delta = str_field(event, "delta");
                if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index)
                {
                    block.thinking.push_str(&delta);
                }
                Some(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                })
            }
            "thinking_end" => {
                let content = str_field(event, "content");
                if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index)
                {
                    block.thinking = content.clone();
                    block.thinking_signature = opt_str(event, "contentSignature");
                    block.redacted = event.get("redacted").and_then(Value::as_bool);
                }
                Some(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.partial.clone(),
                })
            }
            "toolcall_start" => {
                self.set_block(
                    index,
                    AssistantContent::ToolCall(ToolCall {
                        id: str_field(event, "id"),
                        name: str_field(event, "toolName"),
                        arguments: Map::new(),
                        ..Default::default()
                    }),
                );
                self.tool_json.insert(index, String::new());
                Some(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: self.partial.clone(),
                })
            }
            "toolcall_delta" => {
                let delta = str_field(event, "delta");
                let json = self.tool_json.entry(index).or_default();
                json.push_str(&delta);
                let arguments = parse_streaming_json_object(Some(json));
                if let Some(AssistantContent::ToolCall(block)) = self.partial.content.get_mut(index)
                {
                    block.arguments = arguments;
                }
                Some(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                })
            }
            "toolcall_end" => {
                if let (Some(AssistantContent::ToolCall(block)), Some(Value::Object(update))) =
                    (self.partial.content.get_mut(index), event.get("toolCall"))
                {
                    let mut merged = serde_json::to_value(&*block)
                        .ok()
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default();
                    for (key, value) in update {
                        merged.insert(key.clone(), value.clone());
                    }
                    if let Ok(call) = serde_json::from_value::<ToolCall>(Value::Object(merged)) {
                        *block = call;
                    }
                }
                self.tool_json.remove(&index);
                let tool_call = match self.partial.content.get(index) {
                    Some(AssistantContent::ToolCall(call)) => call.clone(),
                    _ => ToolCall::default(),
                };
                Some(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call,
                    partial: self.partial.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Parse one raw SSE frame: the first `data:` line, ignoring `[DONE]`.
fn parse_pi_messages_event(raw: &str) -> anyhow::Result<Option<Value>> {
    let data = raw
        .split('\n')
        .find(|line| line.starts_with("data:"))
        .map(|line| line[5..].trim());
    match data {
        Some(data) if !data.is_empty() && data != "[DONE]" => {
            Ok(Some(crate::utils::js_json::js_json_parse_as(data).map_err(anyhow::Error::msg)?))
        }
        _ => Ok(None),
    }
}

/// Splits a byte stream into pi-messages SSE frames.
#[derive(Default)]
struct FrameDecoder {
    buffer: String,
    pending: Vec<u8>,
}

impl FrameDecoder {
    fn push(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<Value>> {
        self.pending.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                return self.push_text(&text);
            }
        };
        let rest = self.pending.split_off(valid);
        let text =
            String::from_utf8(std::mem::replace(&mut self.pending, rest)).unwrap_or_default();
        self.push_text(&text)
    }

    fn push_text(&mut self, text: &str) -> anyhow::Result<Vec<Value>> {
        self.buffer.push_str(text);
        self.buffer = self.buffer.replace("\r\n", "\n");
        let mut events = Vec::new();
        while let Some(split) = self.buffer.find("\n\n") {
            let frame = self.buffer[..split].to_string();
            self.buffer = self.buffer[split + 2..].to_string();
            if let Some(event) = parse_pi_messages_event(&frame)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> anyhow::Result<Vec<Value>> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let text = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            events.extend(self.push_text(&text)?);
        }
        if !self.buffer.trim().is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            if let Some(event) = parse_pi_messages_event(&frame)? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn create_error_event(
    model: &Model,
    error: &anyhow::Error,
    aborted: bool,
) -> AssistantMessageEvent {
    let reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    let mut message = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: reason,
        error_message: Some(error.to_string()),
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    if !aborted && let Some(response_error) = error.downcast_ref::<PiMessagesResponseError>() {
        append_assistant_message_diagnostic(
            &mut message.diagnostics,
            create_assistant_message_diagnostic(
                "pi_messages_response_failure",
                DiagnosticErrorInfo {
                    name: Some("PiMessagesResponseError".into()),
                    message: response_error.message.clone(),
                    stack: None,
                    code: response_error
                        .code
                        .clone()
                        .map(crate::utils::diagnostics::DiagnosticErrorCode::String),
                },
                Some(response_error.diagnostic_details.clone()),
            ),
        );
    }
    AssistantMessageEvent::Error {
        reason,
        error: message,
    }
}

fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> Option<CacheRetention> {
    if cache_retention.is_some() {
        return cache_retention;
    }
    (get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long"))
        .then_some(CacheRetention::Long)
}

async fn with_signal<T>(
    signal: Option<&tokio_util::sync::CancellationToken>,
    future: impl std::future::Future<Output = T>,
) -> anyhow::Result<T> {
    match signal {
        Some(signal) => {
            if signal.is_cancelled() {
                return Err(FetchAborted.into());
            }
            tokio::select! {
                value = future => Ok(value),
                _ = signal.cancelled() => Err(FetchAborted.into()),
            }
        }
        None => Ok(future.await),
    }
}

async fn run_stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let mut converter = EventConverter::new(&model);
    let signal = options.signal.clone();
    let result: anyhow::Result<()> = async {
        let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
            anyhow::bail!("No API key provided for provider \"{}\"", model.provider);
        };
        let mut url = url::Url::parse(&format!(
            "{}/messages",
            model.base_url.trim_end_matches('/')
        ))?;
        if options.extra_bool("debug") == Some(true) {
            url.query_pairs_mut().append_pair("debug", "1");
        }
        let url = url.to_string();

        let mut request_options = Map::new();
        if let Some(temperature) = options.temperature {
            request_options.insert(
                "temperature".into(),
                crate::serde_util::js_number(temperature),
            );
        }
        if let Some(max_tokens) = options.max_tokens {
            request_options.insert("maxTokens".into(), json!(max_tokens));
        }
        if let Some(reasoning) = options.reasoning {
            request_options.insert("reasoning".into(), json!(reasoning.as_str()));
        }
        if let Some(retention) =
            resolve_cache_retention(options.cache_retention, options.env.as_ref())
        {
            request_options.insert("cacheRetention".into(), serde_json::to_value(retention)?);
        }
        if let Some(session_id) = &options.session_id {
            request_options.insert("sessionId".into(), json!(session_id));
        }
        let tool_choice = options
            .extra_value("toolChoice")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| {
                options
                    .tool_choice
                    .and_then(|c| serde_json::to_value(c).ok())
            });
        if let Some(choice) = tool_choice {
            request_options.insert("toolChoice".into(), choice);
        }
        let mut payload =
            json!({ "model": model.id, "context": context, "options": request_options });
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(payload.clone(), model.clone()).await
        {
            payload = next;
        }

        let target = ClientIdentityTarget {
            provider: &model.provider,
            base_url: &model.base_url,
        };
        let identity: ProviderHeaders =
            to_provider_headers(&get_identity_headers(target, options.identity.as_ref()));
        let mut defaults: ProviderHeaders = ProviderHeaders::new();
        defaults.insert("authorization".into(), Some(format!("Bearer {api_key}")));
        defaults.insert("accept".into(), Some("text/event-stream".into()));
        defaults.insert("content-type".into(), Some("application/json".into()));
        let headers = provider_headers_to_record(&[
            Some(&identity),
            Some(&defaults),
            options.headers.as_ref(),
        ])
        .unwrap_or_default();

        let client = match &options.http_client {
            Some(client) => client.clone(),
            None => client_for(&url, options.env.as_ref())?,
        };
        let mut request = client.post(&url).body(serde_json::to_vec(&payload)?);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = with_signal(signal.as_ref(), request.send()).await??;
        if let Some(on_response) = &options.on_response {
            on_response(
                ProviderResponse {
                    status: response.status().as_u16(),
                    headers: headers_to_record(response.headers()),
                },
                model.clone(),
            )
            .await;
        }
        let status = response.status();
        if !status.is_success() {
            let body = with_signal(signal.as_ref(), response.text())
                .await?
                .unwrap_or_default();
            return Err(create_pi_messages_response_error(&model, &url, status, &body).into());
        }

        let mut body = response.bytes_stream();
        let mut decoder = FrameDecoder::default();
        loop {
            let chunk = with_signal(signal.as_ref(), body.next()).await?;
            let ended = chunk.is_none();
            let events = match chunk {
                Some(Ok(bytes)) => decoder.push(&bytes)?,
                Some(Err(error)) => return Err(error.into()),
                None => decoder.finish()?,
            };
            for pi_event in events {
                if let Some(observer) = &options.on_provider_stream_event {
                    observer(pi_event.clone(), model.clone()).await;
                }
                let Some(event) = converter.convert(&pi_event) else {
                    continue;
                };
                let terminal = event.is_terminal();
                sender.push(event);
                if terminal {
                    sender.end(None);
                    return Ok(());
                }
            }
            if ended {
                break;
            }
        }
        anyhow::bail!("{} stream ended without a terminal event", model.provider)
    }
    .await;

    if let Err(error) = result {
        let aborted = signal.as_ref().is_some_and(|s| s.is_cancelled());
        sender.push(create_error_event(&model, &error, aborted));
        sender.end(None);
    }
}

/// Stream a pi-messages response.
pub fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Stream with provider-neutral options (`reasoning`, `toolChoice` and `debug` pass through).
pub fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    stream(model, context, options)
}

/// The `pi-messages` API implementation.
pub struct PiMessagesApi;

impl ProviderStreams for PiMessagesApi {
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
