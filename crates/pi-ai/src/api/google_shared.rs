//! Shared utilities for the Google Generative AI and Google Vertex APIs (port of
//! `api/google-shared.ts`), plus the parts of `@google/genai` the TypeScript adapters rely on:
//! the `GenerateContentParameters` → REST body conversion, the client's URL/header/auth rules and
//! its SSE stream reader.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::constrained_sampling::{get_json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use super::google_auth::{GoogleAuth, VERTEX_AI_SCOPE};
use super::transform_messages::transform_messages;
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Model, ModelInputModality, ModelThinkingLevel,
    ProviderHeaders, StopReason, StreamOptions, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext,
    Usage, UserContent, UserContentBlock,
};
use crate::utils::error_body::{ProviderHttpError, format_provider_error, normalize_provider_error};
use crate::utils::event_stream::AssistantMessageEventSender;
use crate::utils::headers::{provider_headers_to_record, to_provider_headers};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::provider_retry::{AbortError, ProviderRetryOptions, retry_provider_request};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::text::{get_system_message_text, get_system_message_text_is_empty};
use crate::utils::transcript::{collapse_system_messages, get_current_tools, get_initial_system_message, without_initial_system_message};

/// Version of `@google/genai` whose request shape this module reproduces.
pub const GOOGLE_GENAI_SDK_VERSION: &str = "2.21.0";
/// Node.js version reported in `x-goog-api-client` (the SDK sends `gl-node/<process.version>`).
const NODE_VERSION_LABEL: &str = "v22.19.0";

// ---------------------------------------------------------------------------
// Thinking levels
// ---------------------------------------------------------------------------

/// Google's thinking levels (`GoogleApiThinkingLevel`).
pub type GoogleApiThinkingLevel = &'static str;

/// A pi thinking level Google supports natively (`minimal`, `low`, `medium`, `high`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedGoogleThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ResolvedGoogleThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolvedGoogleThinkingLevel::Minimal => "minimal",
            ResolvedGoogleThinkingLevel::Low => "low",
            ResolvedGoogleThinkingLevel::Medium => "medium",
            ResolvedGoogleThinkingLevel::High => "high",
        }
    }
}

fn level_name(level: ModelThinkingLevel) -> String {
    serde_json::to_value(level).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// Resolve a supported pi level or model-specific Google mapping to a standard Google level.
pub fn resolve_google_thinking_level(model: &Model, level: ModelThinkingLevel) -> anyhow::Result<ResolvedGoogleThinkingLevel> {
    let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(&level));
    let resolved = match mapped {
        Some(Some(value)) => value.to_lowercase(),
        _ => level_name(level),
    };
    match resolved.as_str() {
        "minimal" => Ok(ResolvedGoogleThinkingLevel::Minimal),
        "low" => Ok(ResolvedGoogleThinkingLevel::Low),
        "medium" => Ok(ResolvedGoogleThinkingLevel::Medium),
        "high" => Ok(ResolvedGoogleThinkingLevel::High),
        _ => {
            let shown = match mapped {
                None => "undefined".to_string(),
                Some(None) => "null".to_string(),
                Some(Some(value)) => value.clone(),
            };
            anyhow::bail!(
                "Unsupported Google thinking level mapping for {}/{}: {} -> {shown}",
                model.provider,
                model.id,
                level_name(level)
            )
        }
    }
}

/// Whether this model uses Gemini's discrete `thinkingLevel` control instead of the token-based
/// `thinkingBudget` control.
pub fn uses_google_thinking_level(model: &Model) -> bool {
    static GEMINI_3: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"gemini-3(?:\.\d+)?-(?:pro|flash)").expect("valid regex"));
    static GEMINI_4_PLUS: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"gemini-(?:[4-9]|[1-9]\d)").expect("valid regex"));
    static GEMMA_4: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"gemma-?4").expect("valid regex"));
    let id = model.id.to_lowercase();
    GEMINI_3.is_match(&id)
        || GEMINI_4_PLUS.is_match(&id)
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || GEMMA_4.is_match(&id)
}

pub fn to_google_thinking_level(level: ResolvedGoogleThinkingLevel) -> GoogleApiThinkingLevel {
    match level {
        ResolvedGoogleThinkingLevel::Minimal => "MINIMAL",
        ResolvedGoogleThinkingLevel::Low => "LOW",
        ResolvedGoogleThinkingLevel::Medium => "MEDIUM",
        ResolvedGoogleThinkingLevel::High => "HIGH",
    }
}

/// The thinking config that disables (or minimizes) thinking.
pub fn get_disabled_google_thinking_config(model: &Model) -> anyhow::Result<Value> {
    if !uses_google_thinking_level(model) {
        return Ok(json!({ "thinkingBudget": 0 }));
    }
    let fallback = clamp_thinking_level(model, ModelThinkingLevel::Off);
    if fallback == ModelThinkingLevel::Off {
        return Ok(json!({ "thinkingBudget": 0 }));
    }
    let resolved = resolve_google_thinking_level(model, fallback)?;
    Ok(json!({ "thinkingLevel": to_google_thinking_level(resolved) }))
}

/// Whether a streamed Gemini part is thinking: `thought: true` is the definitive marker.
/// `thoughtSignature` can appear on any part type and does not make a part thinking.
pub fn is_thinking_part(part: &Value) -> bool {
    part.get("thought") == Some(&Value::Bool(true))
}

/// Keep the last non-empty thought signature of a streamed block.
pub fn retain_thought_signature(existing: Option<String>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(incoming) if !incoming.is_empty() => Some(incoming.to_string()),
        _ => existing,
    }
}

fn is_valid_thought_signature(signature: Option<&str>) -> bool {
    static BASE64: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"^[A-Za-z0-9+/]+={0,2}$").expect("valid regex"));
    match signature {
        Some(s) if !s.is_empty() => s.len() % 4 == 0 && BASE64.is_match(s),
        _ => false,
    }
}

/// Only keep signatures from the same provider/model and with valid base64.
fn resolve_thought_signature(same_provider_and_model: bool, signature: Option<&str>) -> Option<String> {
    if same_provider_and_model && is_valid_thought_signature(signature) { signature.map(str::to_string) } else { None }
}

fn gemini_major_version(model_id: &str) -> Option<u32> {
    static RE: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"^gemini(?:-live)?-(\d+)").expect("valid regex"));
    RE.captures(&model_id.to_lowercase()).and_then(|c| c[1].parse().ok())
}

/// Models via Google APIs that require explicit tool call IDs in function calls/responses.
pub fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).is_none_or(|v| v >= 3)
}

fn inline_data(mime_type: &str, data: &str) -> Value {
    json!({ "inlineData": { "mimeType": mime_type, "data": data } })
}

