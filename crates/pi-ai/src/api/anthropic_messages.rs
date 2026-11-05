//! Anthropic Messages API (port of `api/anthropic-messages.ts`).
//!
//! Implemented directly against the HTTP API (no SDK). Requests mirror what the Anthropic
//! TypeScript SDK sends: `POST {baseUrl}/v1/messages?beta=true`, `anthropic-version`,
//! `x-api-key` / `Authorization`, `anthropic-beta` from `betas`, and the Stainless headers.
//!
//! API-specific options are read from `StreamOptions.extra`: `thinkingEnabled`,
//! `thinkingBudgetTokens`, `effort`, `thinkingDisplay`, `interleavedThinking`, `toolChoice`.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::constrained_sampling::{get_json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use super::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use super::simple_options::{adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context};
use super::transform_messages::{split_error_tool_result_images, transform_messages};
use super::anthropic_federation::{AnthropicFederationConfig, federation_token_cache, with_oauth_beta};
use crate::models::{ProviderStreams, calculate_cost};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, ConstrainedSamplingConfig, ImageContent,
    JsonSchemaStrictness, Message, Model, ProviderEnv, ProviderHeaders, ProviderResponse, StopReason, StreamOptions,
    TextContent, ThinkingContent, ThinkingLevel, Tool, ToolCall, ToolResultMessage, TranscriptContext,
    UserContent, UserContentBlock, is_blank_text,
};
use crate::utils::diagnostics::{AssistantMessageDiagnostic, DiagnosticErrorInfo, append_assistant_message_diagnostic};
use crate::utils::error_body::{ProviderHttpError, SdkErrorStyle};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::headers::headers_to_record;
use crate::utils::http::{HeaderList, client_for, post_json, stainless_headers};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::json_parse::{parse_json_with_repair, parse_streaming_json_object, parse_tool_call_arguments};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{ProviderRetryOptions, retry_provider_request};
use crate::utils::sse::sse_reader;
use crate::utils::text::{
    get_system_message_text, get_system_message_text_is_empty, render_system_message_update,
    render_system_message_update_is_empty,
};
use crate::utils::transcript::{get_current_tools, get_initial_system_message, resolve_transcript};

/// Version of `@anthropic-ai/sdk` whose request shape this module reproduces.
pub const ANTHROPIC_SDK_VERSION: &str = "0.129.0";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

// Stealth mode: mimic Claude Code's tool naming exactly.
const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";

const ANTHROPIC_MESSAGE_EVENTS: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

fn to_claude_code_name(name: &str) -> String {
    let lower = name.to_lowercase();
    CLAUDE_CODE_TOOLS.iter().find(|t| t.to_lowercase() == lower).map(|t| t.to_string()).unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    let lower = name.to_lowercase();
    tools.iter().find(|t| t.name.to_lowercase() == lower).map(|t| t.name.clone()).unwrap_or_else(|| name.to_string())
}

fn deferred_tool_placeholder() -> Value {
    json!({
        "name": "__pi_deferred_placeholder__",
        "description": "Reserved placeholder. Never available. Never call this.",
        "input_schema": { "type": "object", "properties": {}, "required": [] },
        "defer_loading": true
    })
}

/// Resolved Anthropic compatibility flags.
#[derive(Debug, Clone)]
struct AnthropicCompat {
    supports_eager_tool_input_streaming: bool,
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    session_affinity_openrouter: bool,
    supports_cache_control_on_tools: bool,
    supports_temperature: bool,
    allow_empty_signature: bool,
    supports_strict_tools: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_changes: bool,
}

fn get_anthropic_compat(model: &Model) -> AnthropicCompat {
    let compat = model.compat();
    let is_open_router = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
    AnthropicCompat {
        supports_eager_tool_input_streaming: compat.supports_eager_tool_input_streaming.unwrap_or(true),
        supports_long_cache_retention: compat.supports_long_cache_retention.unwrap_or(true),
        send_session_affinity_headers: compat.send_session_affinity_headers.unwrap_or(is_open_router),
        session_affinity_openrouter: match compat.session_affinity_format {
            Some(f) => f == crate::types::SessionAffinityFormat::Openrouter,
            None => is_open_router,
        },
        supports_cache_control_on_tools: compat.supports_cache_control_on_tools.unwrap_or(true),
        supports_temperature: compat.supports_temperature.unwrap_or(true),
        allow_empty_signature: compat.allow_empty_signature.unwrap_or(false),
        supports_strict_tools: compat.supports_strict_tools.unwrap_or(false),
        supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages.unwrap_or(false),
        supports_mid_convo_tool_changes: compat.supports_mid_convo_tool_changes.unwrap_or(false),
    }
}

/// Resolve cache retention preference: defaults to short; `PI_CACHE_RETENTION=long` opts in.
pub fn resolve_cache_retention(cache_retention: Option<CacheRetention>, env: Option<&ProviderEnv>) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

fn get_cache_control(model: &Model, cache_retention: Option<CacheRetention>, env: Option<&ProviderEnv>) -> Option<Value> {
    let retention = resolve_cache_retention(cache_retention, env);
    if retention == CacheRetention::None {
        return None;
    }
    let mut control = Map::new();
    control.insert("type".into(), json!("ephemeral"));
    if retention == CacheRetention::Long && get_anthropic_compat(model).supports_long_cache_retention {
        control.insert("ttl".into(), json!("1h"));
    }
    Some(Value::Object(control))
}

/// Anthropic adaptive-thinking effort.
pub const ANTHROPIC_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn is_anthropic_effort(value: Option<&str>) -> bool {
    value.is_some_and(|v| ANTHROPIC_EFFORTS.contains(&v))
}

/// Typed view of the Anthropic-specific options carried in `StreamOptions.extra`.
#[derive(Debug, Clone, Default)]
struct AnthropicOptions {
    thinking_enabled: Option<bool>,
    thinking_budget_tokens: Option<u64>,
    effort: Option<String>,
    thinking_display: Option<String>,
    interleaved_thinking: Option<bool>,
    tool_choice: Option<Value>,
}

impl AnthropicOptions {
    fn from(options: &StreamOptions) -> Self {
        let tool_choice = options.extra_value("toolChoice").cloned().or_else(|| {
            options.tool_choice.map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        });
        Self {
            thinking_enabled: options.extra_bool("thinkingEnabled"),
            thinking_budget_tokens: options.extra_u64("thinkingBudgetTokens"),
            effort: options.extra_str("effort").map(str::to_string),
            thinking_display: options.extra_str("thinkingDisplay").map(str::to_string),
            interleaved_thinking: options.extra_bool("interleavedThinking"),
            tool_choice: tool_choice.filter(|v| !v.is_null()),
        }
    }
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    headers.is_some_and(|h| {
        h.iter().any(|(k, v)| k.eq_ignore_ascii_case(name) && v.as_ref().is_some_and(|v| !v.trim().is_empty()))
    })
}

