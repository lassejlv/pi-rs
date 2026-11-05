//! Amazon Bedrock Converse streaming API (port of `api/bedrock-converse-stream.ts`).
//!
//! Implemented over HTTP the way `@aws-sdk/client-bedrock-runtime` sends `ConverseStream`:
//! `POST {endpoint}/model/{modelId}/converse-stream` with a JSON body, signed with SigV4
//! ([`super::aws_sigv4`]) or sent with a Bedrock API key (`Authorization: Bearer`), and an
//! `application/vnd.amazon.eventstream` response ([`super::aws_eventstream`]). Credentials and
//! regions follow the SDK default chain ([`super::aws_credentials`]). Requests are retried with
//! the SDK's standard retry strategy (3 attempts by default, `AWS_MAX_ATTEMPTS`).
//!
//! API-specific options are read from `StreamOptions.extra`: `region`, `profile`, `toolChoice`
//! (`auto`/`any`/`none` or `{ type: "tool", name }`), `interleavedThinking`, `thinkingDisplay`
//! (`summarized`/`omitted`), `requestMetadata` and `bearerToken`. `reasoning` and
//! `thinkingBudgets` are the regular stream options. The `onPayload` hook receives the
//! `ConverseStreamCommand` input with binary fields (image bytes, redacted reasoning) as base64
//! strings.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::aws_credentials::{CredentialContext, default_provider, profile_region};
use super::aws_eventstream::{EventMessage, EventStreamDecoder};
use super::aws_sigv4::{AwsCredentials, SignableRequest, escape_uri, sign_request};
use super::constrained_sampling::{get_json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use super::simple_options::{adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context, clamp_reasoning};
use super::transform_messages::{split_error_tool_result_images, transform_messages};
use crate::models::{ProviderStreams, calculate_cost};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Message, Model, ModelThinkingLevel,
    ProviderEnv, ProviderResponse, StopReason, StreamOptions, TextContent, ThinkingBudgets, ThinkingContent,
    ThinkingLevel, Tool, ToolCall, TranscriptContext, UserContent, UserContentBlock,
};
use crate::utils::diagnostics::{AssistantMessageDiagnostic, append_assistant_message_diagnostic};
use crate::utils::error_body::{MAX_PROVIDER_ERROR_BODY_CHARS, truncate_error_text};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::headers::{headers_to_record, provider_headers_to_record};
use crate::utils::identity::{ClientIdentity, UserAgentMode, UserAgentNamed, get_client_version};
use crate::utils::json_parse::{parse_streaming_json_object, parse_tool_call_arguments};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::text::{get_system_message_text, get_system_message_text_is_empty};
use crate::utils::transcript::{collapse_system_messages, get_current_tools, get_initial_system_message, without_initial_system_message};

/// Version of `@aws-sdk/client-bedrock-runtime` whose request shape this module reproduces.
pub const AWS_SDK_VERSION: &str = "3.1127.0";
const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const BEDROCK_DATA_RETENTION_DOCS_URL: &str = "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A Bedrock failure, mirroring what the AWS SDK throws.
#[derive(Debug, Clone)]
pub enum BedrockError {
    /// A modeled or unmodeled service exception from an HTTP error response
    /// (`BedrockRuntimeServiceException`): `name` is the AWS error code (`Unknown` without one).
    Service { name: String, message: String, status: u16, request_id: Option<String>, body: Option<String> },
    /// A modeled exception delivered as an event stream frame: the SDK throws the bare shape.
    StreamObject(Value),
    /// An `Error` with a name: unmodeled stream exceptions and `:message-type: error` frames.
    Named { name: String, message: String },
}

impl std::fmt::Display for BedrockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BedrockError::Service { message, .. } | BedrockError::Named { message, .. } => f.write_str(message),
            BedrockError::StreamObject(value) => f.write_str(&value.to_string()),
        }
    }
}

impl std::error::Error for BedrockError {}

/// Human-readable prefixes for Bedrock service exception names. Retry logic matches patterns like
/// `server.?error` and `service.?unavailable` in them.
fn bedrock_error_prefix(name: &str) -> &str {
    match name {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        other => other,
    }
}

fn format_bedrock_error(error: &anyhow::Error) -> String {
    let (core, service_name) = match error.downcast_ref::<BedrockError>() {
        Some(BedrockError::Service { name, message, status, body, .. }) => {
            let body = body.as_deref().map(str::trim).filter(|b| !b.is_empty()).map(|b| truncate_error_text(b, MAX_PROVIDER_ERROR_BODY_CHARS));
            let core = match body {
                Some(body) if !message.contains(&body) => format!("{status}: {body}"),
                _ => message.clone(),
            };
            (core, Some(name.clone()))
        }
        Some(BedrockError::StreamObject(value)) => (value.to_string(), None),
        Some(BedrockError::Named { message, .. }) => (message.clone(), None),
        None => (error.to_string(), None),
    };
    let hint = if core.to_lowercase().contains("data retention mode") {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    match service_name {
        Some(name) => format!("{}: {core}{hint}", bedrock_error_prefix(&name)),
        None => format!("{core}{hint}"),
    }
}

fn normalize_diagnostic_value(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

fn append_failure_diagnostic(output: &mut AssistantMessage, error: &anyhow::Error, fallback_request_id: Option<&str>) {
    let mut details = Map::new();
    let bedrock = error.downcast_ref::<BedrockError>();
    if let Some(BedrockError::Service { status, .. }) = bedrock {
        details.insert("status".into(), json!(status));
    }
    let name = match bedrock {
        Some(BedrockError::Service { name, .. }) | Some(BedrockError::Named { name, .. }) => Some(name.as_str()),
        _ => None,
    };
    if let Some(code) = name.filter(|n| n.ends_with("Exception")).and_then(|n| normalize_diagnostic_value(Some(n))) {
        details.insert("errorCode".into(), json!(code));
    }
    let request_id = match bedrock {
        Some(BedrockError::Service { request_id, .. }) => normalize_diagnostic_value(request_id.as_deref()),
        _ => None,
    }
    .or_else(|| fallback_request_id.map(str::to_string));
    if let Some(request_id) = request_id {
        details.insert("requestId".into(), json!(request_id));
    }
    if details.is_empty() {
        return;
    }
    append_assistant_message_diagnostic(
        &mut output.diagnostics,
        AssistantMessageDiagnostic {
            diagnostic_type: "bedrock_response_failure".into(),
            timestamp: crate::utils::now_ms(),
            error: None,
            details: Some(details),
        },
    );
}

// ---------------------------------------------------------------------------
// Model capabilities
// ---------------------------------------------------------------------------

fn model_match_candidates(model_id: &str, model_name: Option<&str>) -> Vec<String> {
    let separators = regex::Regex::new(r"[\s_.:]+").expect("valid regex");
    let mut values = vec![model_id];
    if let Some(name) = model_name.filter(|n| !n.is_empty()) {
        values.push(name);
    }
    values
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let dashed = separators.replace_all(&lower, "-").into_owned();
            [lower, dashed]
        })
        .collect()
}