/// Convert transcript messages to Gemini `Content[]`.
pub fn convert_messages(model: &Model, context: &TranscriptContext) -> Vec<Value> {
    let collapsed = collapse_system_messages(context);
    let conversation = without_initial_system_message(&collapsed.messages);
    let needs_id = requires_tool_call_id(&model.id);
    let normalize = |id: &str, _: &Model, _: &AssistantMessage| -> String {
        if !needs_id {
            return id.to_string();
        }
        let replaced: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
        replaced.chars().take(64).collect()
    };
    let transformed = transform_messages(conversation, model, Some(&normalize));
    let mut contents: Vec<Value> = Vec::new();

    for message in &transformed {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    contents.push(json!({ "role": "user", "parts": [{ "text": sanitize_surrogates(text) }] }));
                }
                UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|block| match block {
                            UserContentBlock::Text(t) => json!({ "text": sanitize_surrogates(&t.text) }),
                            UserContentBlock::Image(i) => inline_data(&i.mime_type, &i.data),
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            },
            Message::Assistant(assistant) => {
                let same = assistant.provider == model.provider && assistant.model == model.id;
                let mut parts: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            let signature = resolve_thought_signature(same, text.text_signature.as_deref());
                            if text.is_blank() && signature.is_none() {
                                continue;
                            }
                            let mut part = Map::new();
                            part.insert("text".into(), json!(sanitize_surrogates(&text.text)));
                            if let Some(signature) = signature {
                                part.insert("thoughtSignature".into(), json!(signature));
                            }
                            parts.push(Value::Object(part));
                        }
                        AssistantContent::Thinking(thinking) => {
                            if same {
                                let signature = resolve_thought_signature(same, thinking.thinking_signature.as_deref());
                                if thinking.is_blank() && signature.is_none() {
                                    continue;
                                }
                                let mut part = Map::new();
                                part.insert("thought".into(), json!(true));
                                part.insert("text".into(), json!(sanitize_surrogates(&thinking.thinking)));
                                if let Some(signature) = signature {
                                    part.insert("thoughtSignature".into(), json!(signature));
                                }
                                parts.push(Value::Object(part));
                            } else {
                                if thinking.is_blank() {
                                    continue;
                                }
                                parts.push(json!({ "text": sanitize_surrogates(&thinking.thinking) }));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            let signature = resolve_thought_signature(same, call.thought_signature.as_deref());
                            let mut function_call = Map::new();
                            function_call.insert("name".into(), json!(call.name));
                            function_call.insert("args".into(), Value::Object(call.arguments.clone()));
                            if needs_id {
                                function_call.insert("id".into(), json!(call.id));
                            }
                            let mut part = Map::new();
                            part.insert("functionCall".into(), Value::Object(function_call));
                            if let Some(signature) = signature {
                                part.insert("thoughtSignature".into(), json!(signature));
                            }
                            parts.push(Value::Object(part));
                        }
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Message::ToolResult(result) => {
                let text_result =
                    result.content.iter().filter_map(UserContentBlock::as_text).map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n");
                let images: Vec<_> = if model.input.contains(&ModelInputModality::Image) {
                    result.content.iter().filter_map(UserContentBlock::as_image).collect()
                } else {
                    Vec::new()
                };
                let removed = result.content.iter().filter_map(UserContentBlock::as_text).any(|t| t.lone_surrogates_removed);
                let has_text = !crate::types::is_blank_text(&text_result, removed);
                let has_images = !images.is_empty();
                let multimodal = supports_multimodal_function_response(&model.id);
                let response_value = if has_text {
                    sanitize_surrogates(&text_result)
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };
                let image_parts: Vec<Value> = images.iter().map(|i| inline_data(&i.mime_type, &i.data)).collect();
                let mut function_response = Map::new();
                function_response.insert("name".into(), json!(result.tool_name));
                function_response.insert(
                    "response".into(),
                    if result.is_error { json!({ "error": response_value }) } else { json!({ "output": response_value }) },
                );
                if has_images && multimodal {
                    function_response.insert("parts".into(), Value::Array(image_parts.clone()));
                }
                if needs_id {
                    function_response.insert("id".into(), json!(result.tool_call_id));
                }
                let part = json!({ "functionResponse": function_response });
                let merge = contents.last().is_some_and(|last| {
                    last["role"] == "user"
                        && last["parts"].as_array().is_some_and(|p| p.iter().any(|p| p.get("functionResponse").is_some()))
                });
                if merge {
                    if let Some(Value::Array(parts)) = contents.last_mut().and_then(|l| l.get_mut("parts")) {
                        parts.push(part);
                    }
                } else {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }
                if has_images && !multimodal {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(image_parts);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Message::System(_) => {}
        }
    }
    contents
}

const JSON_SCHEMA_META_DECLARATIONS: [&str; 8] =
    ["$schema", "$id", "$anchor", "$dynamicAnchor", "$vocabulary", "$comment", "$defs", "definitions"];

/// Strip JSON Schema meta declarations recursively through objects (arrays are kept as-is).
fn sanitize_for_open_api(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !JSON_SCHEMA_META_DECLARATIONS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), sanitize_for_open_api(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Convert tools to Gemini function declarations. `use_parameters` selects the legacy OpenAPI
/// `parameters` field (meta keys stripped) instead of `parametersJsonSchema`.
pub fn convert_tools(tools: &[Tool], use_parameters: bool, supports_strict_mode: bool) -> anyhow::Result<Option<Value>> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut declarations = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode, None)?;
        let parameters = get_json_schema_tool_parameters(tool, strict);
        let mut declaration = Map::new();
        declaration.insert("name".into(), json!(tool.name));
        declaration.insert("description".into(), json!(tool.description));
        if use_parameters {
            declaration.insert("parameters".into(), sanitize_for_open_api(&parameters));
        } else {
            declaration.insert("parametersJsonSchema".into(), parameters);
        }
        declarations.push(Value::Object(declaration));
    }
    Ok(Some(json!([{ "functionDeclarations": declarations }])))
}

/// Gemini 3+ enforces required function parameters in validated tool-calling modes.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

/// Map a tool choice string to a Gemini `FunctionCallingConfigMode`.
pub fn map_tool_choice(choice: &str) -> &'static str {
    match choice {
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

/// The function calling mode for the current tools.
pub fn resolve_google_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> anyhow::Result<Option<&'static str>> {
    let mut use_strict = false;
    for tool in tools {
        if resolve_json_schema_strict_sampling(tool, supports_strict_mode, None)? == Some(true) {
            use_strict = true;
        }
    }
    if let Some(choice @ ("none" | "any")) = tool_choice {
        return Ok(Some(map_tool_choice(choice)));
    }
    if use_strict {
        return Ok(Some("VALIDATED"));
    }
    Ok(tool_choice.map(map_tool_choice))
}

/// Map a Gemini `FinishReason` to a stop reason. Values outside the SDK enum are rejected.
pub fn map_stop_reason(reason: &str) -> anyhow::Result<StopReason> {
    Ok(match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "SAFETY" | "IMAGE_SAFETY" | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION" | "IMAGE_OTHER" | "RECITATION" | "FINISH_REASON_UNSPECIFIED" | "OTHER" | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "TOO_MANY_TOOL_CALLS" | "NO_IMAGE" => StopReason::Error,
        other => anyhow::bail!("Unhandled stop reason: {other}"),
    })
}

/// Map a raw finish reason string to a stop reason.
pub fn map_stop_reason_string(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

/// Run a Google request with the shared provider retry policy (408/409/429/5xx with backoff).
/// Google API errors carry a status but no headers, so `retry-after` is not honored.
pub async fn retry_google_request<T, F, Fut>(request: F, options: &StreamOptions) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let retry = ProviderRetryOptions {
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        signal: options.signal.clone(),
        no_retry_statuses: Vec::new(),
    };
    retry_provider_request(request, &retry).await
}

/// `@google/genai`'s `ApiError`.
pub fn google_api_error(status: u16, message: String) -> ProviderHttpError {
    ProviderHttpError { status: Some(status), headers: None, message, body: None, error: None, name: "ApiError".into() }
}

// ---------------------------------------------------------------------------
// GenerateContentParameters -> REST body
// ---------------------------------------------------------------------------