fn has_request_auth(api_key: Option<&str>, headers: Option<&ProviderHeaders>) -> bool {
    api_key.is_some_and(|k| !k.is_empty())
        || has_header(headers, "authorization")
        || has_header(headers, "x-api-key")
        || has_header(headers, "cf-aig-authorization")
}

/// Workload identity federation config from the ANTHROPIC_* variables the Anthropic SDK documents.
/// Only for the anthropic provider (the exchange is an Anthropic API endpoint), and only when no
/// key or auth header was resolved.
fn get_anthropic_federation(
    model: &Model,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
    env: Option<&ProviderEnv>,
) -> Option<AnthropicFederationConfig> {
    if model.provider != "anthropic" || has_request_auth(api_key, headers) {
        return None;
    }
    AnthropicFederationConfig::from_env(env)
}

fn has_federation(model: &Model, api_key: Option<&str>, headers: Option<&ProviderHeaders>, env: Option<&ProviderEnv>) -> bool {
    get_anthropic_federation(model, api_key, headers, env).is_some()
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

/// Normalize tool call IDs to Anthropic's `^[a-zA-Z0-9_-]+$` (max 64).
pub fn normalize_tool_call_id(id: &str) -> String {
    let units: Vec<u16> = id
        .encode_utf16()
        .map(|u| {
            let c = u as u32;
            let ok = (c < 128) && ((c as u8).is_ascii_alphanumeric() || c == b'_' as u32 || c == b'-' as u32);
            if ok { u } else { b'_' as u16 }
        })
        .take(64)
        .collect();
    String::from_utf16_lossy(&units)
}

fn convert_image_block(image: &ImageContent) -> Value {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": image.mime_type, "data": image.data }
    })
}