fn candidates_contain(model: &Model, needles: &[&str]) -> bool {
    model_match_candidates(&model.id, Some(&model.name)).iter().any(|c| needles.iter().any(|n| c.contains(n)))
}

/// Whether the model supports adaptive thinking (Opus 4.6+, Sonnet 4.6, Claude 5).
pub fn supports_adaptive_thinking(model_id: &str, model_name: Option<&str>) -> bool {
    model_match_candidates(model_id, model_name).iter().any(|s| {
        ["opus-4-6", "opus-4-7", "opus-4-8", "opus-5", "sonnet-4-6", "sonnet-5", "haiku-5", "fable-5"].iter().any(|n| s.contains(n))
    })
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
    candidates_contain(model, &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5"])
}

fn supports_thinking_block_binding(model: &Model) -> bool {
    candidates_contain(model, &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5"])
}

/// Whether the model is an Anthropic Claude model (by id or name).
pub fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

/// Whether to add cache points (Claude 3.5 Haiku, 3.7 Sonnet, 4.x, 5; `AWS_BEDROCK_FORCE_CACHE=1`
/// for application inference profiles without a recognizable name).
pub fn supports_prompt_caching(model: &Model, env: Option<&ProviderEnv>) -> bool {
    let candidates = model_match_candidates(&model.id, Some(&model.name));
    if !candidates.iter().any(|s| s.contains("claude")) {
        return get_provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    candidates.iter().any(|s| {
        s.contains("fable-5")
            || s.contains("opus-5")
            || s.contains("sonnet-5")
            || s.contains("haiku-5")
            || s.contains("-4-")
            || s.contains("claude-3-7-sonnet")
            || s.contains("claude-3-5-haiku")
    })
}

fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::Xhigh && supports_native_xhigh_effort(model) {
        return "xhigh".into();
    }
    if let Some(Some(mapped)) = model.thinking_level_map.as_ref().and_then(|m| m.get(&ModelThinkingLevel::from(level))) {
        return mapped.clone();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .into()
}

fn resolve_cache_retention(cache_retention: Option<CacheRetention>, env: Option<&ProviderEnv>) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

fn cache_point(retention: CacheRetention) -> Value {
    let mut point = Map::new();
    point.insert("type".into(), json!("default"));
    if retention == CacheRetention::Long {
        point.insert("ttl".into(), json!("1h"));
    }
    json!({ "cachePoint": point })
}

// ---------------------------------------------------------------------------
// Request conversion
// ---------------------------------------------------------------------------

fn build_system_prompt(prompt: Option<&str>, model: &Model, retention: CacheRetention, env: Option<&ProviderEnv>) -> Option<Value> {
    let prompt = prompt?;
    let mut blocks = vec![json!({ "text": sanitize_surrogates(prompt) })];
    if retention != CacheRetention::None && supports_prompt_caching(model, env) {
        blocks.push(cache_point(retention));
    }
    Some(Value::Array(blocks))
}

fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    sanitized.chars().take(64).collect()
}

fn non_blank_text(text: &str) -> Option<Value> {
    let sanitized = sanitize_surrogates(text);
    if sanitized.trim().is_empty() { None } else { Some(json!({ "text": sanitized })) }
}

fn required_text(text: &str) -> Value {
    non_blank_text(text).unwrap_or_else(|| json!({ "text": EMPTY_TEXT_PLACEHOLDER }))
}

/// Drop empty property names (Bedrock rejects them in tool input documents).
fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().filter(|(k, _)| !k.is_empty()).map(|(k, v)| (k.clone(), sanitize_document(v))).collect())
        }
        other => other.clone(),
    }
}

fn image_block(mime_type: &str, data: &str) -> anyhow::Result<Value> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => anyhow::bail!("Unknown image type: {other}"),
    };
    // Validate like `atob` and carry the bytes as base64 (the JSON wire form of a blob).
    let bytes = BASE64.decode(data.trim()).map_err(|_| anyhow::anyhow!("The string to be decoded is not correctly encoded."))?;
    Ok(json!({ "source": { "bytes": BASE64.encode(bytes) }, "format": format }))
}

fn tool_result_content(content: &[UserContentBlock]) -> anyhow::Result<Vec<Value>> {
    let mut result = Vec::new();
    for block in content {
        match block {
            UserContentBlock::Image(image) => result.push(json!({ "image": image_block(&image.mime_type, &image.data)? })),
            UserContentBlock::Text(text) => {
                if let Some(text) = non_blank_text(&text.text) {
                    result.push(text);
                }
            }
        }
    }
    if result.is_empty() {
        result.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
    }
    Ok(result)
}

fn convert_messages(context: &TranscriptContext, model: &Model, retention: CacheRetention, env: Option<&ProviderEnv>) -> anyhow::Result<Vec<Value>> {
    let normalize = |id: &str, _: &Model, _: &AssistantMessage| normalize_tool_call_id(id);
    let transformed = transform_messages(without_initial_system_message(&context.messages), model, Some(&normalize));
    let claude = is_anthropic_claude_model(model);
    let mut result: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(user) => {
                let mut content = Vec::new();
                match &user.content {
                    UserContent::Text(text) => content.push(required_text(text)),
                    UserContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                UserContentBlock::Text(t) => {
                                    if let Some(text) = non_blank_text(&t.text) {
                                        content.push(text);
                                    }
                                }
                                UserContentBlock::Image(image) => {
                                    content.push(json!({ "image": image_block(&image.mime_type, &image.data)? }))
                                }
                            }
                        }
                        if content.is_empty() {
                            content.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                        }
                    }
                }
                result.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                if assistant.content.is_empty() {
                    i += 1;
                    continue;
                }
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) => {
                            if let Some(text) = non_blank_text(&t.text) {
                                blocks.push(text);
                            }
                        }
                        AssistantContent::ToolCall(call) => blocks.push(json!({
                            "toolUse": { "toolUseId": call.id, "name": call.name, "input": sanitize_document(&Value::Object(call.arguments.clone())) }
                        })),
                        AssistantContent::Thinking(t) => {
                            if t.redacted == Some(true) {
                                let decoded = t.thinking_signature.as_deref().and_then(|s| BASE64.decode(s).ok()).filter(|b| !b.is_empty());
                                if let Some(bytes) = decoded {
                                    blocks.push(json!({ "reasoningContent": { "redactedContent": BASE64.encode(bytes) } }));
                                }
                                continue;
                            }
                            let thinking = sanitize_surrogates(&t.thinking);
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            if claude {
                                match t.thinking_signature.as_deref().filter(|s| !s.trim().is_empty()) {
                                    None => blocks.push(json!({ "text": thinking })),
                                    Some(signature) => blocks.push(json!({
                                        "reasoningContent": { "reasoningText": { "text": thinking, "signature": signature } }
                                    })),
                                }
                            } else {
                                blocks.push(json!({ "reasoningContent": { "reasoningText": { "text": thinking } } }));
                            }
                        }
                    }
                }
                if !blocks.is_empty() {
                    result.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Message::ToolResult(_) => {
                let mut tool_results = Vec::new();
                let mut error_images = Vec::new();
                let mut j = i;
                while let Some(Message::ToolResult(tool)) = transformed.get(j) {
                    let split = if claude { split_error_tool_result_images(tool) } else { None };
                    let content = match &split {
                        Some(split) => {
                            let blocks: Vec<UserContentBlock> = split.content.iter().cloned().map(UserContentBlock::Text).collect();
                            tool_result_content(&blocks)?
                        }
                        None => tool_result_content(&tool.content)?,
                    };
                    tool_results.push(json!({
                        "toolResult": { "toolUseId": tool.tool_call_id, "content": content, "status": if tool.is_error { "error" } else { "success" } }
                    }));
                    if let Some(split) = split {
                        error_images.push(json!({ "text": split.caption }));
                        for image in &split.images {
                            error_images.push(json!({ "image": image_block(&image.mime_type, &image.data)? }));
                        }
                    }
                    j += 1;
                }
                i = j - 1;
                tool_results.extend(error_images);
                result.push(json!({ "role": "user", "content": tool_results }));
            }
            Message::System(_) => {}
        }
        i += 1;
    }
    if retention != CacheRetention::None
        && supports_prompt_caching(model, env)
        && let Some(last) = result.last_mut().filter(|m| m["role"] == "user")
        && let Some(Value::Array(content)) = last.get_mut("content")
    {
        content.push(cache_point(retention));
    }
    Ok(result)
}

