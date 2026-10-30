//! Mistral chat completions over HTTP (port of `api/mistral-conversations.ts`).
//!
//! `POST {baseUrl}/v1/chat/completions` with `stream: true`, read as server-sent events.
//!
//! API-specific options are read from `StreamOptions.extra`: `toolChoice` (`auto`, `none`,
//! `any`, `required` or `{ type: "function", function: { name } }`), `promptMode`
//! (`reasoning`) and `reasoningEffort`. The `onPayload` hook receives the SDK-style camelCase
//! payload (`maxTokens`, `toolChoice`, ...), which is converted to the snake_case wire format
//! afterwards.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::constrained_sampling::{get_json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use super::simple_options::build_base_options;
use super::transform_messages::transform_messages;
use crate::models::{ProviderStreams, calculate_cost, clamp_thinking_level};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Message, Model, ModelInputModality,
    ModelThinkingLevel, ProviderHeaders, ProviderResponse, StopReason, StreamOptions, TextContent, ThinkingContent,
    Tool, ToolCall, TranscriptContext, UserContent, UserContentBlock,
};
use crate::utils::error_body::truncate_error_text;
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::hash::short_hash;
use crate::utils::headers::headers_to_record;
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::json_parse::{parse_streaming_json_object, parse_tool_call_arguments};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::text::{
    get_system_message_text, get_system_message_text_is_empty, render_system_message_update,
    render_system_message_update_is_empty,
};
use crate::utils::transcript::{get_current_tools, resolve_transcript};

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;
const DEFAULT_HEADER_TIMEOUT_MS: u64 = 60_000;

/// An HTTP error from the Mistral endpoint (`MistralHttpError`).
#[derive(Debug, Clone)]
pub struct MistralHttpError {
    pub status_code: u16,
    pub body: String,
    pub message: String,
}

impl std::fmt::Display for MistralHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MistralHttpError {}

/// Error marker for aborted requests.
#[derive(Debug, Clone, thiserror::Error)]
#[error("This operation was aborted")]
struct MistralAborted;

fn format_mistral_error(error: &anyhow::Error) -> String {
    if let Some(http) = error.downcast_ref::<MistralHttpError>() {
        let body = http.body.trim();
        if !body.is_empty() {
            return format!("Mistral API error ({}): {}", http.status_code, truncate_error_text(body, MAX_MISTRAL_ERROR_BODY_CHARS));
        }
        return format!("Mistral API error ({}): {}", http.status_code, http.message);
    }
    error.to_string()
}

/// Map arbitrary tool call ids to Mistral's 9-character alphanumeric ids, keeping the mapping
/// collision-free within one request.
#[derive(Default)]
pub struct MistralToolCallIdNormalizer {
    ids: HashMap<String, String>,
    owners: HashMap<String, String>,
}

impl MistralToolCallIdNormalizer {
    pub fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.ids.get(id) {
            return existing.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            match self.owners.get(&candidate) {
                Some(owner) if owner != id => attempt += 1,
                _ => {
                    self.ids.insert(id.to_string(), candidate.clone());
                    self.owners.insert(candidate.clone(), id.to_string());
                    return candidate;
                }
            }
        }
    }
}

/// Derive a 9-character alphanumeric tool call id.
pub fn derive_mistral_tool_call_id(id: &str, attempt: usize) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.chars().count() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() { id.to_string() } else { normalized };
    let seed = if attempt == 0 { seed_base } else { format!("{seed_base}:{attempt}") };
    short_hash(&seed).chars().filter(|c| c.is_ascii_alphanumeric()).take(MISTRAL_TOOL_CALL_ID_LENGTH).collect()
}

fn image_url(mime_type: &str, data: &str) -> String {
    format!("data:{mime_type};base64,{data}")
}

fn build_tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
    let trimmed = text.trim();
    let prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let suffix = if has_images && !supports_images { "\n[tool image omitted: model does not support images]" } else { "" };
        return format!("{prefix}{trimmed}{suffix}");
    }
    if has_images {
        if supports_images {
            return if is_error { "[tool error] (see attached image)".into() } else { "(see attached image)".into() };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".into()
        } else {
            "(image omitted: model does not support images)".into()
        };
    }
    if is_error { "[tool error] (no tool output)".into() } else { "(no tool output)".into() }
}

