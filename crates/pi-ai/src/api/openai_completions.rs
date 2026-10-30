//! OpenAI Chat Completions API (port of `api/openai-completions.ts`).
//!
//! Implemented directly against the HTTP API: `POST {baseUrl}/chat/completions` with the
//! headers the OpenAI TypeScript SDK sends. API-specific options are read from
//! `StreamOptions.extra`: `toolChoice`, `reasoningEffort`.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use super::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use super::simple_options::{build_base_options, clamp_thinking_budget_to_answer_room, resolve_sampling_params, thinking_budget_for_level};
use super::transform_messages::transform_messages;
use crate::models::{ProviderStreams, calculate_cost, clamp_thinking_level};
use crate::providers::compat_schema::{
    CacheControlFormat, ChatTemplateKwargValue, ChatTemplateVar, MaxTokensField, OpenRouterRouting, SessionAffinityFormat,
    ThinkingFormat, ThinkingTokenBudgetField, VercelGatewayRouting,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Message, Model, ModelInputModality,
    ModelThinkingLevel, ProviderHeaders, ProviderResponse, StopReason, StreamOptions, TextContent, ThinkingContent,
    ThinkingLevel, Tool, ToolCall, TranscriptContext, Usage, UserContent, UserContentBlock, is_blank_text,
};
use crate::utils::error_body::{SdkErrorStyle, format_provider_error, normalize_provider_error};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::hash::short_hash;
use crate::utils::headers::headers_to_record;
use crate::utils::http::{HeaderList, apply_openai_custom_headers_env, client_for, post_json};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::json_parse::{parse_streaming_json_object, parse_tool_call_arguments};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{ProviderRetryOptions, retry_provider_request};
use crate::utils::text::{
    get_system_message_text, get_system_message_text_is_empty, render_system_message_update,
    render_system_message_update_is_empty,
};
use crate::utils::transcript::{TranscriptToolSupport, get_declared_tools, resolve_transcript, resolve_transcript_tools};

/// Version of the `openai` SDK whose request shape this module reproduces.
pub const OPENAI_SDK_VERSION: &str = "7.19.0";
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// Clamp a prompt cache key to 64 code points.
pub fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    let key = key?;
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH {
        return Some(key.to_string());
    }
    Some(chars[..OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH].iter().collect())
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    headers.is_some_and(|h| {
        h.iter().any(|(k, v)| k.eq_ignore_ascii_case(name) && v.as_ref().is_some_and(|v| !v.trim().is_empty()))
    })
}

fn get_client_api_key(provider: &str, api_key: Option<&str>, headers: Option<&ProviderHeaders>) -> anyhow::Result<String> {
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    anyhow::bail!("No API key for provider: {provider}")
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|msg| match msg {
        Message::ToolResult(_) => true,
        Message::Assistant(a) => a.tool_calls().next().is_some(),
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Compat
// ---------------------------------------------------------------------------

/// Resolved compatibility settings (`ResolvedOpenAICompletionsCompat`).
#[derive(Debug, Clone)]
pub struct ResolvedCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: ThinkingFormat,
    pub open_router_routing: OpenRouterRouting,
    pub vercel_gateway_routing: VercelGatewayRouting,
    pub chat_template_kwargs: IndexMap<String, ChatTemplateKwargValue>,
    pub chat_template_args: IndexMap<String, ChatTemplateKwargValue>,
    pub zai_tool_stream: bool,
    pub supports_thinking_token_budget: Option<bool>,
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_mid_convo_system_messages: Option<bool>,
    pub supports_mid_convo_tool_additions: Option<bool>,
    pub cache_control_format: Option<CacheControlFormat>,
    pub send_session_affinity_headers: bool,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
    pub vllm_priority: Option<f64>,
}

/// Auto-detect compatibility settings from provider name and baseUrl.
pub fn detect_compat(model: &Model) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();
    let is_zai = provider == "zai" || provider == "zai-coding-cn" || base_url.contains("api.z.ai") || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together" || base_url.contains("api.together.ai") || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai" || provider == "moonshotai-cn" || base_url.contains("api.moonshot.");
    let is_open_router = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cf_workers = provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cf_gateway = provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_cerebras = provider == "cerebras" || base_url.contains("cerebras.ai");
    let is_deepseek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");
    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_non_standard = is_nvidia
        || is_cerebras
        || is_grok
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cf_workers
        || is_cf_gateway
        || is_ant_ling;
    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cf_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;
    let open_router_developer = is_open_router && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let cache_control_format =
        if provider == "openrouter" && model.id.starts_with("anthropic/") { Some(CacheControlFormat::Anthropic) } else { None };
    ResolvedCompat {
        supports_store: !is_non_standard,
        supports_developer_role: open_router_developer || (!is_non_standard && !is_open_router),
        supports_reasoning_effort: !is_grok && !is_zai && !is_moonshot && !is_together && !is_cf_gateway && !is_nvidia && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens { MaxTokensField::MaxTokens } else { MaxTokensField::MaxCompletionTokens },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format: if is_deepseek {
            ThinkingFormat::Deepseek
        } else if is_zai {
            ThinkingFormat::Zai
        } else if is_together {
            ThinkingFormat::Together
        } else if is_ant_ling {
            ThinkingFormat::AntLing
        } else if is_open_router {
            ThinkingFormat::Openrouter
        } else {
            ThinkingFormat::Openai
        },
        open_router_routing: OpenRouterRouting::default(),
        vercel_gateway_routing: VercelGatewayRouting::default(),
        chat_template_kwargs: IndexMap::new(),
        chat_template_args: IndexMap::new(),
        zai_tool_stream: false,
        supports_thinking_token_budget: Some(false),
        thinking_token_budget_field: None,
        supports_strict_mode: false,
        supports_openai_grammar_tools: false,
        supports_mid_convo_system_messages: Some(false),
        supports_mid_convo_tool_additions: Some(false),
        cache_control_format,
        send_session_affinity_headers: is_open_router,
        session_affinity_format: if is_open_router { SessionAffinityFormat::Openrouter } else { SessionAffinityFormat::Openai },
        supports_long_cache_retention: !(is_together || is_cf_workers || is_cf_gateway || is_nvidia || is_ant_ling),
        vllm_priority: None,
    }
}