fn convert_tool_config(tools: &[Tool], tool_choice: Option<&Value>, supports_strict: bool) -> anyhow::Result<Option<Value>> {
    if tools.is_empty() || tool_choice.and_then(Value::as_str) == Some("none") {
        return Ok(None);
    }
    let mut specs = Vec::new();
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict, None)?;
        let mut spec = Map::new();
        spec.insert("name".into(), json!(tool.name));
        spec.insert("description".into(), json!(tool.description));
        spec.insert("inputSchema".into(), json!({ "json": get_json_schema_tool_parameters(tool, strict) }));
        if strict == Some(true) {
            spec.insert("strict".into(), json!(true));
        }
        specs.push(json!({ "toolSpec": spec }));
    }
    let mut config = Map::new();
    config.insert("tools".into(), Value::Array(specs));
    let choice = match tool_choice {
        Some(Value::String(s)) if s == "auto" => Some(json!({ "auto": {} })),
        Some(Value::String(s)) if s == "any" => Some(json!({ "any": {} })),
        Some(Value::Object(obj)) if obj.get("type").and_then(Value::as_str) == Some("tool") => {
            Some(json!({ "tool": { "name": obj.get("name").cloned().unwrap_or(Value::Null) } }))
        }
        _ => None,
    };
    if let Some(choice) = choice {
        config.insert("toolChoice".into(), choice);
    }
    Ok(Some(Value::Object(config)))
}

fn configured_region(options: &StreamOptions) -> Option<String> {
    options
        .extra_str("region")
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| get_provider_env_value("AWS_REGION", options.env.as_ref()))
        .or_else(|| get_provider_env_value("AWS_DEFAULT_REGION", options.env.as_ref()))
}

fn is_gov_cloud_target(model: &Model, options: &StreamOptions) -> bool {
    if configured_region(options).is_some_and(|r| r.to_lowercase().starts_with("us-gov-")) {
        return true;
    }
    let id = model.id.to_lowercase();
    id.starts_with("us-gov.") || id.starts_with("arn:aws-us-gov:")
}

fn openai_gpt_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn openai_gpt_oss_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
}

fn budget_for(budgets: Option<&ThinkingBudgets>, level: ThinkingLevel) -> u64 {
    let custom = budgets.and_then(|b| match clamp_reasoning(Some(level)).unwrap_or(ThinkingLevel::High) {
        ThinkingLevel::Minimal => b.minimal,
        ThinkingLevel::Low => b.low,
        ThinkingLevel::Medium => b.medium,
        _ => b.high,
    });
    custom.unwrap_or(match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        _ => 16384,
    })
}

fn build_additional_model_request_fields(model: &Model, options: &StreamOptions) -> Option<Value> {
    let reasoning = options.reasoning?;
    if !model.reasoning {
        return None;
    }
    if is_anthropic_claude_model(model) {
        let gov_cloud = is_gov_cloud_target(model, options);
        let display = if gov_cloud { None } else { Some(options.extra_str("thinkingDisplay").unwrap_or("summarized").to_string()) };
        let block_binding = !gov_cloud && supports_thinking_block_binding(model);
        let adaptive = supports_adaptive_thinking(&model.id, Some(&model.name));
        let mut result = Map::new();
        if adaptive {
            let mut thinking = Map::new();
            thinking.insert("type".into(), json!("adaptive"));
            if let Some(display) = &display {
                thinking.insert("display".into(), json!(display));
            }
            if block_binding {
                thinking.insert("block_binding".into(), json!({ "prefix_mismatch_behavior": "drop_block" }));
            }
            result.insert("thinking".into(), Value::Object(thinking));
            result.insert("output_config".into(), json!({ "effort": map_thinking_level_to_effort(model, reasoning) }));
            if block_binding {
                result.insert("anthropic_beta".into(), json!([THINKING_BINDING_CONTROLS_BETA]));
            }
        } else {
            let mut thinking = Map::new();
            thinking.insert("type".into(), json!("enabled"));
            thinking.insert("budget_tokens".into(), json!(budget_for(options.thinking_budgets.as_ref(), reasoning)));
            if let Some(display) = &display {
                thinking.insert("display".into(), json!(display));
            }
            result.insert("thinking".into(), Value::Object(thinking));
        }
        if !adaptive && options.extra_bool("interleavedThinking").unwrap_or(true) {
            result.insert("anthropic_beta".into(), json!(["interleaved-thinking-2025-05-14"]));
        }
        return Some(Value::Object(result));
    }
    let candidates = model_match_candidates(&model.id, Some(&model.name));
    if candidates.iter().any(|s| s.contains("gpt-oss")) {
        return Some(json!({ "reasoning_effort": openai_gpt_oss_effort(reasoning) }));
    }
    if candidates.iter().any(|s| s.contains("gpt-")) {
        let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(&ModelThinkingLevel::from(reasoning))).cloned().flatten();
        return Some(json!({ "reasoning": { "effort": mapped.unwrap_or_else(|| openai_gpt_effort(reasoning).to_string()) } }));
    }
    None
}