/// Convert transcript messages to SDK-style (camelCase) Mistral chat messages.
fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::System(system) => {
                let text = if index == 0 { get_system_message_text(system) } else { render_system_message_update(system) };
                let empty = if index == 0 { get_system_message_text_is_empty(system) } else { render_system_message_update_is_empty(system) };
                if !empty {
                    result.push(json!({ "role": "system", "content": sanitize_surrogates(&text) }));
                }
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) => result.push(json!({ "role": "user", "content": sanitize_surrogates(text) })),
                UserContent::Blocks(blocks) => {
                    let had_images = blocks.iter().any(|b| b.as_image().is_some());
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter(|b| b.as_text().is_some() || supports_images)
                        .map(|b| match b {
                            UserContentBlock::Text(t) => json!({ "type": "text", "text": sanitize_surrogates(&t.text) }),
                            UserContentBlock::Image(i) => json!({ "type": "image_url", "imageUrl": image_url(&i.mime_type, &i.data) }),
                        })
                        .collect();
                    if !content.is_empty() {
                        result.push(json!({ "role": "user", "content": content }));
                    } else if had_images && !supports_images {
                        result.push(json!({ "role": "user", "content": "(image omitted: model does not support images)" }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut parts = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) => {
                            if !t.is_blank() {
                                parts.push(json!({ "type": "text", "text": sanitize_surrogates(&t.text) }));
                            }
                        }
                        AssistantContent::Thinking(t) => {
                            if !t.is_blank() {
                                parts.push(json!({ "type": "thinking", "thinking": [{ "type": "text", "text": sanitize_surrogates(&t.thinking) }] }));
                            }
                        }
                        AssistantContent::ToolCall(call) => tool_calls.push(json!({
                            "id": call.id,
                            "type": "function",
                            "function": { "name": call.name, "arguments": Value::Object(call.arguments.clone()).to_string() },
                            "index": 0,
                        })),
                    }
                }
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                message.insert("prefix".into(), json!(false));
                let has_content = !parts.is_empty();
                let has_calls = !tool_calls.is_empty();
                if has_content {
                    message.insert("content".into(), Value::Array(parts));
                }
                if has_calls {
                    message.insert("toolCalls".into(), Value::Array(tool_calls));
                }
                if has_content || has_calls {
                    result.push(Value::Object(message));
                }
            }
            Message::ToolResult(tool) => {
                let text = tool
                    .content
                    .iter()
                    .filter_map(UserContentBlock::as_text)
                    .map(|t| sanitize_surrogates(&t.text))
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool.content.iter().any(|c| c.as_image().is_some());
                let mut content = vec![json!({ "type": "text", "text": build_tool_result_text(&text, has_images, supports_images, tool.is_error) })];
                if supports_images {
                    for image in tool.content.iter().filter_map(UserContentBlock::as_image) {
                        content.push(json!({ "type": "image_url", "imageUrl": image_url(&image.mime_type, &image.data) }));
                    }
                }
                result.push(json!({ "role": "tool", "toolCallId": tool.tool_call_id, "name": tool.tool_name, "content": content }));
            }
        }
    }
    result
}

fn to_function_tools(tools: &[Tool]) -> anyhow::Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, true, None)?;
            Ok(json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": get_json_schema_tool_parameters(tool, strict),
                    "strict": strict.unwrap_or(false),
                }
            }))
        })
        .collect()
}

fn map_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(s) if matches!(s.as_str(), "auto" | "none" | "any" | "required") => Some(choice.clone()),
        Value::Object(obj) => {
            let name = obj.get("function").and_then(|f| f.get("name")).cloned().unwrap_or(Value::Null);
            Some(json!({ "type": "function", "function": { "name": name } }))
        }
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        other => Some(other.clone()),
    }
}

fn should_use_prompt_caching(options: &StreamOptions) -> Option<&str> {
    if options.cache_retention == Some(CacheRetention::None) {
        return None;
    }
    options.session_id.as_deref().filter(|s| !s.is_empty())
}