/// Resolved compatibility: auto-detected values overridden by `model.compat`.
pub fn get_compat(model: &Model) -> ResolvedCompat {
    let detected = detect_compat(model);
    let Some(c) = &model.compat else { return detected };
    ResolvedCompat {
        supports_store: c.supports_store.unwrap_or(detected.supports_store),
        supports_developer_role: c.supports_developer_role.unwrap_or(detected.supports_developer_role),
        supports_reasoning_effort: c.supports_reasoning_effort.unwrap_or(detected.supports_reasoning_effort),
        supports_usage_in_streaming: c.supports_usage_in_streaming.unwrap_or(detected.supports_usage_in_streaming),
        supports_finish_reason: c.supports_finish_reason.unwrap_or(detected.supports_finish_reason),
        max_tokens_field: c.max_tokens_field.unwrap_or(detected.max_tokens_field),
        requires_tool_result_name: c.requires_tool_result_name.unwrap_or(detected.requires_tool_result_name),
        requires_assistant_after_tool_result: c
            .requires_assistant_after_tool_result
            .unwrap_or(detected.requires_assistant_after_tool_result),
        requires_thinking_as_text: c.requires_thinking_as_text.unwrap_or(detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: c
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
        thinking_format: c.thinking_format.unwrap_or(detected.thinking_format),
        open_router_routing: c.open_router_routing.clone().unwrap_or_default(),
        vercel_gateway_routing: c.vercel_gateway_routing.clone().unwrap_or(detected.vercel_gateway_routing),
        chat_template_kwargs: c.chat_template_kwargs.clone().unwrap_or(detected.chat_template_kwargs),
        chat_template_args: c.chat_template_args.clone().unwrap_or(detected.chat_template_args),
        zai_tool_stream: c.zai_tool_stream.unwrap_or(detected.zai_tool_stream),
        supports_thinking_token_budget: c.supports_thinking_token_budget.or(detected.supports_thinking_token_budget),
        thinking_token_budget_field: c.thinking_token_budget_field.or(detected.thinking_token_budget_field),
        supports_strict_mode: c.supports_strict_mode.unwrap_or(detected.supports_strict_mode),
        supports_openai_grammar_tools: c.supports_openai_grammar_tools.unwrap_or(detected.supports_openai_grammar_tools),
        supports_mid_convo_system_messages: c.supports_mid_convo_system_messages.or(detected.supports_mid_convo_system_messages),
        supports_mid_convo_tool_additions: c.supports_mid_convo_tool_additions.or(detected.supports_mid_convo_tool_additions),
        cache_control_format: c.cache_control_format.or(detected.cache_control_format),
        send_session_affinity_headers: c.send_session_affinity_headers.unwrap_or(detected.send_session_affinity_headers),
        session_affinity_format: c.session_affinity_format.unwrap_or(detected.session_affinity_format),
        supports_long_cache_retention: c.supports_long_cache_retention.unwrap_or(detected.supports_long_cache_retention),
        vllm_priority: c.vllm_priority,
    }
}

fn resolve_cache_retention(cache_retention: Option<CacheRetention>, env: Option<&crate::types::ProviderEnv>) -> CacheRetention {
    if let Some(r) = cache_retention {
        return r;
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

// ---------------------------------------------------------------------------
// Reasoning details
// ---------------------------------------------------------------------------

fn has_valid_common_reasoning_detail_fields(detail: &Map<String, Value>) -> bool {
    matches!(detail.get("id"), None | Some(Value::Null) | Some(Value::String(_)))
        && matches!(detail.get("format"), None | Some(Value::String(_)))
        && matches!(detail.get("index"), None | Some(Value::Number(_)))
}

fn is_openai_reasoning_detail(detail: &Value) -> bool {
    let Some(obj) = detail.as_object() else { return false };
    if !has_valid_common_reasoning_detail_fields(obj) {
        return false;
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => obj.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => obj.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            obj.get("text").is_some_and(Value::is_string)
                && matches!(obj.get("signature"), None | Some(Value::Null) | Some(Value::String(_)))
        }
        _ => false,
    }
}

fn parse_openai_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(signature.filter(|s| !s.is_empty())?).ok()?;
    let items = parsed.as_array()?;
    if !items.is_empty() && items.iter().all(is_openai_reasoning_detail) { Some(items.clone()) } else { None }
}

fn parse_legacy_encrypted_reasoning_detail(signature: Option<&str>) -> Option<Value> {
    let parsed: Value = serde_json::from_str(signature.filter(|s| !s.is_empty())?).ok()?;
    let valid = is_openai_reasoning_detail(&parsed)
        && parsed["type"] == "reasoning.encrypted"
        && parsed.get("id").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
        && parsed.get("data").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if valid { Some(parsed) } else { None }
}

/// `target[key] ??= source[key]` (`nullish_only`) or `target[key] ||= source[key]`: an absent
/// source value assigns `undefined`, which drops the key from the serialized detail.
fn js_assign_if_unset(target: &mut Map<String, Value>, key: &str, source: &Map<String, Value>, nullish_only: bool) {
    let unset = match target.get(key) {
        None | Some(Value::Null) => true,
        Some(value) => !nullish_only && !crate::serde_util::js_truthy(value),
    };
    if !unset {
        return;
    }
    match source.get(key) {
        Some(value) => {
            target.insert(key.into(), value.clone());
        }
        None => {
            target.remove(key);
        }
    }
}

fn fill_missing_common_fields(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    js_assign_if_unset(target, "id", source, true);
    js_assign_if_unset(target, "format", source, false);
    js_assign_if_unset(target, "index", source, true);
}

fn append_openai_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let detail_obj = detail.as_object().cloned().unwrap_or_default();
    let detail_type = detail_obj.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    if let Some(Value::Object(last)) = details.last_mut() {
        let last_type = last.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
        if detail_type == "reasoning.text" && last_type == "reasoning.text" {
            let text = format!(
                "{}{}",
                last.get("text").and_then(Value::as_str).unwrap_or_default(),
                detail_obj.get("text").and_then(Value::as_str).unwrap_or_default()
            );
            last.insert("text".into(), json!(text));
            js_assign_if_unset(last, "signature", &detail_obj, false);
            fill_missing_common_fields(last, &detail_obj);
            return;
        }
        if detail_type == "reasoning.summary" && last_type == "reasoning.summary" {
            let summary = format!(
                "{}{}",
                last.get("summary").and_then(Value::as_str).unwrap_or_default(),
                detail_obj.get("summary").and_then(Value::as_str).unwrap_or_default()
            );
            last.insert("summary".into(), json!(summary));
            fill_missing_common_fields(last, &detail_obj);
            return;
        }
    }
    details.push(Value::Object(detail_obj));
}

const REASONING_FIELDS: [&str; 3] = ["reasoning", "reasoning_content", "reasoning_text"];

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn data_url(image: &crate::types::ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data)
}

fn normalize_tool_call_id_for(model: &Model, id: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.encode_utf16()
            .map(|u| {
                let ok = u < 128 && ((u as u8).is_ascii_alphanumeric() || u == b'_' as u16 || u == b'-' as u16);
                if ok { u } else { b'_' as u16 }
            })
            .map(|u| u as u8 as char)
            .collect()
    };
    if let Some(separator) = id.find('|') {
        let call_id = sanitize(&id[..separator]);
        let item_id = sanitize(&id[separator + 1..]);
        let combined = if item_id.is_empty() { call_id.clone() } else { format!("{call_id}_{item_id}") };
        if combined.len() <= 40 {
            return combined;
        }
        let hash: String = short_hash(id).chars().take(8).collect();
        let prefix_len = 1usize.max(40 - hash.len() - 1);
        let prefix: String = call_id.chars().take(prefix_len).collect();
        return format!("{prefix}_{hash}");
    }
    if model.provider == "openai" {
        let units: Vec<u16> = id.encode_utf16().collect();
        if units.len() > 40 {
            return String::from_utf16_lossy(&units[..40]);
        }
    }
    id.to_string()
}

fn transcript_tool_support(compat: &ResolvedCompat) -> TranscriptToolSupport {
    if compat.supports_mid_convo_system_messages == Some(true) && compat.supports_mid_convo_tool_additions == Some(true) {
        TranscriptToolSupport::Additions
    } else {
        TranscriptToolSupport::None
    }
}