/// Build the `ConverseStreamCommand` input.
pub fn build_command_input(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> anyhow::Result<Value> {
    let retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let max_tokens = options.max_tokens.or_else(|| is_anthropic_claude_model(model).then_some(model.max_tokens));
    let initial = get_initial_system_message(&context.messages);
    // `if (!systemPrompt)` sees the text before `sanitizeSurrogates`.
    let prompt = initial.filter(|m| !get_system_message_text_is_empty(m)).map(get_system_message_text);
    let supports_strict = model.compat.as_ref().and_then(|c| c.supports_strict_mode).unwrap_or(false);
    let mut input = Map::new();
    input.insert("modelId".into(), json!(model.id));
    input.insert("messages".into(), Value::Array(convert_messages(context, model, retention, options.env.as_ref())?));
    if let Some(system) = build_system_prompt(prompt.as_deref(), model, retention, options.env.as_ref()) {
        input.insert("system".into(), system);
    }
    let mut inference = Map::new();
    if let Some(max_tokens) = max_tokens {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        inference.insert("temperature".into(), crate::serde_util::js_number(temperature));
    }
    input.insert("inferenceConfig".into(), Value::Object(inference));
    let tool_choice = options
        .extra_value("toolChoice")
        .cloned()
        .or_else(|| options.tool_choice.and_then(|c| serde_json::to_value(c).ok()));
    if let Some(tool_config) = convert_tool_config(&get_current_tools(&context.messages), tool_choice.as_ref(), supports_strict)? {
        input.insert("toolConfig".into(), tool_config);
    }
    if let Some(fields) = build_additional_model_request_fields(model, options) {
        input.insert("additionalModelRequestFields".into(), fields);
    }
    if let Some(metadata) = options.extra_value("requestMetadata").filter(|v| !v.is_null()) {
        input.insert("requestMetadata".into(), metadata.clone());
    }
    Ok(Value::Object(input))
}

/// The JSON request body for a command input: known members in schema order, `modelId` moved to
/// the path, `null`s dropped.
pub fn command_body(input: &Value) -> Value {
    const MEMBERS: [&str; 12] = [
        "messages",
        "system",
        "inferenceConfig",
        "toolConfig",
        "guardrailConfig",
        "additionalModelRequestFields",
        "promptVariables",
        "additionalModelResponseFieldPaths",
        "requestMetadata",
        "performanceConfig",
        "serviceTier",
        "outputConfig",
    ];
    let mut body = Map::new();
    for member in MEMBERS {
        if let Some(value) = input.get(member).filter(|v| !v.is_null()) {
            body.insert(member.to_string(), value.clone());
        }
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// Client configuration
// ---------------------------------------------------------------------------

/// The resolved `BedrockRuntimeClientConfig` of one request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BedrockClientConfig {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    /// Explicit static credentials (`config.credentials`).
    pub credentials: Option<AwsCredentials>,
    /// Bedrock API key (`config.token` with `authSchemePreference: ["httpBearerAuth"]`).
    pub bearer_token: Option<String>,
    pub skip_auth: bool,
    pub force_http1: bool,
    /// Custom user agent pairs appended to the SDK user agent.
    pub custom_user_agent: Vec<(String, Option<String>)>,
    /// A custom user agent string (identity `{ custom }`).
    pub custom_user_agent_string: Option<String>,
}

fn standard_endpoint_region(base_url: &str) -> Option<String> {
    let host = url::Url::parse(base_url).ok()?.host_str()?.to_lowercase();
    let re = regex::Regex::new(r"^bedrock-runtime(?:-fips)?\.([a-z0-9-]+)\.amazonaws\.com(?:\.cn)?$").expect("valid regex");
    re.captures(&host).map(|c| c[1].to_string())
}

fn should_use_explicit_endpoint(base_url: &str, configured_region: Option<&str>, ambient_profile: bool) -> bool {
    if standard_endpoint_region(base_url).is_none() {
        return true;
    }
    configured_region.is_none() && !ambient_profile
}

fn bedrock_user_agent(identity: Option<&ClientIdentity>) -> (Vec<(String, Option<String>)>, Option<String>) {
    match identity.and_then(|i| i.user_agent.as_ref()) {
        Some(UserAgentMode::Custom { custom }) => (Vec::new(), Some(custom.clone())),
        Some(UserAgentMode::Named(UserAgentNamed::Short)) => (vec![("pi".into(), None)], None),
        _ => (vec![("pi".into(), Some(get_client_version(identity)))], None),
    }
}

/// Resolve the client configuration the TypeScript adapter builds for a request.
pub fn resolve_client_config(model: &Model, options: &StreamOptions) -> BedrockClientConfig {
    let options_profile = options
        .extra_str("profile")
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| options.env.as_ref().and_then(|e| e.get("AWS_PROFILE")).filter(|p| !p.is_empty()).cloned());
    let (custom_user_agent, custom_user_agent_string) = bedrock_user_agent(options.identity.as_ref());
    let mut config = BedrockClientConfig {
        profile: options_profile.clone().or_else(|| get_provider_env_value("AWS_PROFILE", options.env.as_ref())),
        custom_user_agent,
        custom_user_agent_string,
        ..Default::default()
    };
    let configured = configured_region(options);
    let ambient_profile = get_provider_env_value("AWS_PROFILE", None).is_some();
    let endpoint_region = standard_endpoint_region(&model.base_url);
    let explicit_endpoint = should_use_explicit_endpoint(&model.base_url, configured.as_deref(), ambient_profile);
    if explicit_endpoint {
        config.endpoint = Some(model.base_url.clone());
    }
    let skip_auth = get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", options.env.as_ref()).as_deref() == Some("1");
    let bearer = options
        .extra_str("bearerToken")
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| options.api_key.clone().filter(|k| !k.is_empty()))
        .or_else(|| get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", options.env.as_ref()));
    let arn_region = regex::Regex::new(r"^arn:aws(?:-[a-z0-9-]+)?:bedrock:([a-z0-9-]+):")
        .expect("valid regex")
        .captures(&model.id)
        .map(|c| c[1].to_string());
    config.region = if let Some(region) = arn_region {
        Some(region)
    } else if let Some(region) = configured {
        Some(region)
    } else if let (Some(region), true) = (endpoint_region, explicit_endpoint) {
        Some(region)
    } else if !ambient_profile {
        Some("us-east-1".into())
    } else {
        None
    };
    if skip_auth {
        config.skip_auth = true;
        config.credentials = Some(AwsCredentials::new("dummy-access-key", "dummy-secret-key", None));
    }
    let access = get_provider_env_value("AWS_ACCESS_KEY_ID", options.env.as_ref());
    let secret = get_provider_env_value("AWS_SECRET_ACCESS_KEY", options.env.as_ref());
    if let (false, Some(access), Some(secret), None) = (skip_auth, access, secret, &options_profile) {
        config.credentials =
            Some(AwsCredentials::new(access, secret, get_provider_env_value("AWS_SESSION_TOKEN", options.env.as_ref())));
    }
    config.force_http1 = get_provider_env_value("AWS_BEDROCK_FORCE_HTTP1", options.env.as_ref()).as_deref() == Some("1");
    if !skip_auth {
        config.bearer_token = bearer;
    }
    config
}

fn default_endpoint(region: &str) -> String {
    if let Some(endpoint) = super::aws_credentials::configured_endpoint("BEDROCK_RUNTIME") {
        return endpoint;
    }
    let fips = std::env::var("AWS_USE_FIPS_ENDPOINT").is_ok_and(|v| v.eq_ignore_ascii_case("true"));
    let dualstack = std::env::var("AWS_USE_DUALSTACK_ENDPOINT").is_ok_and(|v| v.eq_ignore_ascii_case("true"));
    let service = if fips { "bedrock-runtime-fips" } else { "bedrock-runtime" };
    let suffix = if region.starts_with("cn-") { "amazonaws.com.cn" } else { "amazonaws.com" };
    if dualstack {
        let dual_suffix = if region.starts_with("cn-") { "api.amazonwebservices.com.cn" } else { "api.aws" };
        return format!("https://{service}.{region}.{dual_suffix}");
    }
    format!("https://{service}.{region}.{suffix}")
}

/// `user-agent` and `x-amz-user-agent` of the `BedrockRuntimeClient` (`customUserAgent` from the
/// client identity; a custom string becomes a single-name section).
fn sdk_user_agent(config: &BedrockClientConfig, app_id: Option<&str>, credentials: Option<&AwsCredentials>) -> (String, String) {
    let mut custom = config.custom_user_agent.clone();
    if let Some(text) = &config.custom_user_agent_string {
        custom.push((text.clone(), None));
    }
    super::aws_credentials::aws_sdk_user_agent(AWS_SDK_VERSION, "Bedrock Runtime", &custom, app_id, credentials)
}

// ---------------------------------------------------------------------------
// HTTP and retries
// ---------------------------------------------------------------------------

pub(crate) const THROTTLING_CODES: [&str; 13] = [
    "BandwidthLimitExceeded",
    "EC2ThrottledException",
    "LimitExceededException",
    "PriorRequestNotComplete",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
    "RequestThrottled",
    "RequestThrottledException",
    "SlowDown",
    "ThrottledException",
    "ThrottlingException",
    "TooManyRequestsException",
    "TransactionInProgressException",
];
pub(crate) const TRANSIENT_CODES: [&str; 4] = ["TimeoutError", "RequestTimeout", "RequestTimeoutException", "InternalServerException"];
pub(crate) const TRANSIENT_STATUSES: [u16; 4] = [500, 502, 503, 504];

enum Attempt {
    Ok(reqwest::Response),
    /// `retry_after`: the response's retry-after hint (epoch ms).
    Failed { error: anyhow::Error, retryable: bool, throttling: bool, retry_after: Option<i64> },
}



fn parse_service_error(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> BedrockError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let sanitize = |code: &str| -> String {
        let code = code.split([',', ':']).next().unwrap_or(code);
        code.rsplit('#').next().unwrap_or(code).trim().to_string()
    };
    let header_code = headers.get("x-amzn-errortype").and_then(|v| v.to_str().ok()).map(sanitize);
    let body_code = parsed.as_ref().and_then(|p| {
        ["__type", "code", "Code"].iter().find_map(|k| p.get(*k).and_then(Value::as_str)).map(sanitize)
    });
    let code = header_code.or(body_code).filter(|c| !c.is_empty());
    let message = parsed
        .as_ref()
        .and_then(|p| ["message", "Message", "errorMessage"].iter().find_map(|k| p.get(*k).and_then(Value::as_str)))
        .map(str::to_string)
        .unwrap_or_else(|| "UnknownError".to_string());
    let request_id = headers.get("x-amzn-requestid").and_then(|v| v.to_str().ok()).map(str::to_string);
    BedrockError::Service {
        body: if code.is_none() { Some(body.to_string()) } else { None },
        name: code.unwrap_or_else(|| "Unknown".into()),
        message,
        status,
        request_id,
    }
}

struct PreparedRequest {
    url: url::Url,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn attempt_request(
    client: &reqwest::Client,
    prepared: &PreparedRequest,
    attempt: u32,
    max: u32,
    auth: &RequestAuth,
    region: &str,
    signal: Option<&CancellationToken>,
) -> Attempt {
    let mut headers = prepared.headers.clone();
    headers.push(("amz-sdk-request".into(), format!("attempt={attempt}; max={max}")));
    match auth {
        RequestAuth::Bearer(token) => headers.push(("authorization".into(), format!("Bearer {token}"))),
        RequestAuth::SigV4(credentials) => {
            let signed = sign_request(
                &SignableRequest { method: "POST", url: &prepared.url, headers: &headers, body: &prepared.body },
                credentials,
                region,
                "bedrock",
                crate::utils::now_ms(),
            );
            headers.extend(signed);
        }
    }
    let mut request = client.post(prepared.url.clone()).body(prepared.body.clone());
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }
        request = request.header(name.as_str(), value.as_str());
    }
    let send = request.send();
    let result = match signal {
        Some(signal) => tokio::select! {
            r = send => r,
            _ = signal.cancelled() => {
                return Attempt::Failed { error: anyhow::anyhow!("Request aborted"), retryable: false, throttling: false, retry_after: None };
            }
        },
        None => send.await,
    };
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            // The SDK's Node HTTP handler rejects with the socket error itself.
            let message = if error.is_timeout() {
                format!("Connection timed out: {error}")
            } else {
                crate::utils::http::NetworkError::from_reqwest(&error).to_string()
            };
            return Attempt::Failed { error: anyhow::anyhow!(message), retryable: true, throttling: false, retry_after: None };
        }
    };
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Attempt::Ok(response);
    }
    let headers = response.headers().clone();
    let retry_after = super::aws_credentials::retry_after_hint(&headers);
    let body = response.text().await.unwrap_or_default();
    let error = parse_service_error(status, &headers, &body);
    let name = match &error {
        BedrockError::Service { name, .. } => name.clone(),
        _ => String::new(),
    };
    let throttling = status == 429 || THROTTLING_CODES.contains(&name.as_str());
    let retryable = throttling || TRANSIENT_STATUSES.contains(&status) || TRANSIENT_CODES.contains(&name.as_str());
    Attempt::Failed { error: error.into(), retryable, throttling, retry_after }
}