/// `tModel`: the model resource path for the backend.
pub fn t_model(model: &str, vertex: bool) -> anyhow::Result<String> {
    if model.is_empty() {
        anyhow::bail!("model is required and must be a string");
    }
    if model.contains("..") || model.contains('?') || model.contains('&') {
        anyhow::bail!("invalid model parameter");
    }
    if vertex {
        if model.starts_with("publishers/") || model.starts_with("projects/") || model.starts_with("models/") {
            return Ok(model.to_string());
        }
        if let Some((publisher, rest)) = model.split_once('/') {
            let name = rest.split('/').next().unwrap_or(rest);
            return Ok(format!("publishers/{publisher}/models/{name}"));
        }
        return Ok(format!("publishers/google/models/{model}"));
    }
    if model.starts_with("models/") || model.starts_with("tunedModels/") {
        return Ok(model.to_string());
    }
    Ok(format!("models/{model}"))
}

fn copy_keys(from: &Map<String, Value>, to: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = from.get(*key).filter(|v| !v.is_null()) {
            to.insert((*key).to_string(), value.clone());
        }
    }
}

fn convert_part(part: &Value, vertex: bool) -> anyhow::Result<Value> {
    let Some(part) = part.as_object() else {
        return match part {
            Value::String(text) => Ok(json!({ "text": text })),
            _ => anyhow::bail!("Unsupported part type"),
        };
    };
    let mut out = Map::new();
    copy_keys(part, &mut out, &["mediaResolution"]);
    if vertex {
        for key in ["toolCall", "toolResponse"] {
            if part.contains_key(key) {
                anyhow::bail!(
                    "{key} parameter is only supported in Gemini Developer API mode, not in Gemini Enterprise Agent Platform mode."
                );
            }
        }
    } else {
        copy_keys(part, &mut out, &["toolCall", "toolResponse"]);
    }
    copy_keys(part, &mut out, &["audioTranscription", "codeExecutionResult", "executableCode", "fileData"]);
    if let Some(call) = part.get("functionCall").filter(|v| !v.is_null()) {
        if vertex {
            out.insert("functionCall".into(), call.clone());
        } else {
            let call = call.as_object().cloned().unwrap_or_default();
            for key in ["partialArgs", "willContinue"] {
                if call.contains_key(key) {
                    anyhow::bail!(
                        "{key} parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
                    );
                }
            }
            let mut converted = Map::new();
            copy_keys(&call, &mut converted, &["args", "id", "name"]);
            out.insert("functionCall".into(), Value::Object(converted));
        }
    }
    copy_keys(part, &mut out, &["functionResponse", "inlineData", "text", "thought", "thoughtSignature", "videoMetadata"]);
    if vertex {
        if part.contains_key("partMetadata") {
            anyhow::bail!(
                "partMetadata parameter is only supported in Gemini Developer API mode, not in Gemini Enterprise Agent Platform mode."
            );
        }
    } else {
        copy_keys(part, &mut out, &["partMetadata"]);
    }
    copy_keys(part, &mut out, &["mediaProcessing"]);
    Ok(Value::Object(out))
}

fn convert_content(content: &Value, vertex: bool) -> anyhow::Result<Value> {
    let mut out = Map::new();
    if let Some(parts) = content.get("parts").filter(|v| !v.is_null()) {
        let converted = match parts {
            Value::Array(items) => Value::Array(items.iter().map(|p| convert_part(p, vertex)).collect::<anyhow::Result<_>>()?),
            other => other.clone(),
        };
        out.insert("parts".into(), converted);
    }
    if let Some(role) = content.get("role").filter(|v| !v.is_null()) {
        out.insert("role".into(), role.clone());
    }
    Ok(Value::Object(out))
}

fn is_content(value: &Value) -> bool {
    value.get("parts").is_some_and(Value::is_array)
}

/// `tContent`: a `Content`, or parts/text wrapped in a user content.
fn t_content(value: &Value) -> anyhow::Result<Value> {
    if value.is_null() {
        anyhow::bail!("ContentUnion is required");
    }
    if is_content(value) {
        return Ok(value.clone());
    }
    let parts = match value {
        Value::Array(items) if items.is_empty() => anyhow::bail!("PartListUnion is required"),
        Value::Array(items) => items.iter().map(t_part).collect::<anyhow::Result<Vec<_>>>()?,
        other => vec![t_part(other)?],
    };
    Ok(json!({ "role": "user", "parts": parts }))
}

fn t_part(value: &Value) -> anyhow::Result<Value> {
    match value {
        Value::Null => anyhow::bail!("PartUnion is required"),
        Value::String(text) => Ok(json!({ "text": text })),
        Value::Object(_) => Ok(value.clone()),
        _ => anyhow::bail!("Unsupported part type"),
    }
}

/// `tContents`.
fn t_contents(value: &Value) -> anyhow::Result<Vec<Value>> {
    match value {
        Value::Null => anyhow::bail!("contents are required"),
        Value::Array(items) if items.is_empty() => anyhow::bail!("contents are required"),
        Value::Array(items) => {
            let content_array = is_content(&items[0]);
            let mut result = Vec::new();
            let mut accumulated = Vec::new();
            for item in items {
                if is_content(item) != content_array {
                    anyhow::bail!(
                        "Mixing Content and Parts is not supported, please group the parts into a the appropriate Content objects and specify the roles for them"
                    );
                }
                if content_array {
                    result.push(item.clone());
                } else if item.get("functionCall").is_some() || item.get("functionResponse").is_some() {
                    anyhow::bail!(
                        "To specify functionCall or functionResponse parts, please wrap them in a Content object, specifying the role for them"
                    );
                } else {
                    accumulated.push(t_part(item)?);
                }
            }
            if !content_array {
                result.push(json!({ "role": "user", "parts": accumulated }));
            }
            Ok(result)
        }
        other => {
            if other.get("functionCall").is_some() || other.get("functionResponse").is_some() {
                anyhow::bail!(
                    "To specify functionCall or functionResponse parts, please wrap them in a Content object, specifying the role for them"
                );
            }
            Ok(vec![t_content(other)?])
        }
    }
}

fn convert_tool(tool: &Value, vertex: bool) -> anyhow::Result<Value> {
    let Some(tool) = tool.as_object() else { return Ok(tool.clone()) };
    let mut out = Map::new();
    if vertex {
        copy_keys(tool, &mut out, &[
            "retrieval",
            "googleMaps",
            "mcpServers",
            "codeExecution",
            "computerUse",
            "enterpriseWebSearch",
            "exaAiSearch",
            "functionDeclarations",
            "googleSearch",
            "googleSearchRetrieval",
            "parallelAiSearch",
            "urlContext",
        ]);
        if tool.contains_key("fileSearch") {
            anyhow::bail!(
                "fileSearch parameter is only supported in Gemini Developer API mode, not in Gemini Enterprise Agent Platform mode."
            );
        }
    } else {
        for key in ["retrieval", "enterpriseWebSearch", "exaAiSearch", "parallelAiSearch"] {
            if tool.contains_key(key) {
                anyhow::bail!(
                    "{key} parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
                );
            }
        }
        copy_keys(tool, &mut out, &[
            "googleMaps",
            "mcpServers",
            "codeExecution",
            "computerUse",
            "functionDeclarations",
            "googleSearch",
            "googleSearchRetrieval",
            "urlContext",
            "fileSearch",
        ]);
    }
    Ok(Value::Object(out))
}