fn convert_tools(tools: &[Tool], compat: &ResolvedCompat) -> anyhow::Result<Vec<Value>> {
    let mut result = Vec::with_capacity(tools.len());
    for tool in tools {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)? {
            result.push(json!({
                "type": "custom",
                "custom": {
                    "name": tool.name,
                    "description": tool.description,
                    "format": { "type": "grammar", "grammar": { "syntax": grammar.format, "definition": grammar.definition } }
                }
            }));
            continue;
        }
        let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode, None)?;
        let mut function = Map::new();
        function.insert("name".into(), json!(tool.name));
        function.insert("description".into(), json!(tool.description));
        function.insert("parameters".into(), get_json_schema_tool_parameters(tool, strict));
        if compat.supports_strict_mode {
            function.insert("strict".into(), json!(strict.unwrap_or(false)));
        }
        result.push(json!({ "type": "function", "function": Value::Object(function) }));
    }
    Ok(result)
}

/// Grammar tool input properties for declared tools.
fn grammar_tool_input_properties(tools: &[Tool], supports: bool) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for tool in tools {
        if let Ok(Some(grammar)) = resolve_grammar_constrained_sampling(tool, supports) {
            map.insert(tool.name.clone(), grammar.input_property);
        }
    }
    map
}

/// Convert a transcript to Chat Completions messages.
pub fn convert_messages(
    model: &Model,
    context: &TranscriptContext,
    compat: &ResolvedCompat,
    grammar_properties: &HashMap<String, String>,
) -> anyhow::Result<Vec<Value>> {
    let normalized = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let normalize = |id: &str, m: &Model, _: &AssistantMessage| normalize_tool_call_id_for(m, id);
    let transformed = transform_messages(&normalized.messages, model, Some(&normalize));
    let transcript_tools = resolve_transcript_tools(&normalized.messages, transcript_tool_support(compat));
    let instruction_role = if model.reasoning && compat.supports_developer_role { "developer" } else { "system" };
    let mut params: Vec<Value> = Vec::new();
    let mut last_role: Option<&str> = None;
    let mut system_index = 0usize;

    let mut i = 0;
    while i < transformed.len() {
        let msg = &transformed[i];
        if compat.requires_assistant_after_tool_result && last_role == Some("toolResult") && matches!(msg, Message::User(_)) {
            params.push(json!({ "role": "assistant", "content": "I have processed the tool results." }));
        }
        match msg {
            Message::System(system) => {
                let added = if system_index >= transcript_tools.folded_system_messages {
                    system.tools_added.clone().unwrap_or_default()
                } else {
                    Vec::new()
                };
                system_index += 1;
                if !added.is_empty() {
                    params.push(json!({ "role": "system", "tools": convert_tools(&added, compat)? }));
                }
                let text = if i == 0 { get_system_message_text(system) } else { render_system_message_update(system) };
                let empty = if i == 0 { get_system_message_text_is_empty(system) } else { render_system_message_update_is_empty(system) };
                if !empty {
                    params.push(json!({ "role": instruction_role, "content": text }));
                }
                last_role = Some("system");
            }
            Message::User(user) => {
                match &user.content {
                    UserContent::Text(text) => params.push(json!({ "role": "user", "content": text })),
                    UserContent::Blocks(blocks) => {
                        let content: Vec<Value> = blocks
                            .iter()
                            .filter(|b| !matches!(b, UserContentBlock::Text(t) if t.is_empty_text()))
                            .map(|b| match b {
                                UserContentBlock::Text(t) => json!({ "type": "text", "text": t.text }),
                                UserContentBlock::Image(img) => json!({ "type": "image_url", "image_url": { "url": data_url(img) } }),
                            })
                            .collect();
                        if content.is_empty() {
                            i += 1;
                            continue;
                        }
                        params.push(json!({ "role": "user", "content": content }));
                    }
                }
                last_role = Some("user");
            }
            Message::Assistant(assistant) => {
                let mut msg_obj = Map::new();
                msg_obj.insert("role".into(), json!("assistant"));
                msg_obj.insert("content".into(), if compat.requires_assistant_after_tool_result { json!("") } else { Value::Null });
                let text_parts: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(AssistantContent::as_text)
                    .filter(|t| !t.is_blank())
                    .map(|t| t.text.clone())
                    .collect();
                let assistant_text = text_parts.concat();
                let thinking_blocks: Vec<&ThinkingContent> = assistant.content.iter().filter_map(AssistantContent::as_thinking).collect();
                let tool_calls: Vec<&ToolCall> = assistant.tool_calls().collect();
                let signed_details = thinking_blocks.iter().find_map(|b| parse_openai_reasoning_details(b.thinking_signature.as_deref()));
                let legacy_details: Vec<Value> = tool_calls
                    .iter()
                    .filter_map(|tc| parse_legacy_encrypted_reasoning_detail(tc.thought_signature.as_deref()))
                    .collect();
                let preserved_details = signed_details.or(if legacy_details.is_empty() { None } else { Some(legacy_details) });
                let non_empty_thinking: Vec<&&ThinkingContent> = thinking_blocks.iter().filter(|b| !b.is_blank()).collect();
                if !non_empty_thinking.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = non_empty_thinking.iter().map(|b| b.thinking.as_str()).collect::<Vec<_>>().join("\n\n");
                        let mut parts = vec![json!({ "type": "text", "text": thinking_text })];
                        parts.extend(text_parts.iter().map(|t| json!({ "type": "text", "text": t })));
                        msg_obj.insert("content".into(), Value::Array(parts));
                    } else {
                        if !assistant_text.is_empty() {
                            msg_obj.insert("content".into(), json!(assistant_text));
                        }
                        if preserved_details.is_none() {
                            let mut signature = non_empty_thinking[0].thinking_signature.clone().unwrap_or_default();
                            if model.provider == "opencode-go" && signature == "reasoning" {
                                signature = "reasoning_content".into();
                            }
                            if REASONING_FIELDS.contains(&signature.as_str()) {
                                let joined = non_empty_thinking.iter().map(|b| b.thinking.as_str()).collect::<Vec<_>>().join("\n");
                                msg_obj.insert(signature, json!(joined));
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    msg_obj.insert("content".into(), json!(assistant_text));
                }
                if !tool_calls.is_empty() {
                    let mut calls = Vec::new();
                    for tc in &tool_calls {
                        if let Some(property) = grammar_properties.get(&tc.name) {
                            let input = match tc.arguments.get(property) {
                                Some(Value::String(s)) => s.clone(),
                                _ => anyhow::bail!(
                                    "Grammar tool call \"{}\" requires argument \"{property}\" to be a string.",
                                    tc.name
                                ),
                            };
                            calls.push(json!({ "id": tc.id, "type": "custom", "custom": { "name": tc.name, "input": input } }));
                        } else {
                            calls.push(json!({
                                "id": tc.id,
                                "type": "function",
                                "function": { "name": tc.name, "arguments": serde_json::to_string(&tc.arguments)? }
                            }));
                        }
                    }
                    msg_obj.insert("tool_calls".into(), Value::Array(calls));
                }
                if let Some(details) = preserved_details {
                    msg_obj.insert("reasoning_details".into(), Value::Array(details));
                }
                if compat.requires_reasoning_content_on_assistant_messages && model.reasoning && !msg_obj.contains_key("reasoning_content") {
                    msg_obj.insert("reasoning_content".into(), json!(""));
                }
                let has_content = match msg_obj.get("content") {
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::Array(a)) => !a.is_empty(),
                    _ => false,
                };
                if !has_content && !msg_obj.contains_key("tool_calls") {
                    i += 1;
                    continue;
                }
                params.push(Value::Object(msg_obj));
                last_role = Some("assistant");
            }
            Message::ToolResult(_) => {
                let mut image_blocks: Vec<Value> = Vec::new();
                let mut j = i;
                while let Some(Message::ToolResult(tool_msg)) = transformed.get(j) {
                    let text_result =
                        tool_msg.content.iter().filter_map(UserContentBlock::as_text).map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n");
                    let has_images = tool_msg.content.iter().any(|c| c.as_image().is_some());
                    let removed = tool_msg.content.iter().filter_map(UserContentBlock::as_text).any(|t| t.lone_surrogates_removed);
                    let has_text = !is_blank_text(&text_result, removed);
                    let text = if has_text {
                        text_result
                    } else if has_images {
                        "(see attached image)".into()
                    } else {
                        "(no tool output)".into()
                    };
                    let mut result = Map::new();
                    result.insert("role".into(), json!("tool"));
                    result.insert("content".into(), json!(text));
                    result.insert("tool_call_id".into(), json!(tool_msg.tool_call_id));
                    if compat.requires_tool_result_name && !tool_msg.tool_name.is_empty() {
                        result.insert("name".into(), json!(tool_msg.tool_name));
                    }
                    params.push(Value::Object(result));
                    if has_images && model.input.contains(&ModelInputModality::Image) {
                        for block in &tool_msg.content {
                            if let UserContentBlock::Image(img) = block {
                                image_blocks.push(json!({ "type": "image_url", "image_url": { "url": data_url(img) } }));
                            }
                        }
                    }
                    j += 1;
                }
                i = j - 1;
                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({ "role": "assistant", "content": "I have processed the tool results." }));
                    }
                    let mut content = vec![json!({ "type": "text", "text": "Attached image(s) from tool result:" })];
                    content.extend(image_blocks);
                    params.push(json!({ "role": "user", "content": content }));
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }
            }
        }
        i += 1;
    }
    Ok(params)
}