enum RequestAuth {
    Bearer(String),
    SigV4(AwsCredentials),
}

async fn send_with_retries(
    client: &reqwest::Client,
    prepared: &PreparedRequest,
    auth: &RequestAuth,
    region: &str,
    profile: Option<&str>,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<reqwest::Response> {
    let max = super::aws_credentials::aws_max_attempts_for(profile)?;
    let mut quota = super::aws_credentials::SdkRetryQuota::new();
    let mut attempt = 1;
    loop {
        match attempt_request(client, prepared, attempt, max, auth, region, signal).await {
            Attempt::Ok(response) => return Ok(response),
            Attempt::Failed { error, retryable, throttling, retry_after } => {
                if !retryable
                    || attempt >= max
                    || signal.is_some_and(|s| s.is_cancelled())
                    || !quota.acquire(throttling)
                {
                    return Err(error);
                }
                let delay = super::aws_credentials::sdk_retry_delay_ms(attempt - 1, throttling, retry_after);
                let sleep = crate::utils::wasm::sleep(Duration::from_millis(delay));
                match signal {
                    Some(signal) => tokio::select! {
                        _ = sleep => {},
                        _ = signal.cancelled() => anyhow::bail!("Request aborted"),
                    },
                    None => sleep.await,
                }
                attempt += 1;
            }
        }
    }
}

type ByteStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Reads `ConverseStreamOutput` union members (`{ messageStart: {...} }`, ...) from an event
/// stream body.
struct ConverseEventReader {
    body: ByteStream,
    decoder: EventStreamDecoder,
    queue: std::collections::VecDeque<EventMessage>,
    done: bool,
    signal: Option<CancellationToken>,
}

const MODELED_STREAM_EXCEPTIONS: [&str; 5] = [
    "internalServerException",
    "modelStreamErrorException",
    "validationException",
    "throttlingException",
    "serviceUnavailableException",
];

impl ConverseEventReader {
    async fn next_item(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            if let Some(message) = self.queue.pop_front() {
                match message.header_str(":message-type") {
                    Some("event") => {
                        let event_type = message.header_str(":event-type").unwrap_or_default().to_string();
                        let payload: Value = if message.body.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_slice(&message.body)?
                        };
                        let mut item = Map::new();
                        item.insert(event_type, payload);
                        return Ok(Some(Value::Object(item)));
                    }
                    Some("exception") => {
                        let code = message.header_str(":exception-type").unwrap_or_default().to_string();
                        if MODELED_STREAM_EXCEPTIONS.contains(&code.as_str()) {
                            let payload: Value = serde_json::from_slice(&message.body).unwrap_or(json!({}));
                            return Err(BedrockError::StreamObject(payload).into());
                        }
                        return Err(BedrockError::Named { name: code, message: String::from_utf8_lossy(&message.body).into_owned() }.into());
                    }
                    Some("error") => {
                        let code = message.header_str(":error-code").unwrap_or_default().to_string();
                        let text = message.header_str(":error-message").filter(|m| !m.is_empty()).map(str::to_string).unwrap_or_else(|| code.clone());
                        return Err(BedrockError::Named { name: code, message: text }.into());
                    }
                    other => anyhow::bail!("Unrecognizable event type: {}", other.unwrap_or_default()),
                }
            }
            if self.done {
                return Ok(None);
            }
            let next = match &self.signal {
                Some(signal) => tokio::select! {
                    chunk = self.body.next() => chunk,
                    _ = signal.cancelled() => anyhow::bail!("Request aborted"),
                },
                None => self.body.next().await,
            };
            match next {
                Some(Ok(bytes)) => self.queue.extend(self.decoder.push(&bytes)?),
                Some(Err(error)) => anyhow::bail!("{error}"),
                None => {
                    self.done = true;
                    if self.decoder.remaining() > 0 {
                        anyhow::bail!("Truncated event message received.");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

struct BlockState {
    /// Bedrock `contentBlockIndex` per output block (`None` once stopped).
    index: Vec<Option<i64>>,
    partial_json: std::collections::HashMap<usize, String>,
    redacted_chunks: std::collections::HashMap<usize, Vec<u8>>,
}

impl BlockState {
    fn find(&self, index: i64) -> Option<usize> {
        self.index.iter().position(|i| *i == Some(index))
    }
}

fn flush_redacted(state: &mut BlockState, output: &mut AssistantMessage, position: usize) {
    if let Some(chunks) = state.redacted_chunks.remove(&position)
        && let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position)
    {
        block.thinking_signature = Some(BASE64.encode(chunks));
    }
}

fn finalize_blocks(state: &mut BlockState, output: &mut AssistantMessage) {
    for position in 0..output.content.len() {
        flush_redacted(state, output, position);
    }
    state.partial_json.clear();
}

fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("end_turn" | "stop_sequence") => (StopReason::Stop, None),
        Some("max_tokens" | "model_context_window_exceeded") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some(other) if !other.is_empty() => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
        _ => (StopReason::Error, None),
    }
}

fn handle_item(item: &Value, model: &Model, output: &mut AssistantMessage, sender: &AssistantMessageEventSender, state: &mut BlockState) -> anyhow::Result<()> {
    if let Some(start) = item.get("messageStart") {
        if start.get("role").and_then(Value::as_str) != Some("assistant") {
            anyhow::bail!("Unexpected assistant message start but got user message start instead");
        }
        sender.push(AssistantMessageEvent::Start { partial: output.clone() });
    } else if let Some(event) = item.get("contentBlockStart") {
        let index = event.get("contentBlockIndex").and_then(Value::as_i64);
        if let Some(tool_use) = event.get("start").and_then(|s| s.get("toolUse")) {
            output.content.push(AssistantContent::ToolCall(ToolCall {
                id: tool_use.get("toolUseId").and_then(Value::as_str).unwrap_or_default().to_string(),
                name: tool_use.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                ..Default::default()
            }));
            state.index.push(index);
            state.partial_json.insert(output.content.len() - 1, String::new());
            sender.push(AssistantMessageEvent::ToolCallStart { content_index: output.content.len() - 1, partial: output.clone() });
        }
    } else if let Some(event) = item.get("contentBlockDelta") {
        let block_index = event.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
        let delta = &event["delta"];
        let mut position = state.find(block_index);
        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            if position.is_none() {
                output.content.push(AssistantContent::Text(TextContent::new("")));
                state.index.push(Some(block_index));
                position = Some(output.content.len() - 1);
                sender.push(AssistantMessageEvent::TextStart { content_index: output.content.len() - 1, partial: output.clone() });
            }
            let position = position.unwrap_or_default();
            if let Some(AssistantContent::Text(block)) = output.content.get_mut(position) {
                block.text.push_str(text);
                sender.push(AssistantMessageEvent::TextDelta { content_index: position, delta: text.to_string(), partial: output.clone() });
            }
        } else if let (Some(tool_use), Some(position)) = (delta.get("toolUse"), position) {
            if matches!(output.content.get(position), Some(AssistantContent::ToolCall(_))) {
                let input = tool_use.get("input").and_then(Value::as_str).unwrap_or_default().to_string();
                let partial = state.partial_json.entry(position).or_default();
                partial.push_str(&input);
                let parsed = parse_streaming_json_object(Some(partial));
                if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position) {
                    block.arguments = parsed;
                }
                sender.push(AssistantMessageEvent::ToolCallDelta { content_index: position, delta: input, partial: output.clone() });
            }
        } else if let Some(reasoning) = delta.get("reasoningContent") {
            let position = match position {
                Some(position) => position,
                None => {
                    output.content.push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: Some(String::new()),
                        redacted: None,
                        ..Default::default()
                    }));
                    state.index.push(Some(block_index));
                    sender.push(AssistantMessageEvent::ThinkingStart { content_index: output.content.len() - 1, partial: output.clone() });
                    output.content.len() - 1
                }
            };
            if !matches!(output.content.get(position), Some(AssistantContent::Thinking(_))) {
                return Ok(());
            }
            if let Some(text) = reasoning.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                    block.thinking.push_str(text);
                }
                sender.push(AssistantMessageEvent::ThinkingDelta { content_index: position, delta: text.to_string(), partial: output.clone() });
            }
            if let Some(signature) = reasoning.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty())
                && let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position)
                && block.redacted != Some(true)
            {
                block.thinking_signature.get_or_insert_with(String::new).push_str(signature);
            }
            let redacted = reasoning
                .get("redactedContent")
                .and_then(Value::as_str)
                .and_then(|b| BASE64.decode(b).ok())
                .filter(|b| !b.is_empty());
            if let Some(bytes) = redacted {
                let mut first = false;
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position)
                    && block.redacted != Some(true)
                {
                    block.redacted = Some(true);
                    block.thinking_signature = Some(String::new());
                    block.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                    first = true;
                }
                if first {
                    sender.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: position,
                        delta: REDACTED_THINKING_PLACEHOLDER.to_string(),
                        partial: output.clone(),
                    });
                }
                state.redacted_chunks.entry(position).or_default().extend(bytes);
            }
        }
    } else if let Some(event) = item.get("contentBlockStop") {
        let block_index = event.get("contentBlockIndex").and_then(Value::as_i64).unwrap_or(0);
        let Some(position) = state.find(block_index) else { return Ok(()) };
        state.index[position] = None;
        flush_redacted(state, output, position);
        match output.content.get_mut(position) {
            Some(AssistantContent::Text(block)) => {
                let content = block.text.clone();
                sender.push(AssistantMessageEvent::TextEnd { content_index: position, content, partial: output.clone() });
            }
            Some(AssistantContent::Thinking(block)) => {
                let content = block.thinking.clone();
                sender.push(AssistantMessageEvent::ThinkingEnd { content_index: position, content, partial: output.clone() });
            }
            Some(AssistantContent::ToolCall(block)) => {
                let partial = state.partial_json.remove(&position).unwrap_or_default();
                let parsed = parse_tool_call_arguments(Some(&partial));
                block.arguments = parsed.arguments;
                block.arguments_error = parsed.arguments_error;
                let tool_call = block.clone();
                sender.push(AssistantMessageEvent::ToolCallEnd { content_index: position, tool_call, partial: output.clone() });
            }
            None => {}
        }
    } else if let Some(event) = item.get("messageStop") {
        let reason = event.get("stopReason").and_then(Value::as_str);
        output.raw_stop_reason = reason.map(str::to_string);
        let (stop_reason, error) = map_stop_reason(reason);
        output.stop_reason = stop_reason;
        if error.is_some() {
            output.error_message = error;
        }
    } else if let Some(event) = item.get("metadata") {
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            output.usage.input = count("inputTokens");
            output.usage.output = count("outputTokens");
            output.usage.cache_read = count("cacheReadInputTokens");
            output.usage.cache_write = count("cacheWriteInputTokens");
            output.usage.cache_write_1h = usage.get("cacheDetails").and_then(Value::as_array).map(|details| {
                details
                    .iter()
                    .filter(|d| d.get("ttl").and_then(Value::as_str) == Some("1h"))
                    .map(|d| d.get("inputTokens").and_then(Value::as_u64).unwrap_or(0))
                    .sum()
            });
            let total = count("totalTokens");
            output.usage.total_tokens = if total > 0 { total } else { output.usage.input + output.usage.output };
            calculate_cost(&model.cost, &mut output.usage);
        }
    } else {
        for key in MODELED_STREAM_EXCEPTIONS {
            if let Some(exception) = item.get(key) {
                return Err(BedrockError::StreamObject(exception.clone()).into());
            }
        }
    }
    Ok(())
}