fn convert_tool_config(config: &Value, vertex: bool) -> anyhow::Result<Value> {
    let Some(config) = config.as_object() else { return Ok(config.clone()) };
    let mut out = Map::new();
    if let Some(fcc) = config.get("functionCallingConfig").filter(|v| !v.is_null()) {
        if vertex {
            out.insert("functionCallingConfig".into(), fcc.clone());
        } else {
            let fcc = fcc.as_object().cloned().unwrap_or_default();
            if fcc.contains_key("streamFunctionCallArguments") {
                anyhow::bail!(
                    "streamFunctionCallArguments parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
                );
            }
            let mut converted = Map::new();
            copy_keys(&fcc, &mut converted, &["allowedFunctionNames", "mode"]);
            out.insert("functionCallingConfig".into(), Value::Object(converted));
        }
    }
    copy_keys(config, &mut out, &["retrievalConfig"]);
    if vertex {
        if config.contains_key("includeServerSideToolInvocations") {
            anyhow::bail!(
                "includeServerSideToolInvocations parameter is only supported in Gemini Developer API mode, not in Gemini Enterprise Agent Platform mode."
            );
        }
    } else {
        copy_keys(config, &mut out, &["includeServerSideToolInvocations"]);
    }
    Ok(Value::Object(out))
}