fn build_chat_payload(model: &Model, context: &TranscriptContext, messages: &[Message], options: &StreamOptions) -> anyhow::Result<Value> {
    let mut payload = Map::new();
    payload.insert("model".into(), json!(model.id));
    payload.insert("stream".into(), json!(true));
    payload.insert("messages".into(), Value::Array(to_chat_messages(messages, model.input.contains(&ModelInputModality::Image))));
    let tools = get_current_tools(&context.messages);
    if !tools.is_empty() {
        payload.insert("tools".into(), Value::Array(to_function_tools(&tools)?));
    }
    if let Some(temperature) = options.temperature {
        payload.insert("temperature".into(), crate::serde_util::js_number(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        payload.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(choice) = mistral_tool_choice(options).as_ref().and_then(map_tool_choice) {
        payload.insert("toolChoice".into(), choice);
    }
    if let Some(mode) = options.extra_str("promptMode").filter(|m| !m.is_empty()) {
        payload.insert("promptMode".into(), json!(mode));
    }
    if let Some(effort) = options.extra_str("reasoningEffort").filter(|m| !m.is_empty()) {
        payload.insert("reasoningEffort".into(), json!(effort));
    }
    if let Some(session) = should_use_prompt_caching(options) {
        payload.insert("promptCacheKey".into(), json!(session));
    }
    Ok(Value::Object(payload))
}

fn mistral_tool_choice(options: &StreamOptions) -> Option<Value> {
    options
        .extra_value("toolChoice")
        .cloned()
        .or_else(|| options.tool_choice.and_then(|c| serde_json::to_value(c).ok()))
        .filter(|v| !v.is_null())
}

fn remap(record: &mut Map<String, Value>, source: &str, target: &str) {
    if let Some(value) = record.shift_remove(source) {
        record.insert(target.to_string(), value);
    }
}

fn to_wire_message(message: &Value) -> Value {
    let Value::Object(map) = message else { return message.clone() };
    let mut wire = map.clone();
    remap(&mut wire, "toolCalls", "tool_calls");
    remap(&mut wire, "toolCallId", "tool_call_id");
    if let Some(Value::Array(content)) = map.get("content") {
        let chunks = content
            .iter()
            .map(|chunk| {
                let Value::Object(chunk) = chunk else { return chunk.clone() };
                let mut chunk = chunk.clone();
                for (source, target) in [
                    ("imageUrl", "image_url"),
                    ("documentUrl", "document_url"),
                    ("documentName", "document_name"),
                    ("fileId", "file_id"),
                    ("referenceIds", "reference_ids"),
                    ("inputAudio", "input_audio"),
                ] {
                    remap(&mut chunk, source, target);
                }
                Value::Object(chunk)
            })
            .collect();
        wire.insert("content".into(), Value::Array(chunks));
    }
    Value::Object(wire)
}

/// Convert the SDK-style payload to the snake_case wire format (`toMistralWirePayload`).
pub fn to_mistral_wire_payload(payload: &Value) -> Value {
    let Value::Object(map) = payload else { return payload.clone() };
    let mut wire = map.clone();
    for (source, target) in [
        ("topP", "top_p"),
        ("maxTokens", "max_tokens"),
        ("randomSeed", "random_seed"),
        ("responseFormat", "response_format"),
        ("toolChoice", "tool_choice"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("reasoningEffort", "reasoning_effort"),
        ("promptMode", "prompt_mode"),
        ("promptCacheKey", "prompt_cache_key"),
        ("safePrompt", "safe_prompt"),
    ] {
        remap(&mut wire, source, target);
    }
    if let Some(Value::Array(messages)) = map.get("messages") {
        wire.insert("messages".into(), Value::Array(messages.iter().map(to_wire_message).collect()));
    }
    if let Some(Value::Object(format)) = wire.get("response_format").cloned() {
        let mut format = format;
        remap(&mut format, "jsonSchema", "json_schema");
        if let Some(Value::Object(schema)) = format.get("json_schema").cloned() {
            let mut schema = schema;
            remap(&mut schema, "schemaDefinition", "schema");
            format.insert("json_schema".into(), Value::Object(schema));
        }
        wire.insert("response_format".into(), Value::Object(format));
    }
    Value::Object(wire)
}

fn build_headers(model: &Model, api_key: &str, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers: IndexMap<String, (String, String)> = IndexMap::new();
    let set = |headers: &mut IndexMap<String, (String, String)>, name: &str, value: Option<&str>| {
        let key = name.to_lowercase();
        match value {
            Some(value) => {
                headers.insert(key, (name.to_lowercase(), value.to_string()));
            }
            None => {
                headers.shift_remove(&key);
            }
        }
    };
    for (name, value) in get_identity_headers(
        ClientIdentityTarget { provider: &model.provider, base_url: &model.base_url },
        options.identity.as_ref(),
    ) {
        set(&mut headers, &name, Some(&value));
    }
    set(&mut headers, "accept", Some("text/event-stream"));
    set(&mut headers, "authorization", Some(&format!("Bearer {api_key}")));
    set(&mut headers, "content-type", Some("application/json"));
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            set(&mut headers, name, Some(value));
        }
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            set(&mut headers, name, value.as_deref());
        }
    }
    let explicit_affinity = model.headers.as_ref().is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("x-affinity")))
        || options.headers.as_ref().is_some_and(|h: &ProviderHeaders| h.keys().any(|k| k.eq_ignore_ascii_case("x-affinity")));
    if let Some(session) = should_use_prompt_caching(options)
        && !explicit_affinity
    {
        set(&mut headers, "x-affinity", Some(session));
    }
    headers.into_values().collect()
}