/// Convert content blocks: text-only joins into a string; otherwise a block array.
fn convert_content_blocks(content: &[UserContentBlock]) -> Value {
    let has_images = content.iter().any(|c| c.as_image().is_some());
    if !has_images {
        return Value::String(
            content.iter().filter_map(|c| c.as_text()).map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n"),
        );
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .map(|block| match block {
            UserContentBlock::Text(t) => json!({ "type": "text", "text": t.text }),
            UserContentBlock::Image(i) => convert_image_block(i),
        })
        .collect();
    if !blocks.iter().any(|b| b["type"] == "text") {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

fn convert_tool_result(msg: &ToolResultMessage, content: Option<&[TextContent]>) -> Value {
    let content_value = match content {
        Some(texts) => {
            let blocks: Vec<UserContentBlock> = texts.iter().cloned().map(UserContentBlock::Text).collect();
            convert_content_blocks(&blocks)
        }
        None => convert_content_blocks(&msg.content),
    };
    json!({
        "type": "tool_result",
        "tool_use_id": msg.tool_call_id,
        "content": content_value,
        "is_error": msg.is_error
    })
}

fn is_anthropic_strict_unsupported_keyword(key: &str, value: &Value) -> bool {
    const UNSUPPORTED: [&str; 11] = [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "maxItems",
        "uniqueItems",
        "minContains",
        "maxContains",
        "minProperties",
        "maxProperties",
    ];
    const FORMATS: [&str; 10] = ["date-time", "time", "date", "duration", "email", "hostname", "uri", "ipv4", "ipv6", "uuid"];
    if UNSUPPORTED.contains(&key) {
        return true;
    }
    if key == "minItems" {
        return !(value == &json!(0) || value == &json!(1) || value.as_f64() == Some(0.0) || value.as_f64() == Some(1.0));
    }
    if key == "format" {
        return !value.as_str().is_some_and(|f| FORMATS.contains(&f));
    }
    false
}

fn is_preferred_json_schema(tool: &Tool) -> bool {
    matches!(
        tool.constrained_sampling.as_ref().and_then(|c| c.config()),
        Some(ConstrainedSamplingConfig::JsonSchema { strict: JsonSchemaStrictness::Prefer })
    )
}

fn convert_tools(
    tools: &[Tool],
    is_oauth: bool,
    supports_eager: bool,
    supports_strict: bool,
    cache_control: Option<&Value>,
    disable_preferred_strict: bool,
) -> anyhow::Result<Vec<Value>> {
    let mut result = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let strict = if disable_preferred_strict && is_preferred_json_schema(tool) {
            None
        } else {
            resolve_json_schema_strict_sampling(tool, supports_strict, Some(is_anthropic_strict_unsupported_keyword))?
        };
        let parameters = get_json_schema_tool_parameters(tool, strict);
        let properties = parameters.get("properties").cloned().unwrap_or_else(|| json!({}));
        let required = parameters.get("required").cloned().unwrap_or_else(|| json!([]));
        let input_schema = if strict == Some(true) {
            let mut merged = parameters.as_object().cloned().unwrap_or_default();
            merged.insert("type".into(), json!("object"));
            merged.insert("properties".into(), properties);
            merged.insert("required".into(), required);
            Value::Object(merged)
        } else {
            json!({ "type": "object", "properties": properties, "required": required })
        };
        let mut definition = Map::new();
        definition.insert("name".into(), json!(if is_oauth { to_claude_code_name(&tool.name) } else { tool.name.clone() }));
        definition.insert("description".into(), json!(tool.description));
        if supports_eager {
            definition.insert("eager_input_streaming".into(), json!(true));
        }
        if strict == Some(true) {
            definition.insert("strict".into(), json!(true));
        }
        definition.insert("input_schema".into(), input_schema);
        if let Some(cache_control) = cache_control
            && index == tools.len() - 1 {
                definition.insert("cache_control".into(), cache_control.clone());
            }
        result.push(Value::Object(definition));
    }
    Ok(result)
}

struct ConvertedMessages {
    messages: Vec<Value>,
    assistant_levels: HashMap<usize, String>,
}

type ToolDefinitionConverter<'a> = &'a dyn Fn(&[Tool]) -> anyhow::Result<Vec<Value>>;

fn convert_messages(
    messages: &[Message],
    is_oauth: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    managed_provider: Option<&str>,
    convert_tool_definitions: Option<ToolDefinitionConverter>,
) -> anyhow::Result<ConvertedMessages> {
    let mut params: Vec<Value> = Vec::new();
    let mut assistant_levels = HashMap::new();
    let mut pending_system: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(system) => {
                let text = render_system_message_update(system);
                let mut blocks: Vec<Value> = Vec::new();
                if !render_system_message_update_is_empty(system) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                if let Some(convert) = convert_tool_definitions {
                    let added = system.tools_added.clone().unwrap_or_default();
                    let redefined: HashSet<&str> = added.iter().map(|t| t.name.as_str()).collect();
                    for tool in system.tools_removed.iter().flatten() {
                        if redefined.contains(tool.name.as_str()) {
                            continue;
                        }
                        let name = if is_oauth { to_claude_code_name(&tool.name) } else { tool.name.clone() };
                        blocks.push(json!({ "type": "tool_removal", "tool": { "type": "tool_reference", "name": name } }));
                    }
                    for definition in convert(&added)? {
                        blocks.push(json!({ "type": "tool_addition", "tool": { "type": "tool_definition", "definition": definition } }));
                    }
                }
                if !blocks.is_empty() {
                    pending_system.push(json!({ "role": "system", "content": blocks }));
                }
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !is_blank_text(text, user.lone_surrogates_removed) {
                        params.push(json!({ "role": "user", "content": text }));
                    }
                }
                UserContent::Blocks(blocks) => {
                    let converted: Vec<Value> = blocks
                        .iter()
                        .filter(|b| match b {
                            UserContentBlock::Text(t) => !t.text.trim().is_empty(),
                            UserContentBlock::Image(_) => true,
                        })
                        .map(|b| match b {
                            UserContentBlock::Text(t) => json!({ "type": "text", "text": t.text }),
                            UserContentBlock::Image(img) => convert_image_block(img),
                        })
                        .collect();
                    if !converted.is_empty() {
                        params.push(json!({ "role": "user", "content": converted }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                params.append(&mut pending_system);
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) => {
                            if t.is_blank() {
                                continue;
                            }
                            blocks.push(json!({ "type": "text", "text": t.text }));
                        }
                        AssistantContent::Thinking(t) => {
                            if t.redacted == Some(true) {
                                blocks.push(json!({ "type": "redacted_thinking", "data": t.thinking_signature.clone().unwrap_or_default() }));
                                continue;
                            }
                            let signature = t.thinking_signature.clone().unwrap_or_default();
                            let has_signature = !signature.trim().is_empty();
                            if t.is_blank() && !has_signature {
                                continue;
                            }
                            if !has_signature {
                                if allow_empty_signature {
                                    blocks.push(json!({ "type": "thinking", "thinking": t.thinking, "signature": "" }));
                                } else {
                                    blocks.push(json!({ "type": "text", "text": t.thinking }));
                                }
                            } else {
                                blocks.push(json!({ "type": "thinking", "thinking": t.thinking, "signature": signature }));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            let name = if is_oauth { to_claude_code_name(&call.name) } else { call.name.clone() };
                            blocks.push(json!({ "type": "tool_use", "id": call.id, "name": name, "input": call.arguments }));
                        }
                    }
                }
                if blocks.is_empty() {
                    i += 1;
                    continue;
                }
                let message_index = params.len();
                params.push(json!({ "role": "assistant", "content": blocks }));
                if let Some(provider) = managed_provider
                    && assistant.api == "anthropic-messages"
                        && assistant.provider == provider
                        && is_anthropic_effort(assistant.provider_thinking_level.as_deref())
                    {
                        assistant_levels.insert(message_index, assistant.provider_thinking_level.clone().unwrap_or_default());
                    }
            }
            Message::ToolResult(_) => {
                let mut tool_results: Vec<Value> = Vec::new();
                let mut error_images: Vec<Value> = Vec::new();
                let mut j = i;
                while let Some(Message::ToolResult(tool_msg)) = messages.get(j) {
                    let split = split_error_tool_result_images(tool_msg);
                    tool_results.push(convert_tool_result(tool_msg, split.as_ref().map(|s| s.content.as_slice())));
                    if let Some(split) = split {
                        error_images.push(json!({ "type": "text", "text": split.caption }));
                        error_images.extend(split.images.iter().map(convert_image_block));
                    }
                    j += 1;
                }
                i = j - 1;
                tool_results.extend(error_images);
                params.push(json!({ "role": "user", "content": tool_results }));
            }
        }
        i += 1;
    }
    params.append(&mut pending_system);

    if let (Some(cache_control), Some(last)) = (cache_control, params.last_mut()) {
        let role = last["role"].as_str().unwrap_or_default().to_string();
        if role == "user" || role == "system" {
            match last.get_mut("content") {
                Some(Value::Array(blocks)) => {
                    if let Some(Value::Object(block)) = blocks.last_mut() {
                        let kind = block.get("type").and_then(Value::as_str).unwrap_or_default();
                        if matches!(kind, "text" | "image" | "tool_result" | "tool_addition" | "tool_removal") {
                            block.insert("cache_control".into(), cache_control.clone());
                        }
                    }
                }
                Some(Value::String(text)) => {
                    let text = std::mem::take(text);
                    last["content"] = json!([{ "type": "text", "text": text, "cache_control": cache_control }]);
                }
                _ => {}
            }
        }
    }
    Ok(ConvertedMessages { messages: params, assistant_levels })
}

fn insert_thinking_level_messages(converted: ConvertedMessages, active_effort: &str) -> Vec<Value> {
    let mut messages = Vec::with_capacity(converted.messages.len() + 1);
    for (index, message) in converted.messages.into_iter().enumerate() {
        if let Some(effort) = converted.assistant_levels.get(&index) {
            messages.push(json!({ "role": "system", "content": [], "output_config": { "effort": effort } }));
        }
        messages.push(message);
    }
    messages.push(json!({ "role": "system", "content": [], "output_config": { "effort": active_effort } }));
    messages
}

fn get_beta_features(
    model: &Model,
    context: &TranscriptContext,
    is_oauth: bool,
    native_tool_changes: bool,
    options: &StreamOptions,
    anthropic: &AnthropicOptions,
) -> Vec<String> {
    let mut configured: Option<Option<String>> = None;
    let model_headers: Option<ProviderHeaders> =
        model.headers.as_ref().map(|h| h.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect());
    for headers in [model_headers.as_ref(), options.headers.as_ref()].into_iter().flatten() {
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("anthropic-beta") {
                configured = Some(value.clone());
            }
        }
    }
    match configured {
        Some(None) => return Vec::new(),
        Some(Some(value)) => {
            let mut seen = Vec::new();
            for feature in value.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                if !seen.iter().any(|s: &String| s == feature) {
                    seen.push(feature.to_string());
                }
            }
            return seen;
        }
        None => {}
    }
    let compat = model.compat();
    let mut features: Vec<String> = Vec::new();
    if is_oauth {
        features.push("claude-code-20250219".into());
        features.push("oauth-2025-04-20".into());
    }
    if !get_current_tools(&context.messages).is_empty() && !get_anthropic_compat(model).supports_eager_tool_input_streaming {
        features.push(FINE_GRAINED_TOOL_STREAMING_BETA.into());
    }
    if model.reasoning
        && anthropic.thinking_enabled == Some(true)
        && anthropic.interleaved_thinking.unwrap_or(true)
        && compat.force_adaptive_thinking != Some(true)
    {
        features.push(INTERLEAVED_THINKING_BETA.into());
    }
    if compat.allowed_fallback_models.as_ref().is_some_and(|f| !f.is_empty()) {
        features.push(SERVER_SIDE_FALLBACK_BETA.into());
    }
    if compat.supports_mid_convo_effort == Some(true) {
        features.push(MID_CONVERSATION_OUTPUT_CONFIG_BETA.into());
        features.push(THINKING_BINDING_CONTROLS_BETA.into());
    }
    if native_tool_changes {
        features.push(INLINE_TOOLS_BETA.into());
    }
    let mut unique: Vec<String> = Vec::new();
    for f in features {
        if !unique.contains(&f) {
            unique.push(f);
        }
    }
    unique
}