/// Convert SDK-shaped `{ model, contents, config }` parameters into the REST request body and the
/// model resource path (`generateContentParametersToMldev` / `...ToVertex`).
pub fn generate_content_request_body(params: &Value, vertex: bool) -> anyhow::Result<(String, Value)> {
    let model = params.get("model").and_then(Value::as_str).unwrap_or_default();
    let model_path = t_model(model, vertex)?;
    let mut body = Map::new();
    if let Some(contents) = params.get("contents").filter(|v| !v.is_null()) {
        let converted = t_contents(contents)?.iter().map(|c| convert_content(c, vertex)).collect::<anyhow::Result<Vec<_>>>()?;
        body.insert("contents".into(), Value::Array(converted));
    }
    if let Some(config) = params.get("config").filter(|v| !v.is_null()) {
        let config = config.as_object().cloned().unwrap_or_default();
        let mut generation = Map::new();
        copy_keys(&config, &mut body, &["serviceTier"]);
        if let Some(system) = config.get("systemInstruction").filter(|v| !v.is_null()) {
            body.insert("systemInstruction".into(), convert_content(&t_content(system)?, vertex)?);
        }
        copy_keys(&config, &mut generation, &[
            "temperature",
            "topP",
            "topK",
            "candidateCount",
            "maxOutputTokens",
            "stopSequences",
            "responseLogprobs",
            "logprobs",
            "presencePenalty",
            "frequencyPenalty",
            "seed",
            "responseMimeType",
            "responseSchema",
            "responseJsonSchema",
        ]);
        if vertex {
            copy_keys(&config, &mut generation, &["routingConfig"]);
            if let Some(selection) = config.get("modelSelectionConfig").filter(|v| !v.is_null()) {
                generation.insert("modelConfig".into(), selection.clone());
            }
        } else {
            for key in ["routingConfig", "modelSelectionConfig"] {
                if config.contains_key(key) {
                    anyhow::bail!(
                        "{key} parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
                    );
                }
            }
        }
        copy_keys(&config, &mut body, &["safetySettings"]);
        if let Some(tools) = config.get("tools").filter(|v| !v.is_null()) {
            let list = match tools {
                Value::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            let converted = list.iter().map(|t| convert_tool(t, vertex)).collect::<anyhow::Result<Vec<_>>>()?;
            body.insert("tools".into(), Value::Array(converted));
        }
        if let Some(tool_config) = config.get("toolConfig").filter(|v| !v.is_null()) {
            body.insert("toolConfig".into(), convert_tool_config(tool_config, vertex)?);
        }
        if vertex {
            copy_keys(&config, &mut body, &["labels"]);
        } else if config.contains_key("labels") {
            anyhow::bail!(
                "labels parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
            );
        }
        copy_keys(&config, &mut body, &["cachedContent"]);
        copy_keys(&config, &mut generation, &["responseModalities", "mediaResolution", "speechConfig"]);
        if vertex {
            copy_keys(&config, &mut generation, &["audioTimestamp"]);
        } else if config.contains_key("audioTimestamp") {
            anyhow::bail!(
                "audioTimestamp parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
            );
        }
        copy_keys(&config, &mut generation, &["thinkingConfig", "audioTranscriptionConfig", "imageConfig"]);
        if vertex {
            if config.contains_key("enableEnhancedCivicAnswers") {
                anyhow::bail!(
                    "enableEnhancedCivicAnswers parameter is only supported in Gemini Developer API mode, not in Gemini Enterprise Agent Platform mode."
                );
            }
            copy_keys(&config, &mut body, &["modelArmorConfig"]);
        } else {
            copy_keys(&config, &mut generation, &["enableEnhancedCivicAnswers"]);
            if config.contains_key("modelArmorConfig") {
                anyhow::bail!(
                    "modelArmorConfig parameter is only supported in Gemini Enterprise Agent Platform mode, not in Gemini Developer API mode."
                );
            }
        }
        body.insert("generationConfig".into(), Value::Object(generation));
    }
    Ok((model_path, Value::Object(body)))
}

// ---------------------------------------------------------------------------
// GoogleGenAI client
// ---------------------------------------------------------------------------

/// The `GoogleGenAI` constructor options pi uses.
#[derive(Debug, Clone, Default)]
pub struct GoogleClientOptions {
    pub vertexai: bool,
    pub api_key: Option<String>,
    pub project: Option<String>,
    pub location: Option<String>,
    pub api_version: Option<String>,
    /// `httpOptions.baseUrl`.
    pub base_url: Option<String>,
    /// `httpOptions.apiVersion`.
    pub http_api_version: Option<String>,
    /// `httpOptions.baseUrlResourceScope === COLLECTION`.
    pub base_url_collection_scope: bool,
    /// `httpOptions.headers`.
    pub headers: Option<IndexMap<String, String>>,
    /// `googleAuthOptions.keyFilename`.
    pub key_filename: Option<String>,
}

enum ClientAuth {
    ApiKey(String),
    /// The client's own `GoogleAuth`: credentials and token are reused across this client's
    /// requests (retries) but not across clients.
    Google(std::sync::Arc<GoogleAuth>),
}

/// A resolved `GoogleGenAI` client: endpoint, default headers and auth.
pub struct GoogleClient {
    vertex: bool,
    base_url: String,
    api_version: String,
    project: Option<String>,
    location: Option<String>,
    collection_scope: bool,
    headers: Vec<(String, String)>,
    auth: ClientAuth,
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl GoogleClient {
    /// Resolve options the way the `GoogleGenAI` constructor and its `ApiClient` do.
    pub fn new(options: GoogleClientOptions) -> anyhow::Result<GoogleClient> {
        let vertex = options.vertexai;
        if (options.project.is_some() || options.location.is_some()) && !vertex {
            anyhow::bail!("Project and location are not supported for Gemini API backend.");
        }
        let env_api_key = process_env("GOOGLE_API_KEY").or_else(|| process_env("GEMINI_API_KEY"));
        let env_project = process_env("GOOGLE_CLOUD_PROJECT");
        let env_location = process_env("GOOGLE_CLOUD_LOCATION");
        let mut api_key = options.api_key.clone().or(env_api_key.clone());
        let mut project = options.project.clone().or(env_project.clone());
        let mut location = options.location.clone().or(env_location.clone());
        if vertex {
            let explicit_location_or_project = options.project.is_some() || options.location.is_some();
            if !explicit_location_or_project && (env_project.is_some() || env_location.is_some()) && options.api_key.is_some() {
                project = None;
                location = None;
            } else if options.api_key.is_none()
                && env_api_key.is_some()
                && (explicit_location_or_project || env_project.is_some() || env_location.is_some())
            {
                // Explicit project/location win over an implicit API key; so do implicit ones.
                api_key = None;
            }
            if location.is_none() && api_key.is_none() {
                location = Some("global".into());
            }
        }
        let custom_base_url = options.base_url.clone().filter(|u| !u.is_empty()).or_else(|| {
            if vertex { process_env("GOOGLE_VERTEX_BASE_URL") } else { process_env("GOOGLE_GEMINI_BASE_URL") }
        });

        // ApiClient
        let mut base_url: Option<String> = None;
        let api_version;
        if vertex {
            if location.is_none() && api_key.is_none() && custom_base_url.is_none() {
                location = Some("global".into());
            }
            let sufficient = (project.is_some() && location.is_some()) || api_key.is_some();
            if !sufficient && custom_base_url.is_none() {
                anyhow::bail!(
                    "Authentication is not set up. Please provide either a project and location, or an API key, or a custom base URL."
                );
            }
            let constructor_auth = (project.is_some() && location.is_some()) || api_key.is_some();
            if custom_base_url.is_some() && !constructor_auth {
                base_url = custom_base_url.clone();
                project = None;
                location = None;
            } else if (api_key.is_some() && project.is_none()) || location.as_deref() == Some("global") {
                base_url = Some("https://aiplatform.googleapis.com/".into());
            } else if let (Some(_), Some(loc)) = (&project, &location) {
                base_url = Some(if loc == "us" || loc == "eu" {
                    format!("https://aiplatform.{loc}.rep.googleapis.com/")
                } else {
                    format!("https://{loc}-aiplatform.googleapis.com/")
                });
            }
            api_version = options.api_version.clone().unwrap_or_else(|| "v1beta1".into());
        } else {
            api_version = options.api_version.clone().unwrap_or_else(|| "v1beta".into());
            base_url = Some("https://generativelanguage.googleapis.com/".into());
        }
        if let Some(custom) = &custom_base_url {
            base_url = Some(custom.clone());
        }
        let api_version = options.http_api_version.clone().unwrap_or(api_version);
        let Some(base_url) = base_url else {
            anyhow::bail!("HTTP options are not correctly set.");
        };

        let version_label = format!("google-genai-sdk/{GOOGLE_GENAI_SDK_VERSION} gl-node/{NODE_VERSION_LABEL}");
        let mut defaults: IndexMap<String, String> = IndexMap::new();
        defaults.insert("User-Agent".into(), version_label.clone());
        defaults.insert("x-goog-api-client".into(), version_label);
        defaults.insert("Content-Type".into(), "application/json".into());
        if let Some(custom) = &options.headers {
            for (name, value) in custom {
                defaults.insert(name.clone(), value.clone());
            }
        }
        let headers = defaults.into_iter().collect();
        let auth = match api_key {
            Some(key) => {
                if key.starts_with("auth_tokens/") {
                    anyhow::bail!("Ephemeral tokens are only supported by the live API.");
                }
                ClientAuth::ApiKey(key)
            }
            None => ClientAuth::Google(std::sync::Arc::new(GoogleAuth::new(options.key_filename.clone()))),
        };
        Ok(GoogleClient {
            vertex,
            base_url,
            api_version,
            project,
            location,
            collection_scope: options.base_url_collection_scope && custom_base_url.is_some(),
            headers,
            auth,
        })
    }

    /// The streaming URL for a model resource path.
    pub fn stream_url(&self, model_path: &str) -> String {
        let mut elements = vec![self.base_url.trim_end_matches('/').to_string()];
        if !self.api_version.is_empty() {
            elements.push(self.api_version.clone());
        }
        let prepend = !self.collection_scope
            && self.vertex
            && self.project.is_some()
            && self.location.is_some()
            && !model_path.starts_with("projects/");
        if prepend {
            elements.push(format!(
                "projects/{}/locations/{}",
                self.project.as_deref().unwrap_or_default(),
                self.location.as_deref().unwrap_or_default()
            ));
        }
        elements.push(format!("{model_path}:streamGenerateContent"));
        let joined = elements.join("/");
        match url::Url::parse(&joined) {
            Ok(mut url) => {
                let has_sse = url.query_pairs().any(|(k, v)| k == "alt" && v == "sse");
                if !has_sse {
                    let pairs: Vec<(String, String)> =
                        url.query_pairs().filter(|(k, _)| k != "alt").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
                    url.query_pairs_mut().clear().extend_pairs(pairs).append_pair("alt", "sse");
                }
                url.to_string()
            }
            Err(_) => format!("{joined}?alt=sse"),
        }
    }

    async fn request_headers(&self, client: &reqwest::Client) -> anyhow::Result<Vec<(String, String)>> {
        let mut headers: Vec<(String, String)> = Vec::new();
        for (name, value) in &self.headers {
            append_header(&mut headers, name, value);
        }
        let has = |headers: &Vec<(String, String)>, name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
        match &self.auth {
            ClientAuth::ApiKey(key) => {
                if !has(&headers, "x-goog-api-key") {
                    append_header(&mut headers, "x-goog-api-key", key);
                }
            }
            ClientAuth::Google(auth) => {
                for (name, value) in auth.get_request_headers(VERTEX_AI_SCOPE, client).await? {
                    if !has(&headers, &name) {
                        append_header(&mut headers, &name, &value);
                    }
                }
            }
        }
        Ok(headers)
    }

    /// `models.generateContentStream(params)`: POST the converted body and return the SSE chunk
    /// reader. HTTP failures become `ApiError`s.
    pub async fn generate_content_stream(
        &self,
        client: &reqwest::Client,
        params: &Value,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<GoogleChunkReader> {
        let (model_path, body) = generate_content_request_body(params, self.vertex)?;
        let url = self.stream_url(&model_path);
        let headers = self.request_headers(client).await?;
        let mut request = client.post(&url).body(serde_json::to_vec(&body)?);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let send = request.send();
        let response = match signal {
            Some(signal) => {
                if signal.is_cancelled() {
                    anyhow::bail!(AbortError);
                }
                tokio::select! {
                    r = send => r,
                    _ = signal.cancelled() => anyhow::bail!(AbortError),
                }
            }
            None => send.await,
        }
        .map_err(|e| anyhow::Error::new(crate::utils::http::FetchFailed::new(&e)))?;
        let status = response.status();
        if !status.is_success() {
            let is_json = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("application/json"));
            let status_text = status.canonical_reason().unwrap_or_default().to_string();
            let text = response.text().await.unwrap_or_default();
            let error_body = match serde_json::from_str::<Value>(&text) {
                Ok(json) if is_json => json,
                _ => json!({ "error": { "message": text, "code": status.as_u16(), "status": status_text } }),
            };
            let message = error_body.to_string();
            if (400..600).contains(&status.as_u16()) {
                return Err(google_api_error(status.as_u16(), message).into());
            }
            anyhow::bail!("{message}");
        }
        Ok(GoogleChunkReader::new(Box::pin(crate::utils::wasm::assert_send(response.bytes_stream())), signal.cloned()))
    }
}

fn append_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some((_, existing)) = headers.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        existing.push_str(", ");
        existing.push_str(value);
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

type ByteStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// The SDK's `processStreamResponse`: splits the body on blank lines (`\n\n`, `\r\r`,
/// `\r\n\r\n`) and yields the JSON of each `data:` event.
pub struct GoogleChunkReader {
    body: ByteStream,
    buffer: String,
    pending: Vec<u8>,
    queue: std::collections::VecDeque<Value>,
    done: bool,
    signal: Option<CancellationToken>,
}

impl GoogleChunkReader {
    pub fn new(body: ByteStream, signal: Option<CancellationToken>) -> Self {
        Self { body, buffer: String::new(), pending: Vec::new(), queue: Default::default(), done: false, signal }
    }

    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                text
            }
            Err(e) if e.error_len().is_none() => {
                let valid = e.valid_up_to();
                let rest = self.pending.split_off(valid);
                String::from_utf8(std::mem::replace(&mut self.pending, rest)).unwrap_or_default()
            }
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                text
            }
        }
    }

    fn drain_events(&mut self) -> anyhow::Result<()> {
        loop {
            let mut found: Option<(usize, usize)> = None;
            for delimiter in ["\n\n", "\r\r", "\r\n\r\n"] {
                if let Some(index) = self.buffer.find(delimiter)
                    && found.is_none_or(|(i, _)| index < i)
                {
                    found = Some((index, delimiter.len()));
                }
            }
            let Some((index, length)) = found else { return Ok(()) };
            let event = self.buffer[..index].to_string();
            self.buffer = self.buffer[index + length..].to_string();
            let trimmed = event.trim();
            if let Some(data) = trimmed.strip_prefix("data:") {
                let data = data.trim();
                let json: Value = crate::utils::js_json::js_json_parse(data).map_err(anyhow::Error::msg)?;
                self.queue.push_back(json);
            }
        }
    }

    /// The next chunk; `Ok(None)` at the end of the stream.
    pub async fn next_chunk(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            if let Some(chunk) = self.queue.pop_front() {
                return Ok(Some(chunk));
            }
            if self.done {
                return Ok(None);
            }
            let next = match &self.signal {
                Some(signal) => {
                    if signal.is_cancelled() {
                        anyhow::bail!("This operation was aborted");
                    }
                    tokio::select! {
                        chunk = self.body.next() => chunk,
                        _ = signal.cancelled() => anyhow::bail!("This operation was aborted"),
                    }
                }
                None => self.body.next().await,
            };
            match next {
                Some(Ok(bytes)) => {
                    let chunk = self.decode(&bytes);
                    // The SDK parses each raw network chunk and throws when it is an error object.
                    if let Ok(Value::Object(json)) = serde_json::from_str::<Value>(&chunk)
                        && let Some(error) = json.get("error")
                    {
                        let code = error.get("code").and_then(Value::as_f64).unwrap_or(0.0);
                        if (400.0..600.0).contains(&code) {
                            let status = match error.get("status") {
                                Some(Value::String(s)) => s.clone(),
                                Some(other) => other.to_string(),
                                None => "undefined".into(),
                            };
                            let message = format!("got status: {status}. {}", Value::Object(json.clone()));
                            return Err(google_api_error(code as u16, message).into());
                        }
                    }
                    self.buffer.push_str(&chunk);
                    self.drain_events()?;
                }
                Some(Err(error)) => return Err(anyhow::anyhow!("{error}")),
                None => {
                    self.done = true;
                    if !self.pending.is_empty() {
                        let rest = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
                        self.buffer.push_str(&rest);
                        self.drain_events()?;
                    }
                    if !self.buffer.trim().is_empty() {
                        anyhow::bail!("Incomplete JSON segment at the end");
                    }
                }
            }
        }
    }
}