fn add_cache_control_to_text_content(message: &mut Value, cache_control: &Value) -> bool {
    match message.get_mut("content") {
        Some(Value::String(content)) => {
            if content.is_empty() {
                return false;
            }
            let text = std::mem::take(content);
            message["content"] = json!([{ "type": "text", "text": text, "cache_control": cache_control }]);
            true
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part["cache_control"] = cache_control.clone();
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn apply_anthropic_cache_control(messages: &mut [Value], tools: Option<&mut Vec<Value>>, cache_control: &Value) {
    for message in messages.iter_mut() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or_default();
        // The first instruction message, even a tools-only one without content.
        if role == "system" || role == "developer" {
            add_cache_control_to_text_content(message, cache_control);
            break;
        }
    }
    if let Some(tools) = tools
        && let Some(Value::Object(last)) = tools.last_mut() {
            last.insert("cache_control".into(), cache_control.clone());
        }
    for message in messages.iter_mut().rev() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or_default();
        if matches!(role, "user" | "assistant" | "tool") && add_cache_control_to_text_content(message, cache_control) {
            return;
        }
    }
}

/// Typed view of the OpenAI completions options in `StreamOptions.extra`.
#[derive(Debug, Clone, Default)]
struct CompletionsOptions {
    tool_choice: Option<Value>,
    reasoning_effort: Option<ThinkingLevel>,
}

impl CompletionsOptions {
    fn from(options: &StreamOptions) -> Self {
        Self {
            tool_choice: options
                .extra_value("toolChoice")
                .cloned()
                .or_else(|| options.tool_choice.map(|c| serde_json::to_value(c).unwrap_or(Value::Null)))
                .filter(|v| !v.is_null()),
            reasoning_effort: options.extra_str("reasoningEffort").and_then(ThinkingLevel::parse),
        }
    }
}

fn thinking_map_value(model: &Model, level: ModelThinkingLevel) -> Option<Option<String>> {
    model.thinking_level_map.as_ref().and_then(|m| m.get(&level)).cloned()
}

fn resolve_chat_template_value(
    model: &Model,
    effort: Option<ThinkingLevel>,
    value: &ChatTemplateKwargValue,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let var = match value {
        ChatTemplateKwargValue::Var(v) => v,
        ChatTemplateKwargValue::Bool(b) => return Some(json!(b)),
        ChatTemplateKwargValue::Number(n) => return Some(Value::Number(n.clone())),
        ChatTemplateKwargValue::String(s) => return Some(json!(s)),
        ChatTemplateKwargValue::Null => return Some(Value::Null),
    };
    if effort.is_none() && var.omit_when_off == Some(true) {
        return None;
    }
    match var.var {
        ChatTemplateVar::ThinkingEnabled => Some(json!(effort.is_some())),
        ChatTemplateVar::ThinkingBudget => thinking_budget.map(|b| json!(b)),
        ChatTemplateVar::ThinkingEffort => {
            let level = effort.map(ModelThinkingLevel::from).unwrap_or(ModelThinkingLevel::Off);
            match thinking_map_value(model, level) {
                None => effort.map(|e| json!(e.as_str())),
                Some(Some(mapped)) => Some(json!(mapped)),
                Some(None) => None,
            }
        }
    }
}

fn build_chat_template_values(
    model: &Model,
    effort: Option<ThinkingLevel>,
    values: &IndexMap<String, ChatTemplateKwargValue>,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let mut resolved = Map::new();
    for (key, value) in values {
        if let Some(v) = resolve_chat_template_value(model, effort, value, thinking_budget) {
            resolved.insert(key.clone(), v);
        }
    }
    if resolved.is_empty() { None } else { Some(Value::Object(resolved)) }
}

/// Build the request body.
pub fn build_params(model: &Model, context: &TranscriptContext, options: &StreamOptions, compat: &ResolvedCompat) -> anyhow::Result<Value> {
    let completions = CompletionsOptions::from(options);
    let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let grammar_properties = grammar_tool_input_properties(&get_declared_tools(&context.messages), compat.supports_openai_grammar_tools);
    let transcript_tools = resolve_transcript_tools(&context.messages, transcript_tool_support(compat));
    let mut messages = convert_messages(model, context, compat, &grammar_properties)?;
    let cache_control = if compat.cache_control_format == Some(CacheControlFormat::Anthropic) && cache_retention != CacheRetention::None {
        let mut cc = Map::new();
        cc.insert("type".into(), json!("ephemeral"));
        if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
            cc.insert("ttl".into(), json!("1h"));
        }
        Some(Value::Object(cc))
    } else {
        None
    };

    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), Value::Null);
    params.insert("stream".into(), json!(true));
    let use_cache_key = (model.base_url.contains("api.openai.com") && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention);
    if use_cache_key
        && let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref()) {
            params.insert("prompt_cache_key".into(), json!(key));
        }
    if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if compat.supports_store {
        params.insert("store".into(), json!(false));
    }
    if let Some(max_tokens) = options.max_tokens.filter(|m| *m > 0) {
        match compat.max_tokens_field {
            MaxTokensField::MaxTokens => params.insert("max_tokens".into(), json!(max_tokens)),
            MaxTokensField::MaxCompletionTokens => params.insert("max_completion_tokens".into(), json!(max_tokens)),
        };
    }
    if let Some(temperature) = options.temperature {
        params.insert("temperature".into(), crate::serde_util::js_number(temperature));
    }
    let mut tools: Option<Vec<Value>> = None;
    if !transcript_tools.request_tools.is_empty() {
        tools = Some(convert_tools(&transcript_tools.request_tools, compat)?);
    } else if has_tool_history(&context.messages) {
        tools = Some(Vec::new());
    }
    if let Some(cc) = &cache_control {
        apply_anthropic_cache_control(&mut messages, tools.as_mut(), cc);
    }
    params.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = tools {
        let non_empty = !tools.is_empty();
        params.insert("tools".into(), Value::Array(tools));
        if non_empty && compat.zai_tool_stream {
            params.insert("tool_stream".into(), json!(true));
        }
    }
    if let Some(tool_choice) = &completions.tool_choice {
        params.insert("tool_choice".into(), tool_choice.clone());
    }
    if let Some(priority) = compat.vllm_priority {
        params.insert("priority".into(), crate::serde_util::js_number(priority));
    }

    let budget_field = compat.thinking_token_budget_field.or(if compat.supports_thinking_token_budget == Some(true) {
        Some(ThinkingTokenBudgetField::ThinkingTokenBudget)
    } else {
        None
    });
    let effort = completions.reasoning_effort;
    let thinking_budget = match effort {
        Some(level) if model.reasoning => {
            let ceiling = params
                .get("max_tokens")
                .or_else(|| params.get("max_completion_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(model.max_tokens);
            let budget = clamp_thinking_budget_to_answer_room(thinking_budget_for_level(level, options.thinking_budgets.as_ref()), ceiling);
            if budget > 0 { Some(budget) } else { None }
        }
        _ => None,
    };
    let mapped = |level: ThinkingLevel| thinking_map_value(model, level.into());
    let mapped_or_level = |level: ThinkingLevel| -> Option<String> {
        match mapped(level) {
            None => Some(level.as_str().to_string()),
            Some(v) => v,
        }
    };
    let off_value = thinking_map_value(model, ModelThinkingLevel::Off);
    // `thinkingLevelMap?.off !== null`: true unless explicitly null.
    let off_not_null = !matches!(off_value, Some(None));

    match compat.thinking_format {
        ThinkingFormat::Zai if model.reasoning => {
            params.insert(
                "thinking".into(),
                if effort.is_some() { json!({ "type": "enabled", "clear_thinking": false }) } else { json!({ "type": "disabled" }) },
            );
            if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort)
                && let Some(e) = mapped_or_level(level) {
                    params.insert("reasoning_effort".into(), json!(e));
                }
        }
        ThinkingFormat::Qwen if model.reasoning => {
            params.insert("enable_thinking".into(), json!(effort.is_some()));
            if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort) {
                // `map[level] ?? level`: null falls back to the level.
                let e = mapped(level).flatten().unwrap_or_else(|| level.as_str().to_string());
                params.insert("reasoning_effort".into(), json!(e));
            }
        }
        ThinkingFormat::QwenChatTemplate if model.reasoning => {
            params.insert("chat_template_kwargs".into(), json!({ "enable_thinking": effort.is_some(), "preserve_thinking": true }));
        }
        ThinkingFormat::ChatTemplate if model.reasoning => {
            if let Some(values) = build_chat_template_values(model, effort, &compat.chat_template_kwargs, thinking_budget) {
                params.insert("chat_template_kwargs".into(), values);
            }
        }
        ThinkingFormat::Baseten if model.reasoning => {
            if let Some(values) = build_chat_template_values(model, effort, &compat.chat_template_args, thinking_budget) {
                params.insert("chat_template_args".into(), values);
            }
            if compat.supports_reasoning_effort {
                let mapped_effort = match effort {
                    Some(level) => mapped(level),
                    None => off_value.clone(),
                };
                let e = match mapped_effort {
                    None => effort.map(|l| l.as_str().to_string()),
                    Some(v) => v,
                };
                if let Some(e) = e {
                    params.insert("reasoning_effort".into(), json!(e));
                }
            }
        }
        ThinkingFormat::Deepseek if model.reasoning => {
            if effort.is_some() {
                params.insert("thinking".into(), json!({ "type": "enabled" }));
            } else if off_not_null {
                params.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort) {
                let e = mapped(level).flatten().unwrap_or_else(|| level.as_str().to_string());
                params.insert("reasoning_effort".into(), json!(e));
            }
        }
        ThinkingFormat::Openrouter if model.reasoning => {
            if let Some(level) = effort {
                let e = mapped(level).flatten().unwrap_or_else(|| level.as_str().to_string());
                params.insert("reasoning".into(), json!({ "effort": e }));
            } else if off_not_null {
                let e = off_value.clone().flatten().unwrap_or_else(|| "none".to_string());
                params.insert("reasoning".into(), json!({ "effort": e }));
            }
        }
        ThinkingFormat::AntLing if model.reasoning && effort.is_some() => {
            if let Some(Some(e)) = effort.and_then(mapped) {
                params.insert("reasoning".into(), json!({ "effort": e }));
            }
        }
        ThinkingFormat::Together if model.reasoning => {
            params.insert("reasoning".into(), json!({ "enabled": effort.is_some() }));
            if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort) {
                let e = mapped(level).flatten().unwrap_or_else(|| level.as_str().to_string());
                params.insert("reasoning_effort".into(), json!(e));
            }
        }
        ThinkingFormat::StringThinking if model.reasoning => {
            if let Some(level) = effort {
                let e = mapped(level).flatten().unwrap_or_else(|| level.as_str().to_string());
                params.insert("thinking".into(), json!(e));
            } else if off_not_null {
                let e = off_value.clone().flatten().unwrap_or_else(|| "none".to_string());
                params.insert("thinking".into(), json!(e));
            }
        }
        _ => {
            if let (Some(level), true, true) = (effort, model.reasoning, compat.supports_reasoning_effort) {
                let e = mapped(level).flatten().unwrap_or_else(|| level.as_str().to_string());
                params.insert("reasoning_effort".into(), json!(e));
            } else if effort.is_none() && model.reasoning && compat.supports_reasoning_effort
                && let Some(Some(off)) = &off_value {
                    params.insert("reasoning_effort".into(), json!(off));
                }
        }
    }

    if let (Some(field), Some(budget)) = (budget_field, thinking_budget) {
        params.insert(field.as_str().into(), json!(budget));
    }
    if let Some(routing) = model.compat.as_ref().and_then(|c| c.open_router_routing.as_ref()) {
        params.insert("provider".into(), serde_json::to_value(routing)?);
    }
    if let Some(routing) = model.compat.as_ref().and_then(|c| c.vercel_gateway_routing.as_ref())
        && (routing.only.is_some() || routing.order.is_some()) {
            let mut gateway = Map::new();
            if let Some(only) = &routing.only {
                gateway.insert("only".into(), json!(only));
            }
            if let Some(order) = &routing.order {
                gateway.insert("order".into(), json!(order));
            }
            params.insert("providerOptions".into(), json!({ "gateway": gateway }));
        }
    let level = effort.map(ModelThinkingLevel::from).unwrap_or(ModelThinkingLevel::Off);
    if let Some(sampling) = resolve_sampling_params(model, level, options.sampling_params.as_ref()) {
        for (k, v) in sampling {
            params.insert(k, v);
        }
    }
    Ok(Value::Object(params))
}