struct BuiltParams {
    params: Value,
    has_preferred_strict_tools: bool,
    provider_thinking_level: Option<String>,
}

fn build_params(
    model: &Model,
    context: &TranscriptContext,
    is_oauth: bool,
    options: &StreamOptions,
    anthropic: &AnthropicOptions,
    disable_preferred_strict_tools: bool,
) -> anyhow::Result<BuiltParams> {
    let cache_control = get_cache_control(model, options.cache_retention, options.env.as_ref());
    let compat = get_anthropic_compat(model);
    let model_compat = model.compat();
    let initial_system = get_initial_system_message(&context.messages);
    let initial_system_text = initial_system.map(get_system_message_text).unwrap_or_default();
    let initial_system_empty = initial_system.is_none_or(get_system_message_text_is_empty);
    let normalize = |id: &str, _: &Model, _: &AssistantMessage| normalize_tool_call_id(id);
    let transformed = transform_messages(&context.messages, model, Some(&normalize));
    let conversation: &[Message] = if initial_system.is_some() && !transformed.is_empty() { &transformed[1..] } else { &transformed };

    let has_preferred = std::cell::Cell::new(false);
    let convert_tool_definitions = |tools: &[Tool], tool_cache_control: Option<&Value>| -> anyhow::Result<Vec<Value>> {
        let definitions = convert_tools(
            tools,
            is_oauth,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            tool_cache_control,
            disable_preferred_strict_tools,
        )?;
        if definitions.iter().enumerate().any(|(index, d)| d.get("strict") == Some(&json!(true)) && is_preferred_json_schema(&tools[index])) {
            has_preferred.set(true);
        }
        Ok(definitions)
    };
    let initial_tools: Vec<Tool> = initial_system.and_then(|s| s.tools_added.clone()).unwrap_or_default();
    let native_tool_changes =
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_changes && !initial_tools.is_empty();
    let off_map = model.thinking_level_map.as_ref().and_then(|m| m.get(&crate::types::ModelThinkingLevel::Off));
    let thinking_disabled = model.reasoning && anthropic.thinking_enabled == Some(false) && !matches!(off_map, Some(None));
    let effort_messages = model_compat.supports_mid_convo_effort == Some(true) && !thinking_disabled;

    let addition_converter = |tools: &[Tool]| convert_tool_definitions(tools, None);
    let converted = convert_messages(
        conversation,
        is_oauth,
        cache_control.as_ref(),
        compat.allow_empty_signature,
        if effort_messages { Some(model.provider.as_str()) } else { None },
        if native_tool_changes { Some(&addition_converter) } else { None },
    )?;
    let active_effort = anthropic.effort.clone().unwrap_or_else(|| "high".to_string());
    let betas = get_beta_features(model, context, is_oauth, native_tool_changes, options, anthropic);

    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert(
        "messages".into(),
        Value::Array(if effort_messages { insert_thinking_level_messages(converted, &active_effort) } else { converted.messages }),
    );
    params.insert("max_tokens".into(), json!(options.max_tokens.unwrap_or(model.max_tokens)));
    params.insert("stream".into(), json!(true));
    if !betas.is_empty() {
        params.insert("betas".into(), json!(betas));
    }

    let with_cache = |text: &str| {
        let mut block = Map::new();
        block.insert("type".into(), json!("text"));
        block.insert("text".into(), json!(text));
        if let Some(cc) = &cache_control {
            block.insert("cache_control".into(), cc.clone());
        }
        Value::Object(block)
    };
    if is_oauth {
        let mut system = vec![with_cache("You are Claude Code, Anthropic's official CLI for Claude.")];
        if !initial_system_empty {
            system.push(with_cache(&initial_system_text));
        }
        params.insert("system".into(), Value::Array(system));
    } else if !initial_system_empty {
        params.insert("system".into(), Value::Array(vec![with_cache(&initial_system_text)]));
    }

    if let Some(temperature) = options.temperature
        && anthropic.thinking_enabled != Some(true)
            && model_compat.supports_mid_convo_effort != Some(true)
            && compat.supports_temperature
        {
            params.insert("temperature".into(), crate::serde_util::js_number(temperature));
        }

    let tool_cache_control = if compat.supports_cache_control_on_tools { cache_control.as_ref() } else { None };
    if native_tool_changes {
        let mut tools = convert_tool_definitions(&initial_tools, tool_cache_control)?;
        tools.push(deferred_tool_placeholder());
        params.insert("tools".into(), Value::Array(tools));
    } else {
        let tools = get_current_tools(&context.messages);
        if !tools.is_empty() {
            params.insert("tools".into(), Value::Array(convert_tool_definitions(&tools, tool_cache_control)?));
        }
    }

    let display = anthropic.thinking_display.clone().unwrap_or_else(|| "summarized".to_string());
    if thinking_disabled {
        params.insert("thinking".into(), json!({ "type": "disabled" }));
    } else if effort_messages {
        params.insert(
            "thinking".into(),
            json!({ "type": "adaptive", "display": display, "block_binding": { "prefix_mismatch_behavior": "drop_block" } }),
        );
        // Effort markers set the level for every reply. Keep the cached top-level effort
        // fixed at low because organizations can cap effort and reject higher levels.
        params.insert("output_config".into(), json!({ "effort": "low" }));
    } else if model.reasoning && anthropic.thinking_enabled == Some(true) {
        if model_compat.force_adaptive_thinking == Some(true) {
            params.insert("thinking".into(), json!({ "type": "adaptive", "display": display }));
            if let Some(effort) = &anthropic.effort {
                params.insert("output_config".into(), json!({ "effort": effort }));
            }
        } else {
            let budget = anthropic.thinking_budget_tokens.filter(|b| *b > 0).unwrap_or(1024);
            params.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": budget, "display": display }));
        }
    }

    if let Some(metadata) = &options.metadata
        && let Some(Value::String(user_id)) = metadata.get("user_id") {
            params.insert("metadata".into(), json!({ "user_id": user_id }));
        }

    if let Some(tool_choice) = &anthropic.tool_choice {
        match tool_choice {
            Value::String(kind) => {
                params.insert("tool_choice".into(), json!({ "type": kind }));
            }
            other => {
                params.insert("tool_choice".into(), other.clone());
            }
        }
    }

    if let Some(fallbacks) = model_compat.allowed_fallback_models.as_ref().filter(|f| !f.is_empty()) {
        params.insert(
            "fallbacks".into(),
            Value::Array(fallbacks.iter().map(|f| json!({ "model": f.model })).collect()),
        );
    }

    Ok(BuiltParams {
        params: Value::Object(params),
        has_preferred_strict_tools: has_preferred.get(),
        provider_thinking_level: if effort_messages { Some(active_effort) } else { None },
    })
}