/// `providerHeadersToRecord(getIdentityHeaders(model, identity), model.headers, optionsHeaders)`.
pub fn google_http_headers(model: &Model, options: &StreamOptions) -> Option<IndexMap<String, String>> {
    let identity = to_provider_headers(&get_identity_headers(
        ClientIdentityTarget { provider: &model.provider, base_url: &model.base_url },
        options.identity.as_ref(),
    ));
    let model_headers: Option<ProviderHeaders> = model.headers.as_ref().map(to_provider_headers);
    provider_headers_to_record(&[Some(&identity), model_headers.as_ref(), options.headers.as_ref()])
}

// ---------------------------------------------------------------------------
// Request parameters and streaming
// ---------------------------------------------------------------------------

/// `GoogleOptions.thinking`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GoogleThinkingOptions {
    pub enabled: bool,
    /// -1 for dynamic, 0 to disable.
    pub budget_tokens: Option<i64>,
    pub level: Option<GoogleApiThinkingLevel>,
}

impl GoogleThinkingOptions {
    /// Read `StreamOptions.extra.thinking` (`{ enabled, budgetTokens?, level? }`).
    pub fn from_options(options: &StreamOptions) -> Option<GoogleThinkingOptions> {
        let thinking = options.extra_value("thinking")?.as_object()?;
        let level = thinking.get("level").and_then(Value::as_str).and_then(|l| match l {
            "THINKING_LEVEL_UNSPECIFIED" => Some("THINKING_LEVEL_UNSPECIFIED"),
            "MINIMAL" => Some("MINIMAL"),
            "LOW" => Some("LOW"),
            "MEDIUM" => Some("MEDIUM"),
            "HIGH" => Some("HIGH"),
            _ => None,
        });
        Some(GoogleThinkingOptions {
            enabled: thinking.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            budget_tokens: thinking.get("budgetTokens").and_then(Value::as_i64),
            level,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("enabled".into(), json!(self.enabled));
        if let Some(budget) = self.budget_tokens {
            map.insert("budgetTokens".into(), json!(budget));
        }
        if let Some(level) = self.level {
            map.insert("level".into(), json!(level));
        }
        Value::Object(map)
    }
}

/// The `toolChoice` option (`"auto" | "none" | "any"`).
pub fn google_tool_choice(options: &StreamOptions) -> Option<String> {
    options
        .extra_str("toolChoice")
        .map(str::to_string)
        .or_else(|| options.tool_choice.and_then(|c| serde_json::to_value(c).ok()).and_then(|v| v.as_str().map(str::to_string)))
}

/// Build SDK-shaped `{ model, contents, config }` parameters (`buildParams`).
pub fn build_params(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> anyhow::Result<Value> {
    let contents = convert_messages(model, context);
    let initial_system = get_initial_system_message(&context.messages);
    let current_tools = get_current_tools(&context.messages);
    let supports_strict = supports_google_strict_tool_sampling(&model.id);
    let tool_choice = google_tool_choice(options);
    let function_calling_mode = if current_tools.is_empty() {
        None
    } else {
        resolve_google_function_calling_mode(&current_tools, tool_choice.as_deref(), supports_strict)?
    };
    let system_instruction = initial_system.map(get_system_message_text).unwrap_or_default();
    let system_instruction_empty = initial_system.is_none_or(get_system_message_text_is_empty);

    let mut config = Map::new();
    if let Some(temperature) = options.temperature {
        config.insert("temperature".into(), crate::serde_util::js_number(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        config.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if !system_instruction_empty {
        config.insert("systemInstruction".into(), json!(sanitize_surrogates(&system_instruction)));
    }
    if let Some(tools) = convert_tools(&current_tools, false, supports_strict)? {
        config.insert("tools".into(), tools);
    }
    if let Some(mode) = function_calling_mode {
        config.insert("toolConfig".into(), json!({ "functionCallingConfig": { "mode": mode } }));
    }
    let thinking = GoogleThinkingOptions::from_options(options);
    match &thinking {
        Some(thinking) if thinking.enabled && model.reasoning => {
            let mut thinking_config = Map::new();
            thinking_config.insert("includeThoughts".into(), json!(true));
            if let Some(level) = thinking.level {
                thinking_config.insert("thinkingLevel".into(), json!(level));
            } else if let Some(budget) = thinking.budget_tokens {
                thinking_config.insert("thinkingBudget".into(), json!(budget));
            }
            config.insert("thinkingConfig".into(), Value::Object(thinking_config));
        }
        Some(thinking) if model.reasoning && !thinking.enabled => {
            config.insert("thinkingConfig".into(), get_disabled_google_thinking_config(model)?);
        }
        _ => {}
    }
    if options.is_aborted() {
        anyhow::bail!("Request aborted");
    }
    Ok(json!({ "model": model.id, "contents": contents, "config": config }))
}

/// Token budget for a thinking level (`getGoogleBudget`); -1 means dynamic.
pub fn get_google_budget(
    model: &Model,
    level: ResolvedGoogleThinkingLevel,
    custom: Option<&crate::types::ThinkingBudgets>,
    include_flash_lite: bool,
) -> i64 {
    let custom_value = custom.and_then(|b| match level {
        ResolvedGoogleThinkingLevel::Minimal => b.minimal,
        ResolvedGoogleThinkingLevel::Low => b.low,
        ResolvedGoogleThinkingLevel::Medium => b.medium,
        ResolvedGoogleThinkingLevel::High => b.high,
    });
    if let Some(value) = custom_value {
        return value as i64;
    }
    let pick = |budgets: [i64; 4]| match level {
        ResolvedGoogleThinkingLevel::Minimal => budgets[0],
        ResolvedGoogleThinkingLevel::Low => budgets[1],
        ResolvedGoogleThinkingLevel::Medium => budgets[2],
        ResolvedGoogleThinkingLevel::High => budgets[3],
    };
    if model.id.contains("2.5-pro") {
        return pick([128, 2048, 8192, 32768]);
    }
    if include_flash_lite && model.id.contains("2.5-flash-lite") {
        return pick([512, 2048, 8192, 24576]);
    }
    if model.id.contains("2.5-flash") {
        return pick([128, 2048, 8192, 24576]);
    }
    -1
}

/// Map simple options to Google options (`streamSimple`): thinking from `reasoning`.
pub fn simple_thinking_options(
    model: &Model,
    options: &StreamOptions,
    include_flash_lite_budgets: bool,
) -> anyhow::Result<GoogleThinkingOptions> {
    let Some(reasoning) = options.reasoning else {
        return Ok(GoogleThinkingOptions { enabled: false, ..Default::default() });
    };
    let clamped = clamp_thinking_level(model, ModelThinkingLevel::from(reasoning));
    if clamped == ModelThinkingLevel::Off {
        return Ok(GoogleThinkingOptions { enabled: false, ..Default::default() });
    }
    let resolved = resolve_google_thinking_level(model, clamped)?;
    if uses_google_thinking_level(model) {
        return Ok(GoogleThinkingOptions { enabled: true, level: Some(to_google_thinking_level(resolved)), budget_tokens: None });
    }
    Ok(GoogleThinkingOptions {
        enabled: true,
        budget_tokens: Some(get_google_budget(model, resolved, options.thinking_budgets.as_ref(), include_flash_lite_budgets)),
        level: None,
    })
}

static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);
static VERTEX_TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn end_current_block(output: &AssistantMessage, sender: &AssistantMessageEventSender) {
    let index = output.content.len().saturating_sub(1);
    match output.content.last() {
        Some(AssistantContent::Text(text)) => sender.push(AssistantMessageEvent::TextEnd {
            content_index: index,
            content: text.text.clone(),
            partial: output.clone(),
        }),
        Some(AssistantContent::Thinking(thinking)) => sender.push(AssistantMessageEvent::ThinkingEnd {
            content_index: index,
            content: thinking.thinking.clone(),
            partial: output.clone(),
        }),
        _ => {}
    }
}

fn token_count(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_f64).filter(|v| v.is_finite()).map(|v| v as i64).unwrap_or(0)
}

/// Process one streamed `GenerateContentResponse` chunk.
fn process_chunk(
    chunk: &Value,
    model: &Model,
    output: &mut AssistantMessage,
    sender: &AssistantMessageEventSender,
    in_block: &mut bool,
) -> anyhow::Result<()> {
    if output.response_id.as_deref().is_none_or(str::is_empty)
        && let Some(id) = chunk.get("responseId").and_then(Value::as_str).filter(|s| !s.is_empty())
    {
        output.response_id = Some(id.to_string());
    }
    let candidate = chunk.get("candidates").and_then(|c| c.get(0));
    if let Some(parts) = candidate.and_then(|c| c.get("content")).and_then(|c| c.get("parts")).and_then(Value::as_array) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                let thinking = is_thinking_part(part);
                let switch = match (output.content.last(), *in_block) {
                    (_, false) => true,
                    (Some(AssistantContent::Thinking(_)), true) => !thinking,
                    (Some(AssistantContent::Text(_)), true) => thinking,
                    _ => true,
                };
                if switch {
                    if *in_block {
                        end_current_block(output, sender);
                    }
                    if thinking {
                        output.content.push(AssistantContent::Thinking(ThinkingContent::new("")));
                        sender.push(AssistantMessageEvent::ThinkingStart {
                            content_index: output.content.len() - 1,
                            partial: output.clone(),
                        });
                    } else {
                        output.content.push(AssistantContent::Text(TextContent::new("")));
                        sender.push(AssistantMessageEvent::TextStart { content_index: output.content.len() - 1, partial: output.clone() });
                    }
                    *in_block = true;
                }
                let signature = part.get("thoughtSignature").and_then(Value::as_str);
                let index = output.content.len() - 1;
                match output.content.last_mut() {
                    Some(AssistantContent::Thinking(block)) => {
                        block.thinking.push_str(text);
                        block.thinking_signature = retain_thought_signature(block.thinking_signature.take(), signature);
                        sender.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: text.to_string(),
                            partial: output.clone(),
                        });
                    }
                    Some(AssistantContent::Text(block)) => {
                        block.text.push_str(text);
                        block.text_signature = retain_thought_signature(block.text_signature.take(), signature);
                        sender.push(AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta: text.to_string(),
                            partial: output.clone(),
                        });
                    }
                    _ => {}
                }
            }
            if let Some(function_call) = part.get("functionCall").filter(|f| f.is_object()) {
                if *in_block {
                    end_current_block(output, sender);
                    *in_block = false;
                }
                let name = function_call.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let provided = function_call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
                let duplicate =
                    provided.is_some_and(|id| output.content.iter().any(|b| matches!(b, AssistantContent::ToolCall(c) if c.id == id)));
                // `${functionCall.name}_${Date.now()}_${++toolCallCounter}`: a missing name prints as
                // "undefined"; google-generative-ai and google-vertex keep separate counters.
                let counter = if model.api == "google-vertex" { &VERTEX_TOOL_CALL_COUNTER } else { &TOOL_CALL_COUNTER };
                let id = match provided {
                    Some(id) if !duplicate => id.to_string(),
                    _ => format!(
                        "{}_{}_{}",
                        match function_call.get("name") {
                            None => "undefined".to_string(),
                            Some(value) => crate::serde_util::js_string(value),
                        },
                        crate::utils::now_ms(),
                        counter.fetch_add(1, Ordering::SeqCst) + 1
                    ),
                };
                let arguments = function_call.get("args").and_then(Value::as_object).cloned().unwrap_or_default();
                let tool_call = ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature: part.get("thoughtSignature").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
                    ..Default::default()
                };
                output.content.push(AssistantContent::ToolCall(tool_call.clone()));
                let index = output.content.len() - 1;
                sender.push(AssistantMessageEvent::ToolCallStart { content_index: index, partial: output.clone() });
                sender.push(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta: Value::Object(tool_call.arguments.clone()).to_string(),
                    partial: output.clone(),
                });
                sender.push(AssistantMessageEvent::ToolCallEnd { content_index: index, tool_call, partial: output.clone() });
            }
        }
    }
    if let Some(reason) = candidate.and_then(|c| c.get("finishReason")).and_then(Value::as_str).filter(|r| !r.is_empty()) {
        output.raw_stop_reason = Some(reason.to_string());
        output.stop_reason = map_stop_reason(reason)?;
        if output.stop_reason == StopReason::Stop && output.content.iter().any(|b| matches!(b, AssistantContent::ToolCall(_))) {
            output.stop_reason = StopReason::ToolUse;
        }
    }
    if let Some(usage) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
        let prompt = token_count(usage.get("promptTokenCount"));
        let cached = token_count(usage.get("cachedContentTokenCount"));
        let candidates = token_count(usage.get("candidatesTokenCount"));
        let thoughts = token_count(usage.get("thoughtsTokenCount"));
        output.usage = Usage {
            input: (prompt - cached).max(0) as u64,
            output: (candidates + thoughts).max(0) as u64,
            cache_read: cached.max(0) as u64,
            cache_write: 0,
            reasoning: Some(thoughts.max(0) as u64),
            total_tokens: token_count(usage.get("totalTokenCount")).max(0) as u64,
            ..Default::default()
        };
        calculate_cost(&model.cost, &mut output.usage);
    }
    Ok(())
}