/// The chat completions URL: `new URL("v1/chat/completions", baseUrl + "/")`.
fn chat_url(base_url: &str) -> anyhow::Result<String> {
    let mut base = url::Url::parse(base_url)?;
    let path = format!("{}/", base.path().trim_end_matches('/'));
    base.set_path(&path);
    Ok(base.join("v1/chat/completions")?.to_string())
}

type ByteStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

enum MistralEvent {
    Data(Value),
    Done,
}

struct MistralEventReader {
    body: ByteStream,
    buffer: String,
    pending: Vec<u8>,
    signal: Option<CancellationToken>,
    finished: bool,
}

fn find_boundary(buffer: &str) -> Option<(usize, usize)> {
    let bytes = buffer.as_bytes();
    let patterns: [&[u8]; 8] = [b"\r\n\r\n", b"\r\n\r", b"\r\n\n", b"\r\r\n", b"\n\r\n", b"\r\r", b"\n\r", b"\n\n"];
    for i in 0..bytes.len() {
        for pattern in patterns {
            if bytes[i..].starts_with(pattern) {
                return Some((i, pattern.len()));
            }
        }
    }
    None
}

fn parse_event(raw: &str) -> anyhow::Result<Option<MistralEvent>> {
    let data = raw
        .split("\r\n")
        .flat_map(|l| l.split(['\r', '\n']))
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| line.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    let data = data.trim();
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return Ok(Some(MistralEvent::Done));
    }
    let parsed: Value = crate::utils::js_json::js_json_parse(data).map_err(anyhow::Error::msg)?;
    if !parsed.is_object() || !parsed.get("choices").is_some_and(Value::is_array) {
        anyhow::bail!("Invalid Mistral streaming event");
    }
    Ok(Some(MistralEvent::Data(parsed)))
}