/// Map an Anthropic stop reason.
fn map_stop_reason(reason: &str, stop_details: Option<&Value>) -> anyhow::Result<(StopReason, Option<String>)> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => (
            StopReason::Error,
            Some(
                stop_details
                    .and_then(|d| d.get("explanation"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| "The model refused to complete the request".to_string()),
            ),
        ),
        "sensitive" => (StopReason::Error, Some("Provider stopped with: sensitive".to_string())),
        other => anyhow::bail!("Unhandled stop reason: {other}"),
    })
}

/// Request target, auth and headers for one request (the TS `createClient`).
struct ClientConfig {
    headers: HeaderList,
    is_oauth: bool,
}

fn build_client_config(
    model: &Model,
    api_key: Option<&str>,
    options: &StreamOptions,
    dynamic_headers: Option<&indexmap::IndexMap<String, String>>,
    session_id: Option<&str>,
    timeout_ms: u64,
) -> ClientConfig {
    let target = ClientIdentityTarget { provider: &model.provider, base_url: &model.base_url };
    let mut headers = HeaderList::new();
    headers.set("Accept", Some("application/json"));
    headers.set("User-Agent", Some(&format!("Anthropic/JS {ANTHROPIC_SDK_VERSION}")));
    for (name, value) in stainless_headers(ANTHROPIC_SDK_VERSION, timeout_ms) {
        headers.set(&name, Some(&value));
    }
    headers.set("anthropic-version", Some(ANTHROPIC_VERSION));

    let model_headers = model.headers.clone().unwrap_or_default();
    if model.provider == "github-copilot" {
        if let Some(key) = api_key {
            headers.set("Authorization", Some(&format!("Bearer {key}")));
        }
        headers.extend_strings(&get_identity_headers(target, options.identity.as_ref()));
        headers.set("accept", Some("application/json"));
        headers.set("anthropic-dangerous-direct-browser-access", Some("true"));
        headers.extend_strings(&model_headers);
        if let Some(dynamic) = dynamic_headers {
            headers.extend_strings(dynamic);
        }
        if let Some(h) = &options.headers {
            headers.extend_optional(h);
        }
        return ClientConfig { headers, is_oauth: false };
    }

    if let Some(key) = api_key.filter(|k| is_oauth_token(k)) {
        headers.set("Authorization", Some(&format!("Bearer {key}")));
        headers.set("accept", Some("application/json"));
        headers.set("anthropic-dangerous-direct-browser-access", Some("true"));
        headers.set("user-agent", Some(&format!("claude-cli/{CLAUDE_CODE_VERSION}")));
        headers.set("x-app", Some("cli"));
        headers.extend_strings(&model_headers);
        if let Some(h) = &options.headers {
            headers.extend_optional(h);
        }
        return ClientConfig { headers, is_oauth: true };
    }

    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        headers.set("X-Api-Key", Some(key));
    }
    let compat = get_anthropic_compat(model);
    headers.extend_strings(&get_identity_headers(target, options.identity.as_ref()));
    headers.set("accept", Some("application/json"));
    headers.set("anthropic-dangerous-direct-browser-access", Some("true"));
    if let Some(session_id) = session_id.filter(|s| !s.is_empty())
        && compat.send_session_affinity_headers {
            let header = if compat.session_affinity_openrouter { "x-session-id" } else { "x-session-affinity" };
            headers.set(header, Some(session_id));
        }
    headers.extend_strings(&model_headers);
    if let Some(h) = &options.headers {
        headers.extend_optional(h);
    }
    ClientConfig { headers, is_oauth: false }
}

fn messages_url(base_url: &str) -> String {
    format!("{}/v1/messages?beta=true", base_url.trim_end_matches('/'))
}

struct BlockMeta {
    index: Option<i64>,
    partial_json: String,
}

fn find_block(meta: &[BlockMeta], index: i64) -> Option<usize> {
    meta.iter().position(|m| m.index == Some(index))
}

fn u64_of(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
}