fn build_headers(
    model: &Model,
    context: &TranscriptContext,
    api_key: &str,
    options: &StreamOptions,
    session_id: Option<&str>,
    compat: &ResolvedCompat,
    timeout_ms: u64,
) -> anyhow::Result<HeaderList> {
    let mut headers = super::openai_responses::openai_sdk_base_headers(timeout_ms);
    headers.set("Authorization", Some(&format!("Bearer {api_key}")));
    apply_openai_custom_headers_env(&mut headers)?;
    let target = ClientIdentityTarget { provider: &model.provider, base_url: &model.base_url };
    headers.extend_strings(&get_identity_headers(target, options.identity.as_ref()));
    if let Some(model_headers) = &model.headers {
        headers.extend_strings(model_headers);
    }
    if model.provider == "github-copilot" {
        let dynamic = build_copilot_dynamic_headers(&context.messages, has_copilot_vision_input(&context.messages));
        headers.extend_strings(&dynamic);
    }
    if let Some(session_id) = session_id.filter(|s| !s.is_empty())
        && compat.send_session_affinity_headers {
            if compat.session_affinity_format == SessionAffinityFormat::Openrouter {
                headers.set("x-session-id", Some(session_id));
            } else {
                if compat.session_affinity_format == SessionAffinityFormat::Openai {
                    headers.set("session_id", Some(session_id));
                }
                headers.set("x-client-request-id", Some(session_id));
                headers.set("x-session-affinity", Some(session_id));
            }
        }
    if let Some(h) = &options.headers {
        headers.extend_optional(h);
    }
    headers.set("Content-Type", Some("application/json"));
    Ok(headers)
}

fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

fn parse_chunk_usage(raw: &Value, model: &Model) -> Usage {
    let num = |v: Option<&Value>| v.and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f.max(0.0) as u64)));
    let prompt_tokens = num(raw.get("prompt_tokens")).unwrap_or(0);
    let details = raw.get("prompt_tokens_details");
    let cache_read = num(details.and_then(|d| d.get("cached_tokens")))
        .or_else(|| num(raw.get("prompt_cache_hit_tokens")))
        .or_else(|| num(raw.get("cached_tokens")))
        .unwrap_or(0);
    let cache_write = num(details.and_then(|d| d.get("cache_write_tokens"))).unwrap_or(0);
    let input = prompt_tokens.saturating_sub(cache_read).saturating_sub(cache_write);
    let output = num(raw.get("completion_tokens")).unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(num(raw.get("completion_tokens_details").and_then(|d| d.get("reasoning_tokens"))).unwrap_or(0)),
        total_tokens: input + output + cache_read + cache_write,
        ..Default::default()
    };
    calculate_cost(&model.cost, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (StopReason::Error, Some("Provider finish_reason: content_filter".into())),
        "network_error" => (StopReason::Error, Some("Provider finish_reason: network_error".into())),
        other => (StopReason::Error, Some(format!("Provider finish_reason: {other}"))),
    }
}

/// Streaming state of one tool call block.
struct ToolCallState {
    content_index: usize,
    partial_args: Option<String>,
    custom_input: Option<(String, GrammarJsonBuffer)>,
    stream_index: Option<i64>,
}

#[derive(Default, Clone)]
struct GrammarJsonBuffer {
    input: String,
    started: bool,
    closed: bool,
}

fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarJsonBuffer,
    property: &str,
    next_input: &str,
    close: bool,
) -> anyhow::Result<Option<String>> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        anyhow::bail!("grammar tool input for property \"{property}\" changed after it was closed");
    }
    if !next_input.starts_with(&buffer.input) {
        anyhow::bail!("grammar tool input for property \"{property}\" changed non-monotonically");
    }
    let input_delta = &next_input[buffer.input.len()..];
    if !close && input_delta.is_empty() {
        return Ok(None);
    }
    let mut delta = String::new();
    if !buffer.started {
        delta.push_str(&format!("{{{}:\"", serde_json::to_string(property)?));
        buffer.started = true;
    }
    let encoded = serde_json::to_string(input_delta)?;
    delta.push_str(&encoded[1..encoded.len() - 1]);
    buffer.input = next_input.to_string();
    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