impl MistralEventReader {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                text
            }
            Err(e) if e.error_len().is_none() => {
                let rest = self.pending.split_off(e.valid_up_to());
                String::from_utf8(std::mem::replace(&mut self.pending, rest)).unwrap_or_default()
            }
            Err(_) => String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned(),
        }
    }

    /// The next event; `Ok(None)` at the end of the stream or after `[DONE]`.
    async fn next(&mut self, queue: &mut std::collections::VecDeque<Value>) -> anyhow::Result<bool> {
        loop {
            if self.finished {
                return Ok(false);
            }
            while let Some((index, length)) = find_boundary(&self.buffer) {
                let raw = self.buffer[..index].to_string();
                self.buffer = self.buffer[index + length..].to_string();
                match parse_event(&raw)? {
                    Some(MistralEvent::Done) => {
                        self.finished = true;
                        return Ok(!queue.is_empty());
                    }
                    Some(MistralEvent::Data(event)) => queue.push_back(event),
                    None => {}
                }
            }
            if !queue.is_empty() {
                return Ok(true);
            }
            if self.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
                return Err(MistralAborted.into());
            }
            let next = match &self.signal {
                Some(signal) => tokio::select! {
                    chunk = self.body.next() => chunk,
                    _ = signal.cancelled() => return Err(MistralAborted.into()),
                },
                None => self.body.next().await,
            };
            match next {
                Some(Ok(bytes)) => {
                    let text = self.decode(&bytes);
                    self.buffer.push_str(&text);
                }
                Some(Err(error)) => return Err(anyhow::anyhow!("{error}")),
                None => {
                    if !self.pending.is_empty() {
                        let rest = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
                        self.buffer.push_str(&rest);
                    }
                    while let Some((index, length)) = find_boundary(&self.buffer) {
                        let raw = self.buffer[..index].to_string();
                        self.buffer = self.buffer[index + length..].to_string();
                        match parse_event(&raw)? {
                            Some(MistralEvent::Done) => {
                                self.finished = true;
                                return Ok(!queue.is_empty());
                            }
                            Some(MistralEvent::Data(event)) => queue.push_back(event),
                            None => {}
                        }
                    }
                    if !self.buffer.trim().is_empty() {
                        let raw = std::mem::take(&mut self.buffer);
                        if let Some(MistralEvent::Data(event)) = parse_event(&raw)? {
                            queue.push_back(event);
                        }
                    }
                    self.finished = true;
                    return Ok(!queue.is_empty());
                }
            }
        }
    }
}

async fn request_mistral_stream(
    model: &Model,
    payload: &Value,
    api_key: &str,
    options: &StreamOptions,
) -> anyhow::Result<MistralEventReader> {
    let url = chat_url(&model.base_url)?;
    let headers = build_headers(model, api_key, options);
    let client = match &options.http_client {
        Some(client) => client.clone(),
        None => crate::utils::http::client_for(&url, options.env.as_ref())?,
    };
    let mut request = client.post(&url).body(serde_json::to_vec(&to_mistral_wire_payload(payload))?);
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    // The timeout covers only the wait for response headers; long streams must not be cut off.
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_HEADER_TIMEOUT_MS);
    let send = crate::utils::wasm::timeout(Duration::from_millis(timeout_ms), request.send());
    let response = match &options.signal {
        Some(signal) => {
            if signal.is_cancelled() {
                return Err(MistralAborted.into());
            }
            tokio::select! {
                r = send => r,
                _ = signal.cancelled() => return Err(MistralAborted.into()),
            }
        }
        None => send.await,
    };
    let response = match response {
        Err(_) => anyhow::bail!("Mistral response headers timed out after {timeout_ms}ms"),
        Ok(result) => result.map_err(|e| anyhow::Error::new(crate::utils::http::FetchFailed::new(&e)))?,
    };
    if let Some(on_response) = &options.on_response {
        on_response(ProviderResponse { status: response.status().as_u16(), headers: headers_to_record(response.headers()) }, model.clone())
            .await;
    }
    let status = response.status();
    if !status.is_success() {
        let status_text = status.canonical_reason().unwrap_or_default().to_string();
        let body = response.text().await.unwrap_or_default();
        let message = if status_text.is_empty() { format!("Request failed with status {}", status.as_u16()) } else { status_text };
        return Err(MistralHttpError { status_code: status.as_u16(), body, message }.into());
    }
    Ok(MistralEventReader {
        body: Box::pin(crate::utils::wasm::assert_send(response.bytes_stream())),
        buffer: String::new(),
        pending: Vec::new(),
        signal: options.signal.clone(),
        finished: false,
    })
}