async fn run_stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let compat = get_anthropic_compat(&model);
    let normalized = resolve_transcript(&context, Some(compat.supports_mid_convo_system_messages));
    let current_tools = get_current_tools(&normalized.messages);
    let anthropic = AnthropicOptions::from(&options);
    let mut output = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let mut meta: Vec<BlockMeta> = Vec::new();
    let signal = options.signal.clone();
    let aborted = || signal.as_ref().is_some_and(|s| s.is_cancelled());

    let result: anyhow::Result<()> = async {
        let api_key = options.api_key.clone().filter(|k| !k.is_empty());
        let federation = get_anthropic_federation(&model, api_key.as_deref(), options.headers.as_ref(), options.env.as_ref());
        if federation.is_none() && !has_request_auth(api_key.as_deref(), options.headers.as_ref()) {
            anyhow::bail!("No API key for provider: {}", model.provider);
        }
        let dynamic_headers = if model.provider == "github-copilot" {
            Some(build_copilot_dynamic_headers(&normalized.messages, has_copilot_vision_input(&normalized.messages)))
        } else {
            None
        };
        let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
        let cache_session_id = if cache_retention == CacheRetention::None { None } else { options.session_id.as_deref() };
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let client_config =
            build_client_config(&model, api_key.as_deref(), &options, dynamic_headers.as_ref(), cache_session_id, timeout_ms);
        let is_oauth = client_config.is_oauth;
        let url = messages_url(&model.base_url);
        let client = match &options.http_client {
            Some(client) => client.clone(),
            None => client_for(&url, options.env.as_ref())?,
        };
        // The SDK exchanges the identity token itself, caches the access token per federation
        // client, and appends the OAuth beta to requests it authenticates.
        let token_cache = federation.as_ref().map(|config| {
            federation_token_cache(config, &model.base_url, client.clone(), format!("Anthropic/JS {ANTHROPIC_SDK_VERSION}"))
        });

        let mut disable_preferred_strict = false;
        let response = loop {
            // `signal.throwIfAborted()` throws the reason of an `abort()` without one.
            if aborted() {
                anyhow::bail!("This operation was aborted");
            }
            let built = build_params(&model, &normalized, is_oauth, &options, &anthropic, disable_preferred_strict)?;
            if let Some(level) = &built.provider_thinking_level {
                output.provider_thinking_level = Some(level.clone());
            }
            let mut params = built.params;
            if let Some(on_payload) = &options.on_payload
                && let Some(next) = on_payload(params.clone(), model.clone()).await {
                    params = next;
                    if let Value::Object(map) = &mut params {
                        map.insert("stream".into(), json!(true));
                    }
                }
            let mut headers = client_config.headers.clone();
            if let Value::Object(map) = &mut params
                && let Some(betas) = map.shift_remove("betas") {
                    let list: Vec<String> = match betas {
                        Value::Array(items) => items.iter().filter_map(|b| b.as_str().map(str::to_string)).collect(),
                        Value::String(s) => vec![s],
                        _ => vec![],
                    };
                    if !list.is_empty() {
                        headers.set("anthropic-beta", Some(&list.join(",")));
                    }
                }
            headers.set("content-type", Some("application/json"));
            let retry = ProviderRetryOptions {
                max_retries: options.max_retries,
                max_retry_delay_ms: options.max_retry_delay_ms,
                signal: signal.clone(),
                no_retry_statuses: Vec::new(),
            };
            let attempt = retry_provider_request(
                || {
                    let mut headers = headers.clone();
                    let (client, url, params, signal, token_cache) = (&client, &url, &params, signal.as_ref(), token_cache.clone());
                    async move {
                        if let Some(cache) = &token_cache {
                            let token = cache.get_token().await?;
                            headers.set("Authorization", Some(&format!("Bearer {token}")));
                            let beta = with_oauth_beta(headers.get("anthropic-beta"));
                            headers.set("anthropic-beta", Some(&beta));
                        }
                        let result =
                            post_json(client, url, &headers, params, Some(timeout_ms), signal, SdkErrorStyle::Anthropic).await;
                        if let (Some(cache), Err(error)) = (&token_cache, &result)
                            && error.downcast_ref::<ProviderHttpError>().is_some_and(|e| e.status == Some(401))
                        {
                            // The server rejected the token although it looked fresh.
                            cache.invalidate();
                        }
                        result
                    }
                },
                &retry,
            )
            .await;
            match attempt {
                Ok(response) => break response,
                Err(error) => {
                    let http = error.downcast_ref::<ProviderHttpError>();
                    let is_strict_limit = http.is_some_and(|e| {
                        e.status == Some(400)
                            && e.error.as_ref().and_then(|b| b.get("error")).and_then(|e| e.get("type")).and_then(Value::as_str)
                                == Some("invalid_request_error")
                            && [
                                "The compiled grammar is too large",
                                "Too many strict tools (",
                                "Schemas contains too many parameters with union types (",
                            ]
                            .iter()
                            .any(|m| e.message.contains(m))
                    });
                    if aborted() || disable_preferred_strict || !built.has_preferred_strict_tools || !is_strict_limit {
                        return Err(error);
                    }
                    disable_preferred_strict = true;
                    let mut details = Map::new();
                    details.insert("reason".into(), json!("strict_tool_limits_exceeded"));
                    append_assistant_message_diagnostic(
                        &mut output.diagnostics,
                        AssistantMessageDiagnostic {
                            diagnostic_type: "anthropic_strict_tools_fallback".into(),
                            timestamp: crate::utils::now_ms(),
                            error: Some(DiagnosticErrorInfo { message: error.to_string(), ..Default::default() }),
                            details: Some(details),
                        },
                    );
                }
            }
        };

        if let Some(on_response) = &options.on_response {
            on_response(
                ProviderResponse { status: response.status().as_u16(), headers: headers_to_record(response.headers()) },
                model.clone(),
            )
            .await;
        }
        sender.push(AssistantMessageEvent::Start { partial: output.clone() });

        let mut usage_cost = model.cost.clone();
        let mut input_transformations: Option<Vec<Value>> = None;
        let mut saw_message_start = false;
        let mut saw_message_end = false;
        let mut reader = sse_reader(response, signal.clone());
        while let Some(sse) = reader.next_event().await? {
            if sse.event.as_deref() == Some("error") {
                anyhow::bail!("{}", sse.data);
            }
            let event_name = sse.event.clone().unwrap_or_default();
            if !ANTHROPIC_MESSAGE_EVENTS.contains(&event_name.as_str()) {
                continue;
            }
            let event = parse_json_with_repair(&sse.data).map_err(|e| {
                anyhow::anyhow!(
                    "Could not parse Anthropic SSE event {}: {e}; data={}; raw={}",
                    event_name,
                    sse.data,
                    sse.raw.join("\\n")
                )
            })?;
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
            if event_type == "message_start" {
                saw_message_start = true;
            } else if event_type == "message_stop" {
                saw_message_end = true;
            }
            if let Some(observer) = &options.on_provider_stream_event {
                observer(event.clone(), model.clone()).await;
            }
            match event_type.as_str() {
                "message_start" => {
                    let message = &event["message"];
                    if let Some(id) = message.get("id").and_then(Value::as_str) {
                        output.response_id = Some(id.to_string());
                    }
                    if let Some(Value::Array(t)) = message.get("input_transformations") {
                        input_transformations = Some(t.clone());
                    }
                    let response_model = message.get("model").and_then(Value::as_str).map(str::to_string);
                    if let Some(response_model) = &response_model
                        && *response_model != model.id {
                            output.response_model = Some(response_model.clone());
                        }
                    usage_cost = match &response_model {
                        Some(rm) if *rm != model.id => model
                            .compat
                            .as_ref()
                            .and_then(|c| c.allowed_fallback_models.as_ref())
                            .and_then(|f| f.iter().find(|f| f.provider == model.provider && f.model == *rm))
                            .map(|f| f.cost.clone())
                            .unwrap_or_else(|| model.cost.clone()),
                        _ => model.cost.clone(),
                    };
                    let usage = &message["usage"];
                    output.usage.input = u64_of(usage.get("input_tokens")).unwrap_or(0);
                    output.usage.output = u64_of(usage.get("output_tokens")).unwrap_or(0);
                    output.usage.cache_read = u64_of(usage.get("cache_read_input_tokens")).unwrap_or(0);
                    output.usage.cache_write = u64_of(usage.get("cache_creation_input_tokens")).unwrap_or(0);
                    output.usage.cache_write_1h =
                        Some(u64_of(usage.get("cache_creation").and_then(|c| c.get("ephemeral_1h_input_tokens"))).unwrap_or(0));
                    output.usage.total_tokens =
                        output.usage.input + output.usage.output + output.usage.cache_read + output.usage.cache_write;
                    calculate_cost(&usage_cost, &mut output.usage);
                }
                "content_block_start" => {
                    let block = &event["content_block"];
                    let index = event.get("index").and_then(Value::as_i64);
                    match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "fallback" => {
                            if !output.content.is_empty() {
                                anyhow::bail!("Anthropic performed an unsupported mid-output model fallback");
                            }
                            continue;
                        }
                        "text" => {
                            output.content.push(AssistantContent::Text(TextContent::new(
                                block.get("text").and_then(Value::as_str).unwrap_or_default(),
                            )));
                            meta.push(BlockMeta { index, partial_json: String::new() });
                            sender.push(AssistantMessageEvent::TextStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            });
                        }
                        "thinking" => {
                            output.content.push(AssistantContent::Thinking(ThinkingContent {
                                thinking: block.get("thinking").and_then(Value::as_str).unwrap_or_default().to_string(),
                                thinking_signature: Some(
                                    block.get("signature").and_then(Value::as_str).unwrap_or_default().to_string(),
                                ),
                                redacted: None,
                                ..Default::default()
                            }));
                            meta.push(BlockMeta { index, partial_json: String::new() });
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            });
                        }
                        "redacted_thinking" => {
                            output.content.push(AssistantContent::Thinking(ThinkingContent {
                                thinking: "[Reasoning redacted]".to_string(),
                                thinking_signature: block.get("data").and_then(Value::as_str).map(str::to_string),
                                redacted: Some(true),
                                ..Default::default()
                            }));
                            meta.push(BlockMeta { index, partial_json: String::new() });
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            });
                        }
                        "tool_use" => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
                            output.content.push(AssistantContent::ToolCall(ToolCall {
                                id: block.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                                name: if is_oauth { from_claude_code_name(name, &current_tools) } else { name.to_string() },
                                arguments: block.get("input").and_then(Value::as_object).cloned().unwrap_or_default(),
                                ..Default::default()
                            }));
                            meta.push(BlockMeta { index, partial_json: String::new() });
                            sender.push(AssistantMessageEvent::ToolCallStart {
                                content_index: output.content.len() - 1,
                                partial: output.clone(),
                            });
                        }
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let Some(index) = event.get("index").and_then(Value::as_i64) else { continue };
                    let Some(position) = find_block(&meta, index) else { continue };
                    let delta = &event["delta"];
                    match delta.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "text_delta" => {
                            let text = delta.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                            if let Some(AssistantContent::Text(block)) = output.content.get_mut(position) {
                                block.text.push_str(&text);
                                sender.push(AssistantMessageEvent::TextDelta {
                                    content_index: position,
                                    delta: text,
                                    partial: output.clone(),
                                });
                            }
                        }
                        "thinking_delta" => {
                            let text = delta.get("thinking").and_then(Value::as_str).unwrap_or_default().to_string();
                            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                                block.thinking.push_str(&text);
                                sender.push(AssistantMessageEvent::ThinkingDelta {
                                    content_index: position,
                                    delta: text,
                                    partial: output.clone(),
                                });
                            }
                        }
                        "input_json_delta" => {
                            let partial = delta.get("partial_json").and_then(Value::as_str).unwrap_or_default().to_string();
                            if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position) {
                                meta[position].partial_json.push_str(&partial);
                                block.arguments = parse_streaming_json_object(Some(&meta[position].partial_json));
                                sender.push(AssistantMessageEvent::ToolCallDelta {
                                    content_index: position,
                                    delta: partial,
                                    partial: output.clone(),
                                });
                            }
                        }
                        "signature_delta" => {
                            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                                let signature = delta.get("signature").and_then(Value::as_str).unwrap_or_default();
                                block.thinking_signature.get_or_insert_with(String::new).push_str(signature);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let Some(index) = event.get("index").and_then(Value::as_i64) else { continue };
                    let Some(position) = find_block(&meta, index) else { continue };
                    meta[position].index = None;
                    match output.content.get_mut(position) {
                        Some(AssistantContent::Text(block)) => {
                            let content = block.text.clone();
                            sender.push(AssistantMessageEvent::TextEnd { content_index: position, content, partial: output.clone() });
                        }
                        Some(AssistantContent::Thinking(block)) => {
                            let content = block.thinking.clone();
                            sender.push(AssistantMessageEvent::ThinkingEnd {
                                content_index: position,
                                content,
                                partial: output.clone(),
                            });
                        }
                        Some(AssistantContent::ToolCall(block)) => {
                            let parsed = parse_tool_call_arguments(Some(&meta[position].partial_json));
                            block.arguments = parsed.arguments;
                            block.arguments_error = parsed.arguments_error;
                            meta[position].partial_json.clear();
                            let tool_call = block.clone();
                            sender.push(AssistantMessageEvent::ToolCallEnd {
                                content_index: position,
                                tool_call,
                                partial: output.clone(),
                            });
                        }
                        None => {}
                    }
                }
                "message_delta" => {
                    if let Some(Value::Array(t)) = event.get("input_transformations") {
                        input_transformations = Some(t.clone());
                    }
                    let delta = &event["delta"];
                    if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                        output.raw_stop_reason = Some(reason.to_string());
                        let (stop_reason, error_message) = map_stop_reason(reason, delta.get("stop_details"))?;
                        output.stop_reason = stop_reason;
                        if error_message.is_some() {
                            output.error_message = error_message;
                        }
                    }
                    if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                        if let Some(v) = u64_of(usage.get("input_tokens")) {
                            output.usage.input = v;
                        }
                        if let Some(v) = u64_of(usage.get("output_tokens")) {
                            output.usage.output = v;
                        }
                        if let Some(v) = u64_of(usage.get("cache_read_input_tokens")) {
                            output.usage.cache_read = v;
                        }
                        if let Some(v) = u64_of(usage.get("cache_creation_input_tokens")) {
                            output.usage.cache_write = v;
                        }
                        if let Some(v) = u64_of(usage.get("cache_creation").and_then(|c| c.get("ephemeral_1h_input_tokens"))) {
                            output.usage.cache_write_1h = Some(v);
                        }
                        if let Some(v) = u64_of(usage.get("output_tokens_details").and_then(|d| d.get("thinking_tokens"))) {
                            output.usage.reasoning = Some(v);
                        }
                    }
                    output.usage.total_tokens =
                        output.usage.input + output.usage.output + output.usage.cache_read + output.usage.cache_write;
                    calculate_cost(&usage_cost, &mut output.usage);
                }
                _ => {}
            }
        }
        if saw_message_start && !saw_message_end {
            anyhow::bail!("Anthropic stream ended before message_stop");
        }
        if aborted() {
            anyhow::bail!("Request was aborted");
        }
        match output.stop_reason {
            StopReason::Pending => anyhow::bail!("Anthropic stream ended without a stop reason"),
            StopReason::Aborted | StopReason::Error => {
                anyhow::bail!("{}", output.error_message.clone().unwrap_or_else(|| "An unknown error occurred".into()))
            }
            _ => {}
        }
        if let Some(transformations) = input_transformations.filter(|t| !t.is_empty()) {
            let mapped: Vec<Value> = transformations
                .iter()
                .map(|t| {
                    let mut entry = Map::new();
                    for key in ["type", "path", "reason"] {
                        if let Some(v) = t.get(key).filter(|v| !v.is_null()) {
                            entry.insert(key.into(), v.clone());
                        }
                    }
                    Value::Object(entry)
                })
                .collect();
            let mut details = Map::new();
            details.insert("transformations".into(), Value::Array(mapped));
            append_assistant_message_diagnostic(
                &mut output.diagnostics,
                AssistantMessageDiagnostic {
                    diagnostic_type: "anthropic_input_transformations".into(),
                    timestamp: crate::utils::now_ms(),
                    error: None,
                    details: Some(details),
                },
            );
        }
        sender.push(AssistantMessageEvent::Done { reason: output.stop_reason, message: output.clone() });
        sender.end(None);
        Ok(())
    }
    .await;

    if let Err(error) = result {
        output.stop_reason = if aborted() { StopReason::Aborted } else { StopReason::Error };
        output.error_message = Some(error_message_of(&error));
        sender.push(AssistantMessageEvent::Error { reason: output.stop_reason, error: output.clone() });
        sender.end(None);
    }
}