async fn run_stream(model: Model, context: TranscriptContext, options: StreamOptions, sender: AssistantMessageEventSender) {
    let compat = get_compat(&model);
    let normalized = resolve_transcript(&context, compat.supports_mid_convo_system_messages);
    let mut output = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let signal = options.signal.clone();
    let aborted = || signal.as_ref().is_some_and(|s| s.is_cancelled());
    let mut streamed_reasoning_details: Option<Vec<Value>> = None;
    let mut thinking_index: Option<usize> = None;
    let mut text_index: Option<usize> = None;
    let mut tool_states: Vec<ToolCallState> = Vec::new();

    let result: anyhow::Result<()> = async {
        let api_key = get_client_api_key(&model.provider, options.api_key.as_deref(), options.headers.as_ref())?;
        let grammar_properties =
            grammar_tool_input_properties(&get_declared_tools(&normalized.messages), compat.supports_openai_grammar_tools);
        let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
        let cache_session_id = if cache_retention == CacheRetention::None { None } else { options.session_id.as_deref() };
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let headers = build_headers(&model, &normalized, &api_key, &options, cache_session_id, &compat, timeout_ms)?;
        let mut params = build_params(&model, &normalized, &options, &compat)?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(params.clone(), model.clone()).await {
                params = next;
            }
        let url = chat_completions_url(&model.base_url);
        let client = match &options.http_client {
            Some(client) => client.clone(),
            None => client_for(&url, options.env.as_ref())?,
        };
        let retry = ProviderRetryOptions {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            signal: signal.clone(),
            no_retry_statuses: Vec::new(),
        };
        let response = retry_provider_request(
            || post_json(&client, &url, &headers, &params, Some(timeout_ms), signal.as_ref(), SdkErrorStyle::OpenAI),
            &retry,
        )
        .await?;
        if let Some(on_response) = &options.on_response {
            on_response(
                ProviderResponse { status: response.status().as_u16(), headers: headers_to_record(response.headers()) },
                model.clone(),
            )
            .await;
        }
        sender.push(AssistantMessageEvent::Start { partial: output.clone() });

        let mut has_finish_reason = false;
        let mut by_index: HashMap<i64, usize> = HashMap::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        // The SDK stream ends quietly when the request is aborted mid-stream; the blocks are then
        // finished and the abort is reported below.
        let mut events = super::openai_responses::openai_sse_events(response, signal.clone());
        while let Some(chunk) = futures::StreamExt::next(&mut events).await {
            let chunk = chunk?;
            if let Some(observer) = &options.on_provider_stream_event {
                observer(chunk.clone(), model.clone()).await;
            }
            if !chunk.is_object() {
                continue;
            }
            if output.response_id.as_deref().is_none_or(str::is_empty)
                && let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    output.response_id = Some(id.to_string());
                }
            if let Some(chunk_model) = chunk.get("model").and_then(Value::as_str)
                && !chunk_model.is_empty() && chunk_model != model.id && output.response_model.as_deref().is_none_or(str::is_empty) {
                    output.response_model = Some(chunk_model.to_string());
                }
            let chunk_usage = chunk.get("usage").filter(|u| u.is_object());
            if let Some(usage) = chunk_usage {
                output.usage = parse_chunk_usage(usage, &model);
            }
            let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else { continue };
            if chunk_usage.is_none()
                && let Some(usage) = choice.get("usage").filter(|u| u.is_object()) {
                    output.usage = parse_chunk_usage(usage, &model);
                }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                output.raw_stop_reason = Some(reason.to_string());
                let (stop_reason, error_message) = map_stop_reason(reason);
                output.stop_reason = stop_reason;
                if error_message.is_some() {
                    output.error_message = error_message;
                }
                has_finish_reason = true;
            }
            let Some(delta) = choice.get("delta").filter(|d| d.is_object()) else { continue };

            let ensure_thinking = |output: &mut AssistantMessage, thinking_index: &mut Option<usize>, signature: &str| -> usize {
                if let Some(index) = *thinking_index {
                    return index;
                }
                output.content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some(signature.to_string()),
                    redacted: None,
                    ..Default::default()
                }));
                let index = output.content.len() - 1;
                *thinking_index = Some(index);
                sender.push(AssistantMessageEvent::ThinkingStart { content_index: index, partial: output.clone() });
                index
            };

            let found = ["reasoning_content", "reasoning", "reasoning_text"]
                .into_iter()
                .find(|field| delta.get(*field).and_then(Value::as_str).is_some_and(|s| !s.is_empty()));
            if let Some(field) = found {
                let text = delta[field].as_str().unwrap_or_default().to_string();
                let signature = if model.provider == "opencode-go" && field == "reasoning" { "reasoning_content" } else { field };
                let index = ensure_thinking(&mut output, &mut thinking_index, signature);
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) {
                    block.thinking.push_str(&text);
                }
                sender.push(AssistantMessageEvent::ThinkingDelta { content_index: index, delta: text, partial: output.clone() });
            }
            if let Some(Value::Array(details)) = delta.get("reasoning_details") {
                for detail in details {
                    if !is_openai_reasoning_detail(detail) {
                        continue;
                    }
                    ensure_thinking(&mut output, &mut thinking_index, "");
                    append_openai_reasoning_detail(streamed_reasoning_details.get_or_insert_with(Vec::new), detail);
                }
            }
            if let Some(content) = delta.get("content").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                let index = match text_index {
                    Some(i) => i,
                    None => {
                        output.content.push(AssistantContent::Text(TextContent::new("")));
                        let i = output.content.len() - 1;
                        text_index = Some(i);
                        sender.push(AssistantMessageEvent::TextStart { content_index: i, partial: output.clone() });
                        i
                    }
                };
                if let Some(AssistantContent::Text(block)) = output.content.get_mut(index) {
                    block.text.push_str(content);
                }
                sender.push(AssistantMessageEvent::TextDelta { content_index: index, delta: content.to_string(), partial: output.clone() });
            }
            if let Some(Value::Array(tool_calls)) = delta.get("tool_calls") {
                for tool_call in tool_calls {
                    let stream_index = tool_call.get("index").and_then(Value::as_i64);
                    let id = tool_call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
                    let function = tool_call.get("function").filter(|f| f.is_object());
                    let custom = tool_call.get("custom").filter(|c| c.is_object());
                    // `function?.name ?? custom?.name`: null falls through.
                    let name = function
                        .and_then(|f| f.get("name"))
                        .filter(|n| !n.is_null())
                        .or_else(|| custom.and_then(|c| c.get("name")))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let mut state_index = stream_index.and_then(|i| by_index.get(&i).copied());
                    if state_index.is_none()
                        && let Some(id) = &id {
                            state_index = by_id.get(id).copied();
                        }
                    let state_index = match state_index {
                        Some(i) => i,
                        None => {
                            let block_name = name.clone().unwrap_or_default();
                            let custom_property = if custom.is_some() && function.is_none() {
                                Some(grammar_properties.get(&block_name).cloned().unwrap_or_else(|| "input".into()))
                            } else {
                                None
                            };
                            let mut arguments = Map::new();
                            if let Some(property) = &custom_property {
                                arguments.insert(property.clone(), json!(""));
                            }
                            output.content.push(AssistantContent::ToolCall(ToolCall {
                                id: id.clone().unwrap_or_default(),
                                name: block_name,
                                arguments,
                                ..Default::default()
                            }));
                            let content_index = output.content.len() - 1;
                            tool_states.push(ToolCallState {
                                content_index,
                                partial_args: if custom_property.is_some() { None } else { Some(String::new()) },
                                custom_input: custom_property.map(|p| (p, GrammarJsonBuffer::default())),
                                stream_index,
                            });
                            let state = tool_states.len() - 1;
                            if let Some(si) = stream_index {
                                by_index.insert(si, state);
                            }
                            if let Some(id) = &id {
                                by_id.insert(id.clone(), state);
                            }
                            sender.push(AssistantMessageEvent::ToolCallStart { content_index, partial: output.clone() });
                            state
                        }
                    };
                    if let Some(si) = stream_index
                        && tool_states[state_index].stream_index.is_none() {
                            tool_states[state_index].stream_index = Some(si);
                            by_index.insert(si, state_index);
                        }
                    if let Some(id) = &id {
                        by_id.insert(id.clone(), state_index);
                    }
                    let content_index = tool_states[state_index].content_index;
                    let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(content_index) else { continue };
                    if block.name.is_empty()
                        && let Some(name) = &name {
                            block.name = name.clone();
                        }
                    if custom.is_some() && function.is_none() && tool_states[state_index].custom_input.is_none() {
                        let property = grammar_properties.get(&block.name).cloned().unwrap_or_else(|| "input".into());
                        block.arguments = Map::new();
                        block.arguments.insert(property.clone(), json!(""));
                        tool_states[state_index].custom_input = Some((property, GrammarJsonBuffer::default()));
                        tool_states[state_index].partial_args = None;
                    }
                    if block.id.is_empty()
                        && let Some(id) = &id {
                            block.id = id.clone();
                            by_id.insert(id.clone(), state_index);
                        }
                    let mut delta_text = String::new();
                    let fn_args = function.and_then(|f| f.get("arguments")).and_then(Value::as_str).filter(|a| !a.is_empty());
                    let custom_input = custom.and_then(|c| c.get("input")).and_then(Value::as_str).filter(|a| !a.is_empty());
                    if let Some(args) = fn_args {
                        delta_text = args.to_string();
                        let state = &mut tool_states[state_index];
                        let partial = state.partial_args.get_or_insert_with(String::new);
                        partial.push_str(args);
                        block.arguments = parse_streaming_json_object(Some(partial));
                    } else if let Some(input) = custom_input
                        && let Some((property, buffer)) = &mut tool_states[state_index].custom_input {
                            let current = block.arguments.get(property.as_str()).and_then(Value::as_str).unwrap_or_default().to_string();
                            let next = format!("{current}{input}");
                            delta_text = append_grammar_tool_input_json_delta(buffer, property, &next, false)?.unwrap_or_default();
                            block.arguments = Map::new();
                            block.arguments.insert(property.clone(), json!(next));
                        }
                    sender.push(AssistantMessageEvent::ToolCallDelta { content_index, delta: delta_text, partial: output.clone() });
                }
            }
        }

        finish_blocks(&mut output, &mut tool_states, &streamed_reasoning_details, &sender)?;
        if aborted() {
            anyhow::bail!("Request was aborted");
        }
        if output.stop_reason == StopReason::Aborted {
            anyhow::bail!("Request was aborted");
        }
        if !has_finish_reason && !compat.supports_finish_reason {
            output.stop_reason =
                if output.content.iter().any(|b| matches!(b, AssistantContent::ToolCall(_))) { StopReason::ToolUse } else { StopReason::Stop };
        }
        if output.stop_reason == StopReason::Error {
            anyhow::bail!("{}", output.error_message.clone().unwrap_or_else(|| "Provider returned an error stop reason".into()));
        }
        if (compat.supports_finish_reason && !has_finish_reason) || output.stop_reason == StopReason::Pending {
            anyhow::bail!("Stream ended without finish_reason");
        }
        sender.push(AssistantMessageEvent::Done { reason: output.stop_reason, message: output.clone() });
        sender.end(None);
        Ok(())
    }
    .await;

    if let Err(error) = result {
        if let (Some(details), Some(index)) = (&streamed_reasoning_details, thinking_index)
            && let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) {
                block.thinking_signature = Some(serde_json::to_string(details).unwrap_or_default());
            }
        output.stop_reason = if aborted() { StopReason::Aborted } else { StopReason::Error };
        let mut message = format_provider_error(&normalize_provider_error(&error), None);
        // Some providers via OpenRouter give additional information in `error.metadata.raw`;
        // append it unless the message already carries it.
        if let Some(raw) = error
            .downcast_ref::<crate::utils::error_body::ProviderHttpError>()
            .and_then(|e| e.error.as_ref())
            .and_then(|e| e.get("metadata"))
            .and_then(|m| m.get("raw"))
            .filter(|raw| crate::serde_util::js_truthy(raw))
        {
            let raw = crate::serde_util::js_string(raw);
            if !message.contains(&raw) {
                message.push('\n');
                message.push_str(&raw);
            }
        }
        output.error_message = Some(message);
        sender.push(AssistantMessageEvent::Error { reason: output.stop_reason, error: output.clone() });
        sender.end(None);
    }
}