fn cached_prompt_tokens(usage: &Value, prompt_tokens: f64) -> f64 {
    let raw = usage
        .get("promptTokensDetails")
        .and_then(|d| d.get("cachedTokens"))
        .or_else(|| usage.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens")))
        .or_else(|| usage.get("promptTokenDetails").and_then(|d| d.get("cachedTokens")))
        .or_else(|| usage.get("prompt_token_details").and_then(|d| d.get("cached_tokens")))
        .or_else(|| usage.get("numCachedTokens"))
        .or_else(|| usage.get("num_cached_tokens"))
        .filter(|v| !v.is_null());
    let cached = raw.and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0);
    prompt_tokens.min(cached.max(0.0))
}

fn map_chat_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        // Mistral reports transient server failures this way; "server error" makes it retryable.
        "error" => (StopReason::Error, Some("Provider stopped with: error (server error)".into())),
        other => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
    }
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

struct StreamState {
    /// Index of the open text/thinking block, if any.
    current: Option<usize>,
    /// Tool call blocks by stream key (`index` or id) with their raw argument text.
    tool_blocks: IndexMap<String, usize>,
    partial_args: HashMap<usize, String>,
}

fn finish_current(state: &mut StreamState, output: &AssistantMessage, sender: &AssistantMessageEventSender) {
    let Some(index) = state.current.take() else { return };
    match &output.content[index] {
        AssistantContent::Text(t) => sender.push(AssistantMessageEvent::TextEnd {
            content_index: output.content.len() - 1,
            content: t.text.clone(),
            partial: output.clone(),
        }),
        AssistantContent::Thinking(t) => sender.push(AssistantMessageEvent::ThinkingEnd {
            content_index: output.content.len() - 1,
            content: t.thinking.clone(),
            partial: output.clone(),
        }),
        AssistantContent::ToolCall(_) => {}
    }
}

fn push_text(state: &mut StreamState, output: &mut AssistantMessage, sender: &AssistantMessageEventSender, delta: String, thinking: bool) {
    if delta.is_empty() {
        return;
    }
    let matches = state.current.is_some_and(|i| match &output.content[i] {
        AssistantContent::Thinking(_) => thinking,
        AssistantContent::Text(_) => !thinking,
        AssistantContent::ToolCall(_) => false,
    });
    if !matches {
        finish_current(state, output, sender);
        if thinking {
            output.content.push(AssistantContent::Thinking(ThinkingContent::new("")));
            sender.push(AssistantMessageEvent::ThinkingStart { content_index: output.content.len() - 1, partial: output.clone() });
        } else {
            output.content.push(AssistantContent::Text(TextContent::new("")));
            sender.push(AssistantMessageEvent::TextStart { content_index: output.content.len() - 1, partial: output.clone() });
        }
        state.current = Some(output.content.len() - 1);
    }
    let index = output.content.len() - 1;
    match &mut output.content[index] {
        AssistantContent::Thinking(t) => {
            t.thinking.push_str(&delta);
            sender.push(AssistantMessageEvent::ThinkingDelta { content_index: index, delta, partial: output.clone() });
        }
        AssistantContent::Text(t) => {
            t.text.push_str(&delta);
            sender.push(AssistantMessageEvent::TextDelta { content_index: index, delta, partial: output.clone() });
        }
        AssistantContent::ToolCall(_) => {}
    }
}