async fn run_stream(model: Model, context: TranscriptContext, options: StreamOptions, sender: AssistantMessageEventSender) {
    let context = collapse_system_messages(&context);
    let mut output = AssistantMessage {
        api: "bedrock-converse-stream".into(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let mut state = BlockState { index: Vec::new(), partial_json: Default::default(), redacted_chunks: Default::default() };
    let config = resolve_client_config(&model, &options);
    let mut response_request_id: Option<String> = None;
    let result: anyhow::Result<()> = async {
        let mut input = build_command_input(&model, &context, &options)?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(input.clone(), model.clone()).await
        {
            input = next;
        }
        if options.is_aborted() {
            anyhow::bail!("Request aborted");
        }
        let region = match config.region.clone().or_else(|| profile_region(config.profile.as_deref())) {
            Some(region) => region,
            None => anyhow::bail!("Region is missing"),
        };
        let endpoint = config.endpoint.clone().unwrap_or_else(|| default_endpoint(&region));
        let model_id = input.get("modelId").and_then(Value::as_str).unwrap_or(&model.id).to_string();
        let url = url::Url::parse(&format!("{}/model/{}/converse-stream", endpoint.trim_end_matches('/'), escape_uri(&model_id)))?;
        #[cfg(target_arch = "wasm32")]
        let client = reqwest::Client::builder().build()?; // The host's `fetch` owns proxies and HTTP versions.
        #[cfg(not(target_arch = "wasm32"))]
        let client = {
            let proxy = crate::utils::node_http_proxy::resolve_http_proxy_url_for_target(&model.base_url, options.env.as_ref())?;
            let mut builder = reqwest::Client::builder().no_proxy();
            if let Some(proxy) = proxy {
                builder = builder.proxy(reqwest::Proxy::all(proxy.as_str())?);
            }
            if config.force_http1 {
                builder = builder.http1_only();
            }
            builder.build()?
        };
        let app_id = super::aws_credentials::user_agent_app_id(config.profile.as_deref());
        let auth = if let Some(token) = &config.bearer_token {
            RequestAuth::Bearer(token.clone())
        } else if let Some(credentials) = &config.credentials {
            // `config.credentials` given as an object: the SDK attributes it to code (`e`).
            let mut credentials = credentials.clone();
            if credentials.features.is_empty() {
                credentials.set_feature("CREDENTIALS_CODE", "e");
            }
            RequestAuth::SigV4(credentials)
        } else {
            let context = CredentialContext {
                client: client.clone(),
                region: Some(region.clone()),
                app_id: app_id.clone(),
                role_assumer_region: None,
            };
            RequestAuth::SigV4(default_provider(config.profile.as_deref(), &context).await?)
        };
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let signing = match &auth {
            RequestAuth::SigV4(credentials) => Some(credentials),
            RequestAuth::Bearer(_) => None,
        };
        let (user_agent, x_amz_user_agent) = sdk_user_agent(&config, app_id.as_deref(), signing);
        let mut headers: Vec<(String, String)> = vec![
            ("content-type".into(), "application/json".into()),
            ("x-amz-user-agent".into(), x_amz_user_agent),
            ("user-agent".into(), user_agent),
            ("host".into(), host),
            ("amz-sdk-invocation-id".into(), uuid::Uuid::new_v4().to_string()),
        ];
        if let Some(custom) = provider_headers_to_record(&[options.headers.as_ref()]) {
            for (name, value) in custom {
                let lower = name.to_lowercase();
                if lower.starts_with("x-amz-") || lower == "authorization" || lower == "host" {
                    continue;
                }
                headers.retain(|(n, _)| *n != name);
                headers.push((name, value));
            }
        }
        let prepared = PreparedRequest { url, headers, body: serde_json::to_vec(&command_body(&input))? };
        let response = send_with_retries(&client, &prepared, &auth, &region, config.profile.as_deref(), options.signal.as_ref()).await?;
        response_request_id = normalize_diagnostic_value(response.headers().get("x-amzn-requestid").and_then(|v| v.to_str().ok()));
        if let Some(on_response) = &options.on_response {
            on_response(ProviderResponse { status: response.status().as_u16(), headers: headers_to_record(response.headers()) }, model.clone()).await;
        }
        let mut reader = ConverseEventReader {
            body: Box::pin(crate::utils::wasm::assert_send(response.bytes_stream())),
            decoder: EventStreamDecoder::new(),
            queue: Default::default(),
            done: false,
            signal: options.signal.clone(),
        };
        while let Some(item) = reader.next_item().await? {
            if let Some(observer) = &options.on_provider_stream_event {
                observer(item.clone(), model.clone()).await;
            }
            handle_item(&item, &model, &mut output, &sender, &mut state)?;
        }
        if options.is_aborted() {
            anyhow::bail!("Request was aborted");
        }
        match output.stop_reason {
            StopReason::Pending => anyhow::bail!("Bedrock stream ended without a stop reason"),
            StopReason::Error | StopReason::Aborted => {
                anyhow::bail!("{}", output.error_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "An unknown error occurred".into()))
            }
            _ => {}
        }
        finalize_blocks(&mut state, &mut output);
        sender.push(AssistantMessageEvent::Done { reason: output.stop_reason, message: output.clone() });
        sender.end(None);
        Ok(())
    }
    .await;
    if let Err(error) = result {
        finalize_blocks(&mut state, &mut output);
        output.stop_reason = if options.is_aborted() { StopReason::Aborted } else { StopReason::Error };
        output.error_message = Some(format_bedrock_error(&error));
        if output.stop_reason == StopReason::Error {
            append_failure_diagnostic(&mut output, &error, response_request_id.as_deref());
        }
        sender.push(AssistantMessageEvent::Error { reason: output.stop_reason, error: output.clone() });
        sender.end(None);
    }
}

/// Stream a Bedrock Converse response.
pub fn stream(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let mut base = build_base_options(model, context, &options, None);
    base.extra = options.extra.clone();
    if let Some(choice) = options.tool_choice.and_then(|c| serde_json::to_value(c).ok()) {
        base.extra.insert("toolChoice".into(), choice);
    }
    let Some(reasoning) = options.reasoning else {
        base.reasoning = None;
        return stream(model, context, base);
    };
    base.reasoning = Some(reasoning);
    base.thinking_budgets = options.thinking_budgets.clone();
    if is_anthropic_claude_model(model) && !supports_adaptive_thinking(&model.id, Some(&model.name)) {
        let (max_tokens, thinking_budget) =
            adjust_max_tokens_for_thinking(base.max_tokens, model.max_tokens, reasoning, options.thinking_budgets.as_ref());
        let max_tokens = clamp_max_tokens_to_context(model, context, max_tokens);
        let budget = thinking_budget.min(max_tokens.saturating_sub(1024));
        let mut budgets = options.thinking_budgets.clone().unwrap_or_default();
        match clamp_reasoning(Some(reasoning)).unwrap_or(ThinkingLevel::High) {
            ThinkingLevel::Minimal => budgets.minimal = Some(budget),
            ThinkingLevel::Low => budgets.low = Some(budget),
            ThinkingLevel::Medium => budgets.medium = Some(budget),
            _ => budgets.high = Some(budget),
        }
        base.max_tokens = Some(max_tokens);
        base.thinking_budgets = Some(budgets);
    }
    stream(model, context, base)
}

/// The `bedrock-converse-stream` API implementation.
pub struct BedrockConverseStreamApi;

impl ProviderStreams for BedrockConverseStreamApi {
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
    fn endpoint_regions() {
        assert_eq!(standard_endpoint_region("https://bedrock-runtime.eu-central-1.amazonaws.com").as_deref(), Some("eu-central-1"));
        assert_eq!(standard_endpoint_region("https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com").as_deref(), Some("us-gov-west-1"));
        assert_eq!(standard_endpoint_region("https://bedrock-vpc.example.com"), None);
    }

    #[test]
    fn service_errors() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-amzn-errortype", "ValidationException:http://internal.amazon.com/coral/".parse().unwrap());
        headers.insert("x-amzn-requestid", "req-1".parse().unwrap());
        let error = parse_service_error(400, &headers, r#"{"message":"bad model"}"#);
        assert_eq!(format_bedrock_error(&error.clone().into()), "Validation error: bad model");
        let gateway = parse_service_error(403, &reqwest::header::HeaderMap::new(), "<html>denied</html>");
        assert_eq!(format_bedrock_error(&gateway.into()), "Unknown: 403: <html>denied</html>");
        let stream = BedrockError::StreamObject(json!({ "message": "Too many requests, please wait." }));
        assert_eq!(format_bedrock_error(&stream.into()), r#"{"message":"Too many requests, please wait."}"#);
    }

    #[test]
    fn stop_reasons() {
        assert_eq!(map_stop_reason(Some("model_context_window_exceeded")).0, StopReason::Length);
        assert_eq!(map_stop_reason(Some("guardrail_intervened")).1.as_deref(), Some("Provider stopped with: guardrail_intervened"));
        assert_eq!(map_stop_reason(None), (StopReason::Error, None));
    }
}