fn finish_blocks(
    output: &mut AssistantMessage,
    tool_states: &mut [ToolCallState],
    streamed_reasoning_details: &Option<Vec<Value>>,
    sender: &AssistantMessageEventSender,
) -> anyhow::Result<()> {
    for index in 0..output.content.len() {
        match &mut output.content[index] {
            AssistantContent::Text(block) => {
                let content = block.text.clone();
                sender.push(AssistantMessageEvent::TextEnd { content_index: index, content, partial: output.clone() });
            }
            AssistantContent::Thinking(block) => {
                if let Some(details) = streamed_reasoning_details {
                    block.thinking_signature = Some(serde_json::to_string(details)?);
                }
                let content = block.thinking.clone();
                sender.push(AssistantMessageEvent::ThinkingEnd { content_index: index, content, partial: output.clone() });
            }
            AssistantContent::ToolCall(block) => {
                let state = tool_states.iter_mut().find(|s| s.content_index == index);
                if let Some(state) = state {
                    if let Some((property, buffer)) = &mut state.custom_input {
                        let current = block.arguments.get(property.as_str()).and_then(Value::as_str).unwrap_or_default().to_string();
                        let delta = append_grammar_tool_input_json_delta(buffer, property, &current, true)?;
                        block.arguments = Map::new();
                        block.arguments.insert(property.clone(), json!(current));
                        if let Some(delta) = delta {
                            sender.push(AssistantMessageEvent::ToolCallDelta { content_index: index, delta, partial: output.clone() });
                        }
                    } else {
                        let parsed = parse_tool_call_arguments(state.partial_args.as_deref());
                        block.arguments = parsed.arguments;
                        block.arguments_error = parsed.arguments_error;
                    }
                    state.partial_args = None;
                    state.custom_input = None;
                }
                let AssistantContent::ToolCall(block) = &output.content[index] else { unreachable!() };
                let tool_call = block.clone();
                sender.push(AssistantMessageEvent::ToolCallEnd { content_index: index, tool_call, partial: output.clone() });
            }
        }
    }
    Ok(())
}

/// Stream a Chat Completions response.
pub fn stream(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    if let Err(error) = get_client_api_key(&model.provider, options.api_key.as_deref(), options.headers.as_ref()) {
        return super::lazy::error_stream(model, error);
    }
    let mut base = build_base_options(model, context, &options, options.api_key.as_deref());
    // TS forwards `options.toolChoice` as given, so values outside the provider-neutral
    // `ToolChoice` (e.g. "required" or a named function) arrive through `extra`.
    if let Some(choice) = options.tool_choice {
        base.extra.insert("toolChoice".into(), serde_json::to_value(choice).unwrap_or(Value::Null));
    } else if let Some(choice) = options.extra_value("toolChoice").filter(|c| !c.is_null()) {
        base.extra.insert("toolChoice".into(), choice.clone());
    }
    if let Some(level) = options.reasoning {
        let clamped = clamp_thinking_level(model, level.into());
        if let Some(effort) = clamped.thinking() {
            base.extra.insert("reasoningEffort".into(), json!(effort.as_str()));
        }
    }
    base.thinking_budgets = options.thinking_budgets.clone();
    stream(model, context, base)
}

/// The `openai-completions` API implementation.
pub struct OpenAICompletionsApi;

impl ProviderStreams for OpenAICompletionsApi {
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
    fn normalizes_pipe_ids() {
        let model = Model { provider: "openai".into(), ..Default::default() };
        assert_eq!(normalize_tool_call_id_for(&model, "call_1|item+1"), "call_1_item_1");
        let long = format!("call_abc|{}", "x".repeat(450));
        let id = normalize_tool_call_id_for(&model, &long);
        assert!(id.len() <= 40, "{id}");
        assert!(id.starts_with("call_abc_"));
        assert_eq!(normalize_tool_call_id_for(&model, &"y".repeat(50)).len(), 40);
    }

    #[test]
    fn grammar_json_delta() {
        let mut buffer = GrammarJsonBuffer::default();
        assert_eq!(append_grammar_tool_input_json_delta(&mut buffer, "input", "a\"b", false).unwrap().as_deref(), Some("{\"input\":\"a\\\"b"));
        assert_eq!(append_grammar_tool_input_json_delta(&mut buffer, "input", "a\"bc", true).unwrap().as_deref(), Some("c\"}"));
        assert_eq!(append_grammar_tool_input_json_delta(&mut buffer, "input", "a\"bc", true).unwrap(), None);
    }

    #[test]
    fn reasoning_detail_merge_assigns_undefined_like_js() {
        // `target.id ??= source.id` with no source id leaves `undefined`: the key is dropped.
        let mut details = vec![json!({"type": "reasoning.text", "text": "a", "id": null, "format": "", "signature": null})];
        append_openai_reasoning_detail(&mut details, &json!({"type": "reasoning.text", "text": "b"}));
        assert_eq!(details, vec![json!({"type": "reasoning.text", "text": "ab"})]);
        let mut details = vec![json!({"type": "reasoning.text", "text": "a", "format": ""})];
        append_openai_reasoning_detail(&mut details, &json!({"type": "reasoning.text", "text": "b", "format": "f", "signature": "s", "id": "x"}));
        assert_eq!(details, vec![json!({"type": "reasoning.text", "text": "ab", "format": "f", "signature": "s", "id": "x"})]);
    }

    #[test]
    fn anthropic_cache_control_targets_the_first_instruction_message() {
        // A tools-only system message (no content) is the first instruction message: nothing
        // else gets the system cache marker.
        let mut messages = vec![
            json!({"role": "system", "tools": []}),
            json!({"role": "system", "content": "later"}),
            json!({"role": "user", "content": "hi"}),
        ];
        let cc = json!({"type": "ephemeral"});
        apply_anthropic_cache_control(&mut messages, None, &cc);
        assert_eq!(messages[1], json!({"role": "system", "content": "later"}));
        assert_eq!(messages[2], json!({"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": cc}]}));
    }

    #[test]
    fn prompt_cache_key_clamp() {
        assert_eq!(clamp_openai_prompt_cache_key(Some(&"é".repeat(70))).unwrap().chars().count(), 64);
        assert_eq!(clamp_openai_prompt_cache_key(None), None);
    }
}