fn consume_chunk(chunk: &Value, model: &Model, output: &mut AssistantMessage, sender: &AssistantMessageEventSender, state: &mut StreamState) {
    if output.response_id.as_deref().is_none_or(str::is_empty)
        && let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())
    {
        output.response_id = Some(id.to_string());
    }
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        let prompt = number(usage.get("prompt_tokens"));
        let cached = cached_prompt_tokens(usage, prompt);
        output.usage.input = (prompt - cached).max(0.0) as u64;
        output.usage.output = number(usage.get("completion_tokens")) as u64;
        output.usage.cache_read = cached as u64;
        output.usage.cache_write = 0;
        let total = number(usage.get("total_tokens")) as u64;
        output.usage.total_tokens = if total > 0 {
            total
        } else {
            output.usage.input + output.usage.output + output.usage.cache_read + output.usage.cache_write
        };
        calculate_cost(&model.cost, &mut output.usage);
    }
    let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else { return };
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        output.raw_stop_reason = Some(reason.to_string());
        let (stop_reason, error) = map_chat_stop_reason(reason);
        output.stop_reason = stop_reason;
        if error.is_some() {
            output.error_message = error;
        }
    }
    let delta = &choice["delta"];
    match delta.get("content") {
        Some(Value::String(text)) => push_text(state, output, sender, sanitize_surrogates(text), false),
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::String(text) => push_text(state, output, sender, sanitize_surrogates(text), false),
                    Value::Object(obj) => match obj.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            let text: String = obj
                                .get("thinking")
                                .and_then(Value::as_array)
                                .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect())
                                .unwrap_or_default();
                            push_text(state, output, sender, sanitize_surrogates(&text), true);
                        }
                        Some("text") => {
                            let text = obj.get("text").and_then(Value::as_str).unwrap_or_default();
                            push_text(state, output, sender, sanitize_surrogates(text), false);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for tool_call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        finish_current(state, output, sender);
        let index_value = tool_call.get("index").and_then(Value::as_u64);
        let call_id = match tool_call.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id != "null" => id.to_string(),
            _ => derive_mistral_tool_call_id(&format!("toolcall:{}", index_value.unwrap_or(0)), 0),
        };
        let key = match index_value {
            Some(i) => format!("i:{i}"),
            None => format!("id:{call_id}"),
        };
        let position = match state.tool_blocks.get(&key).copied().filter(|p| matches!(output.content.get(*p), Some(AssistantContent::ToolCall(_)))) {
            Some(position) => position,
            None => {
                let name = tool_call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default();
                output.content.push(AssistantContent::ToolCall(ToolCall { id: call_id, name: name.to_string(), ..Default::default() }));
                let position = output.content.len() - 1;
                state.tool_blocks.insert(key, position);
                state.partial_args.insert(position, String::new());
                sender.push(AssistantMessageEvent::ToolCallStart { content_index: position, partial: output.clone() });
                position
            }
        };
        let args_delta = match tool_call.get("function").and_then(|f| f.get("arguments")) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => "{}".to_string(),
            Some(other) => other.to_string(),
        };
        let partial = state.partial_args.entry(position).or_default();
        partial.push_str(&args_delta);
        let parsed = parse_streaming_json_object(Some(partial));
        if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position) {
            block.arguments = parsed;
        }
        sender.push(AssistantMessageEvent::ToolCallDelta { content_index: position, delta: args_delta, partial: output.clone() });
    }
}

async fn run_stream(model: Model, context: TranscriptContext, options: StreamOptions, sender: AssistantMessageEventSender) {
    let context = resolve_transcript(&context, model.compat.as_ref().and_then(|c| c.supports_mid_convo_system_messages));
    let mut output = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let result: anyhow::Result<()> = async {
        let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
            anyhow::bail!("No API key for provider: {}", model.provider);
        };
        let normalizer = std::sync::Mutex::new(MistralToolCallIdNormalizer::default());
        let normalize = |id: &str, _: &Model, _: &AssistantMessage| normalizer.lock().map(|mut n| n.normalize(id)).unwrap_or_default();
        let transformed = transform_messages(&context.messages, &model, Some(&normalize));
        let mut payload = build_chat_payload(&model, &context, &transformed, &options)?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(payload.clone(), model.clone()).await
        {
            payload = next;
        }
        let mut reader = request_mistral_stream(&model, &payload, &api_key, &options).await?;
        sender.push(AssistantMessageEvent::Start { partial: output.clone() });
        let mut state = StreamState { current: None, tool_blocks: IndexMap::new(), partial_args: HashMap::new() };
        let mut queue = std::collections::VecDeque::new();
        while reader.next(&mut queue).await? {
            while let Some(chunk) = queue.pop_front() {
                if let Some(observer) = &options.on_provider_stream_event {
                    observer(chunk.clone(), model.clone()).await;
                }
                consume_chunk(&chunk, &model, &mut output, &sender, &mut state);
            }
        }
        finish_current(&mut state, &output, &sender);
        for position in state.tool_blocks.values().copied().collect::<Vec<_>>() {
            let partial = state.partial_args.remove(&position).unwrap_or_default();
            let parsed = parse_tool_call_arguments(Some(&partial));
            if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position) {
                block.arguments = parsed.arguments;
                block.arguments_error = parsed.arguments_error;
                let tool_call = block.clone();
                sender.push(AssistantMessageEvent::ToolCallEnd { content_index: position, tool_call, partial: output.clone() });
            }
        }
        if options.is_aborted() {
            anyhow::bail!("Request was aborted");
        }
        match output.stop_reason {
            StopReason::Pending => anyhow::bail!("Mistral stream ended without a finish reason"),
            StopReason::Aborted | StopReason::Error => {
                anyhow::bail!("{}", output.error_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "An unknown error occurred".into()))
            }
            _ => {}
        }
        sender.push(AssistantMessageEvent::Done { reason: output.stop_reason, message: output.clone() });
        sender.end(None);
        Ok(())
    }
    .await;
    if let Err(error) = result {
        output.stop_reason = if options.is_aborted() { StopReason::Aborted } else { StopReason::Error };
        output.error_message = Some(format_mistral_error(&error));
        sender.push(AssistantMessageEvent::Error { reason: output.stop_reason, error: output.clone() });
        sender.end(None);
    }
}