/// Error text as surfaced on the assistant message (`error.message`): an abort before the
/// response is `provider-retry`'s "Request aborted", the abort check of the SSE loop is "Request
/// was aborted", and an abort during a body read is undici's "This operation was aborted".
fn error_message_of(error: &anyhow::Error) -> String {
    error.to_string()
}

/// Stream an Anthropic Messages response.
pub fn stream(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Map a pi thinking level to an Anthropic effort.
fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&crate::types::ModelThinkingLevel::from(level)))
        .and_then(|v| v.clone());
    if let Some(mapped) = mapped {
        return mapped;
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .to_string()
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let federation = has_federation(model, options.api_key.as_deref(), options.headers.as_ref(), options.env.as_ref());
    if !federation && !has_request_auth(options.api_key.as_deref(), options.headers.as_ref()) {
        return super::lazy::error_stream(model, anyhow::anyhow!("No API key for provider: {}", model.provider));
    }
    let mut base = build_base_options(model, context, &options, options.api_key.as_deref());
    if let Some(choice) = options.tool_choice {
        base.extra.insert("toolChoice".into(), serde_json::to_value(choice).unwrap_or(Value::Null));
    }
    let Some(reasoning) = options.reasoning else {
        return stream(model, context, base.with_extra("thinkingEnabled", false));
    };
    if model.compat.as_ref().and_then(|c| c.force_adaptive_thinking) == Some(true) {
        let effort = map_thinking_level_to_effort(model, reasoning);
        return stream(model, context, base.with_extra("thinkingEnabled", true).with_extra("effort", effort));
    }
    let (max_tokens, thinking_budget) =
        adjust_max_tokens_for_thinking(base.max_tokens, model.max_tokens, reasoning, options.thinking_budgets.as_ref());
    let max_tokens = clamp_max_tokens_to_context(model, context, max_tokens);
    let budget = thinking_budget.min(max_tokens.saturating_sub(1024));
    let mut base = base.with_extra("thinkingEnabled", true).with_extra("thinkingBudgetTokens", budget);
    base.max_tokens = Some(max_tokens);
    stream(model, context, base)
}

/// The `anthropic-messages` API implementation.
pub struct AnthropicMessagesApi;

impl ProviderStreams for AnthropicMessagesApi {
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
    fn normalizes_tool_call_ids() {
        assert_eq!(normalize_tool_call_id("call|abc.def"), "call_abc_def");
        assert_eq!(normalize_tool_call_id(&"x".repeat(80)).len(), 64);
    }

    #[test]
    fn maps_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn", None).unwrap().0, StopReason::Stop);
        assert_eq!(map_stop_reason("tool_use", None).unwrap().0, StopReason::ToolUse);
        let (reason, message) = map_stop_reason("refusal", None).unwrap();
        assert_eq!(reason, StopReason::Error);
        assert_eq!(message.as_deref(), Some("The model refused to complete the request"));
        assert!(map_stop_reason("weird", None).is_err());
    }
}