/// Builds the client for one request.
pub type CreateGoogleClient = Box<dyn FnOnce(&Model, &StreamOptions) -> anyhow::Result<GoogleClient> + Send>;

/// What differs between the Generative AI and Vertex adapters.
pub struct GoogleStreamSpec {
    /// Name used in "... stream ended without a finish reason".
    pub label: &'static str,
    /// Build the client (may fail, e.g. missing project).
    pub create_client: CreateGoogleClient,
    /// Error for a custom HTTP client, which the SDK-based adapters do not support.
    pub custom_client_error: &'static str,
}

/// Stream one `generateContentStream` call and normalize it into assistant events.
pub async fn run_google_stream(model: Model, context: TranscriptContext, options: StreamOptions, sender: AssistantMessageEventSender, spec: GoogleStreamSpec) {
    let context = collapse_system_messages(&context);
    let mut output = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let result: anyhow::Result<()> = async {
        if options.http_client.is_some() {
            anyhow::bail!("{}", spec.custom_client_error);
        }
        let client = (spec.create_client)(&model, &options)?;
        let mut params = build_params(&model, &context, &options)?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(params.clone(), model.clone()).await
        {
            params = next;
        }
        let http = crate::utils::http::client_for(&client.base_url, options.env.as_ref())?;
        let mut reader =
            retry_google_request(|| client.generate_content_stream(&http, &params, options.signal.as_ref()), &options).await?;
        sender.push(AssistantMessageEvent::Start { partial: output.clone() });
        let mut in_block = false;
        while let Some(chunk) = reader.next_chunk().await? {
            if let Some(observer) = &options.on_provider_stream_event {
                observer(chunk.clone(), model.clone()).await;
            }
            process_chunk(&chunk, &model, &mut output, &sender, &mut in_block)?;
        }
        if in_block {
            end_current_block(&output, &sender);
        }
        if options.is_aborted() {
            anyhow::bail!("Request was aborted");
        }
        match output.stop_reason {
            StopReason::Pending => anyhow::bail!("{} stream ended without a finish reason", spec.label),
            StopReason::Aborted | StopReason::Error => match &output.raw_stop_reason {
                Some(raw) => anyhow::bail!("Provider stopped with: {raw}"),
                None => anyhow::bail!("An unknown error occurred"),
            },
            _ => {}
        }
        sender.push(AssistantMessageEvent::Done { reason: output.stop_reason, message: output.clone() });
        sender.end(None);
        Ok(())
    }
    .await;
    if let Err(error) = result {
        output.stop_reason = if options.is_aborted() { StopReason::Aborted } else { StopReason::Error };
        output.error_message = Some(format_provider_error(&normalize_provider_error(&error), None));
        sender.push(AssistantMessageEvent::Error { reason: output.stop_reason, error: output.clone() });
        sender.end(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_paths() {
        assert_eq!(t_model("gemini-2.5-flash", false).unwrap(), "models/gemini-2.5-flash");
        assert_eq!(t_model("tunedModels/x", false).unwrap(), "tunedModels/x");
        assert_eq!(t_model("gemini-2.5-flash", true).unwrap(), "publishers/google/models/gemini-2.5-flash");
        assert_eq!(t_model("anthropic/claude", true).unwrap(), "publishers/anthropic/models/claude");
        assert!(t_model("a..b", true).is_err());
    }

    #[test]
    fn request_body_shape() {
        let params = json!({
            "model": "gemini-2.5-flash",
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }, { "role": "model", "parts": [{ "functionCall": { "name": "f", "args": {}, "id": "1" }, "thoughtSignature": "c2ln" }] }],
            "config": { "temperature": 0.5, "maxOutputTokens": 10, "systemInstruction": "sys", "tools": [{ "functionDeclarations": [] }], "toolConfig": { "functionCallingConfig": { "mode": "AUTO" } }, "thinkingConfig": { "thinkingBudget": 0 } }
        });
        let (path, body) = generate_content_request_body(&params, false).unwrap();
        assert_eq!(path, "models/gemini-2.5-flash");
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["contents", "systemInstruction", "tools", "toolConfig", "generationConfig"]);
        assert_eq!(body["systemInstruction"], json!({ "parts": [{ "text": "sys" }], "role": "user" }));
        assert_eq!(body["generationConfig"], json!({ "temperature": 0.5, "maxOutputTokens": 10, "thinkingConfig": { "thinkingBudget": 0 } }));
        let call_keys: Vec<&String> = body["contents"][1]["parts"][0]["functionCall"].as_object().unwrap().keys().collect();
        assert_eq!(call_keys, ["args", "id", "name"]);
        assert!(generate_content_request_body(&json!({ "model": "m", "contents": [] }), false).is_err());
    }

    #[test]
    fn thought_signature_validation() {
        assert!(is_valid_thought_signature(Some("AAAA")));
        assert!(!is_valid_thought_signature(Some("AAA")));
        assert!(!is_valid_thought_signature(Some("not base64!")));
        assert!(!is_valid_thought_signature(None));
    }

    #[test]
    fn stop_reasons() {
        assert_eq!(map_stop_reason("STOP").unwrap(), StopReason::Stop);
        assert_eq!(map_stop_reason("MAX_TOKENS").unwrap(), StopReason::Length);
        assert_eq!(map_stop_reason("SAFETY").unwrap(), StopReason::Error);
        assert!(map_stop_reason("NEW_REASON").is_err());
        assert_eq!(map_stop_reason_string("NEW_REASON"), StopReason::Error);
    }
}