/// Stream responses from the native Mistral chat completions endpoint.
pub fn stream(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Map provider-neutral options to Mistral options: models with a thinking level map use
/// `reasoning_effort`; other reasoning models use `prompt_mode: "reasoning"`.
pub fn stream_simple(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
        return super::lazy::error_stream(model, anyhow::anyhow!("No API key for provider: {}", model.provider));
    };
    let mut base = build_base_options(model, context, &options, Some(&api_key));
    if let Some(choice) = options.tool_choice.and_then(|c| serde_json::to_value(c).ok()) {
        base.extra.insert("toolChoice".into(), choice);
    }
    let clamped = options.reasoning.map(|r| clamp_thinking_level(model, ModelThinkingLevel::from(r)));
    let reasoning = clamped.filter(|l| *l != ModelThinkingLevel::Off);
    let effort_map = if model.reasoning { model.thinking_level_map.as_ref() } else { None };
    let reasoning_effort: Option<String> = match effort_map {
        Some(map) => match reasoning {
            Some(level) => Some(map.get(&level).cloned().flatten().unwrap_or_else(|| "high".into())),
            None => map.get(&ModelThinkingLevel::Off).cloned().flatten(),
        },
        None => None,
    };
    if model.reasoning && effort_map.is_none() && reasoning.is_some() {
        base.extra.insert("promptMode".into(), json!("reasoning"));
    }
    if let Some(effort) = reasoning_effort {
        base.extra.insert("reasoningEffort".into(), json!(effort));
    }
    stream(model, context, base)
}

/// The `mistral-conversations` API implementation.
pub struct MistralConversationsApi;

impl ProviderStreams for MistralConversationsApi {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        stream(model, context, options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_ids() {
        assert_eq!(derive_mistral_tool_call_id("abc123456", 0), "abc123456");
        let derived = derive_mistral_tool_call_id("call_|xyz", 0);
        assert_eq!(derived.len(), 9);
        let mut normalizer = MistralToolCallIdNormalizer::default();
        let a = normalizer.normalize("toolu_01");
        assert_eq!(normalizer.normalize("toolu_01"), a);
        assert_ne!(normalizer.normalize("toolu_02"), a);
    }

    #[test]
    fn event_boundaries() {
        assert_eq!(find_boundary("a\r\n\r\nb"), Some((1, 4)));
        assert_eq!(find_boundary("a\n\nb"), Some((1, 2)));
        assert_eq!(find_boundary("a\nb"), None);
    }

    #[test]
    fn stop_reasons() {
        assert_eq!(map_chat_stop_reason("model_length").0, StopReason::Length);
        assert_eq!(map_chat_stop_reason("error").1.as_deref(), Some("Provider stopped with: error (server error)"));
        assert_eq!(map_chat_stop_reason("weird").1.as_deref(), Some("Provider stopped with: weird"));
    }
}
