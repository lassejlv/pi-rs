//! Shared OpenAI Responses request conversion and stream processing
//! (port of `api/openai-responses-shared.ts`).
//!
//! Used by `openai-responses`, `azure-openai-responses` and `openai-codex-responses`.

use std::collections::{HashMap, HashSet};

use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};

use super::constrained_sampling::{
    GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta, get_grammar_tool_input,
    get_json_schema_tool_parameters, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
use super::transform_messages::transform_messages;
use crate::models::calculate_cost;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, ImageContent, Message, Model,
    OnProviderStreamEvent, StopReason, SystemMessage, TextContent, ThinkingContent, Tool, ToolCall,
    TranscriptContext, Usage, UserContent, UserContentBlock,
};
use crate::utils::event_stream::AssistantMessageEventSender;
use crate::utils::hash::short_hash;
use crate::utils::json_parse::{parse_streaming_json_object, parse_tool_call_arguments};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::text::{
    get_system_message_text, get_system_message_text_is_empty, render_system_message_update,
    render_system_message_update_is_empty,
};
use crate::utils::transcript::{
    TranscriptToolSupport, resolve_transcript, resolve_transcript_tools,
};

// =============================================================================
// Utilities
// =============================================================================

fn encode_text_signature_v1(id: Option<&str>, phase: Option<&str>) -> String {
    let mut payload = Map::new();
    payload.insert("v".into(), json!(1));
    if let Some(id) = id {
        payload.insert("id".into(), json!(id));
    }
    if let Some(phase) = phase.filter(|p| !p.is_empty()) {
        payload.insert("phase".into(), json!(phase));
    }
    Value::Object(payload).to_string()
}

struct ParsedTextSignature {
    id: String,
    phase: Option<String>,
}

fn parse_text_signature(signature: Option<&str>) -> Option<ParsedTextSignature> {
    let signature = signature.filter(|s| !s.is_empty())?;
    if signature.starts_with('{')
        && let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(signature)
        && parsed.get("v") == Some(&json!(1))
        && let Some(Value::String(id)) = parsed.get("id")
    {
        let phase = match parsed.get("phase").and_then(Value::as_str) {
            Some(p @ ("commentary" | "final_answer")) => Some(p.to_string()),
            _ => None,
        };
        return Some(ParsedTextSignature {
            id: id.clone(),
            phase,
        });
    }
    Some(ParsedTextSignature {
        id: signature.to_string(),
        phase: None,
    })
}

fn image_data_url(image: &ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data)
}

fn convert_tool_result_output(model: &Model, content: &[UserContentBlock]) -> Value {
    let text_result = content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&ImageContent> = content.iter().filter_map(|c| c.as_image()).collect();
    let removed = content.iter().filter_map(|c| c.as_text()).any(|t| t.lone_surrogates_removed);
    let has_text = !crate::types::is_blank_text(&text_result, removed);
    if images.is_empty() || !model.supports_images() {
        let text = if has_text {
            text_result
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return Value::String(sanitize_surrogates(&text));
    }
    let mut output = Vec::new();
    if has_text {
        output.push(json!({ "type": "input_text", "text": sanitize_surrogates(&text_result) }));
    }
    for image in images {
        output.push(
            json!({ "type": "input_image", "detail": "auto", "image_url": image_data_url(image) }),
        );
    }
    Value::Array(output)
}

/// Default for a function tool's `strict` field when constrained sampling does not decide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultToolStrict {
    /// `strict: false` (the TS default when `strict` is undefined).
    #[default]
    False,
    /// `strict: null`.
    Null,
    True,
}

#[derive(Debug, Clone, Default)]
pub struct ConvertResponsesToolsOptions {
    pub strict: DefaultToolStrict,
    /// Defaults to true.
    pub supports_strict_mode: Option<bool>,
    pub supports_openai_grammar_tools: bool,
    pub tool_search_result: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ConvertResponsesMessagesOptions<'a> {
    /// Defaults to true.
    pub include_system_prompt: Option<bool>,
    pub grammar_tool_input_properties: Option<&'a HashMap<String, String>>,
    /// Whether later system messages are sent in place; otherwise they are folded into the leading prompt.
    pub supports_mid_convo_system_messages: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub tool_options: ConvertResponsesToolsOptions,
    /// Effort for the reply being generated, sent as `configuration_update` items.
    pub configuration_update_effort: Option<String>,
}

// =============================================================================
// Message conversion
// =============================================================================

/// How a Responses transport expresses tool changes.
pub fn get_responses_tool_support(
    supports_mid_convo_system_messages: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
) -> TranscriptToolSupport {
    if supports_mid_convo_system_messages && supports_additional_tools {
        return TranscriptToolSupport::Changes;
    }
    if supports_tool_search {
        TranscriptToolSupport::Additions
    } else {
        TranscriptToolSupport::None
    }
}

fn render_incremental_tools_notice(unavailable: &[String]) -> String {
    let notice = "This is an incremental tools update. Previously declared tools remain available unless explicitly marked unavailable. A tool declared again here is available again, and its latest definition replaces the earlier one.";
    if unavailable.is_empty() {
        notice.to_string()
    } else {
        format!("{notice}\nStill unavailable: {}.", unavailable.join(", "))
    }
}

fn render_tool_removal_notice(names: &[String]) -> String {
    format!(
        "The following tools are no longer available. Do not call them:\n{}",
        names
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .encode_utf16()
        .map(|u| {
            let c = u as u32;
            if c < 128
                && ((c as u8).is_ascii_alphanumeric() || c == b'_' as u32 || c == b'-' as u32)
            {
                u
            } else {
                b'_' as u16
            }
        })
        .take(64)
        .collect::<Vec<u16>>()
        .into_iter()
        .map(|u| u as u8 as char)
        .collect();
    sanitized.trim_end_matches('_').to_string()
}

fn build_foreign_responses_item_id(item_id: &str) -> String {
    let normalized = format!("fc_{}", short_hash(item_id));
    normalized.chars().take(64).collect()
}

/// Insertion-ordered set (JS `Set` iteration order).
#[derive(Default)]
struct OrderedSet(Vec<String>);

impl OrderedSet {
    fn insert(&mut self, value: &str) {
        if !self.0.iter().any(|v| v == value) {
            self.0.push(value.to_string());
        }
    }
    fn remove(&mut self, value: &str) {
        self.0.retain(|v| v != value);
    }
}

/// Convert a transcript to Responses `input` items.
pub fn convert_responses_messages(
    model: &Model,
    context: &TranscriptContext,
    allowed_tool_call_providers: &[&str],
    options: &ConvertResponsesMessagesOptions,
) -> anyhow::Result<Vec<Value>> {
    let normalized_context =
        resolve_transcript(context, Some(options.supports_mid_convo_system_messages));
    let mut messages: Vec<Value> = Vec::new();

    let provider_allowed = allowed_tool_call_providers.contains(&model.provider.as_str());
    let normalize_tool_call_id = |id: &str, _target: &Model, source: &AssistantMessage| -> String {
        if !provider_allowed || !id.contains('|') {
            return normalize_id_part(id);
        }
        let mut parts = id.split('|');
        let call_id = parts.next().unwrap_or_default();
        let item_id = parts.next().unwrap_or_default();
        let normalized_call_id = normalize_id_part(call_id);
        let is_foreign = source.provider != model.provider || source.api != model.api;
        let mut normalized_item_id = if is_foreign {
            build_foreign_responses_item_id(item_id)
        } else {
            normalize_id_part(item_id)
        };
        if !normalized_item_id.starts_with("fc_") {
            normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
        }
        format!("{normalized_call_id}|{normalized_item_id}")
    };

    let transformed = transform_messages(
        &normalized_context.messages,
        model,
        Some(&normalize_tool_call_id),
    );
    let tool_support = get_responses_tool_support(
        options.supports_mid_convo_system_messages,
        options.supports_additional_tools,
        options.supports_tool_search,
    );
    let transcript_tools = resolve_transcript_tools(&normalized_context.messages, tool_support);
    let compat = model.compat();
    let instruction_role = if model.reasoning && compat.supports_developer_role != Some(false) {
        "developer"
    } else {
        "system"
    };

    let mut declared_additional_tools = false;
    let mut unavailable_tools = OrderedSet::default();

    let mut append_system_tool_additions = |messages: &mut Vec<Value>,
                                            unavailable: &mut OrderedSet,
                                            message: &SystemMessage,
                                            seed: &str|
     -> anyhow::Result<()> {
        let tools: &[Tool] = message.tools_added.as_deref().unwrap_or(&[]);
        for tool in tools {
            unavailable.remove(&tool.name);
        }
        if tools.is_empty() {
            return Ok(());
        }
        match tool_support {
            TranscriptToolSupport::Changes => {
                if declared_additional_tools {
                    messages.push(json!({ "role": instruction_role, "content": render_incremental_tools_notice(&unavailable.0) }));
                }
                declared_additional_tools = true;
                messages.push(json!({
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": convert_responses_tools(tools, &options.tool_options)?,
                }));
            }
            TranscriptToolSupport::Additions => {
                let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
                let call_id = format!(
                    "pi_tool_load_{}",
                    short_hash(&format!("{seed}:{}", names.join(",")))
                );
                messages.push(json!({
                    "type": "tool_search_call",
                    "call_id": call_id,
                    "execution": "client",
                    "status": "completed",
                    "arguments": { "query": names.join(" "), "limit": names.len() },
                }));
                let tool_options = ConvertResponsesToolsOptions {
                    tool_search_result: true,
                    ..options.tool_options.clone()
                };
                messages.push(json!({
                    "type": "tool_search_output",
                    "call_id": call_id,
                    "execution": "client",
                    "status": "completed",
                    "tools": convert_responses_tools(tools, &tool_options)?,
                }));
            }
            TranscriptToolSupport::None => {}
        }
        Ok(())
    };

    let include_initial_system_message = options.include_system_prompt.unwrap_or(true);
    let configuration_update =
        |effort: &str| json!({ "type": "configuration_update", "reasoning": { "effort": effort } });
    let grammar_props = options.grammar_tool_input_properties;

    let mut msg_index = 0usize;
    let mut system_index = 0usize;
    for (source_index, msg) in transformed.iter().enumerate() {
        let is_leading_system_message = source_index == 0 && matches!(msg, Message::System(_));
        match msg {
            Message::System(system) => {
                let anchors_tool_changes = system_index >= transcript_tools.folded_system_messages;
                system_index += 1;
                if anchors_tool_changes {
                    append_system_tool_additions(
                        &mut messages,
                        &mut unavailable_tools,
                        system,
                        &format!("system:{msg_index}"),
                    )?;
                }
                if !is_leading_system_message || include_initial_system_message {
                    // Each part with upstream's emptiness before `sanitizeSurrogates`.
                    let mut parts = vec![if is_leading_system_message {
                        (get_system_message_text(system), get_system_message_text_is_empty(system))
                    } else {
                        (render_system_message_update(system), render_system_message_update_is_empty(system))
                    }];
                    if anchors_tool_changes {
                        let added: HashSet<&str> = system
                            .tools_added
                            .iter()
                            .flatten()
                            .map(|t| t.name.as_str())
                            .collect();
                        let removed: Vec<String> = system
                            .tools_removed
                            .iter()
                            .flatten()
                            .map(|t| t.name.clone())
                            .filter(|name| !added.contains(name.as_str()))
                            .collect();
                        if !removed.is_empty() {
                            let notice = render_tool_removal_notice(&removed);
                            let empty = notice.is_empty();
                            parts.push((notice, empty));
                        }
                        for name in &removed {
                            unavailable_tools.insert(name);
                        }
                    }
                    let parts: Vec<String> =
                        parts.into_iter().filter(|(_, empty)| !empty).map(|(part, _)| part).collect();
                    let text = parts.join("\n\n");
                    if !parts.is_empty() {
                        messages.push(json!({ "role": instruction_role, "content": sanitize_surrogates(&text) }));
                    }
                }
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": sanitize_surrogates(text) }],
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            UserContentBlock::Text(t) => json!({ "type": "input_text", "text": sanitize_surrogates(&t.text) }),
                            UserContentBlock::Image(i) => {
                                json!({ "type": "input_image", "detail": "auto", "image_url": image_data_url(i) })
                            }
                        })
                        .collect();
                    if content.is_empty() {
                        continue;
                    }
                    messages.push(json!({ "role": "user", "content": content }));
                }
            },
            Message::Assistant(assistant) => {
                let mut output: Vec<Value> = Vec::new();
                let same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let is_same_model = same_provider_and_api && assistant.model == model.id;
                let is_different_model = same_provider_and_api && assistant.model != model.id;
                let mut text_block_index = 0usize;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if let Some(signature) = thinking
                                .thinking_signature
                                .as_deref()
                                .filter(|s| !s.is_empty())
                            {
                                let item: Value = crate::utils::js_json::js_json_parse(signature).map_err(anyhow::Error::msg)?;
                                output.push(item);
                            }
                        }
                        AssistantContent::Text(text) => {
                            let parsed = parse_text_signature(text.text_signature.as_deref());
                            let fallback = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let msg_id = match parsed
                                .as_ref()
                                .map(|p| p.id.as_str())
                                .filter(|id| !id.is_empty())
                            {
                                None => fallback,
                                Some(id) if id.encode_utf16().count() > 64 => {
                                    format!("msg_{}", short_hash(id))
                                }
                                Some(id) => id.to_string(),
                            };
                            let mut item = Map::new();
                            item.insert("type".into(), json!("message"));
                            item.insert("role".into(), json!("assistant"));
                            item.insert(
                                "content".into(),
                                json!([{ "type": "output_text", "text": sanitize_surrogates(&text.text), "annotations": [] }]),
                            );
                            item.insert("status".into(), json!("completed"));
                            item.insert("id".into(), json!(msg_id));
                            if let Some(phase) = parsed.and_then(|p| p.phase) {
                                item.insert("phase".into(), json!(phase));
                            }
                            output.push(Value::Object(item));
                        }
                        AssistantContent::ToolCall(call) => {
                            let mut split = call.id.split('|');
                            let call_id = split.next().unwrap_or_default();
                            let item_id_raw = split.next();
                            let custom_input_property =
                                grammar_props.and_then(|p| p.get(&call.name));
                            let prefix = if custom_input_property.is_none() {
                                "fc_"
                            } else {
                                "ctc_"
                            };
                            let item_id = item_id_raw
                                .filter(|id| !is_different_model && id.starts_with(prefix));
                            let mut item = Map::new();
                            if let Some(property) = custom_input_property {
                                item.insert("type".into(), json!("custom_tool_call"));
                                if let Some(id) = item_id {
                                    item.insert("id".into(), json!(id));
                                }
                                item.insert("call_id".into(), json!(call_id));
                                item.insert("name".into(), json!(call.name));
                                item.insert(
                                    "input".into(),
                                    json!(sanitize_surrogates(&get_grammar_tool_input(
                                        &call.name,
                                        &call.arguments,
                                        property
                                    )?)),
                                );
                            } else {
                                item.insert("type".into(), json!("function_call"));
                                if let Some(id) = item_id {
                                    item.insert("id".into(), json!(id));
                                }
                                item.insert("call_id".into(), json!(call_id));
                                item.insert("name".into(), json!(call.name));
                                item.insert(
                                    "arguments".into(),
                                    json!(Value::Object(call.arguments.clone()).to_string()),
                                );
                            }
                            if is_same_model && let Some(namespace) = &call.namespace {
                                item.insert("namespace".into(), json!(namespace));
                            }
                            output.push(Value::Object(item));
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                if options.configuration_update_effort.is_some()
                    && is_same_model
                    && let Some(level) = &assistant.provider_thinking_level
                {
                    messages.push(configuration_update(level));
                }
                messages.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
                let output = convert_tool_result_output(model, &result.content);
                let kind = if grammar_props.is_some_and(|p| p.contains_key(&result.tool_name)) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                messages.push(json!({ "type": kind, "call_id": call_id, "output": output }));
            }
        }
        if !is_leading_system_message {
            msg_index += 1;
        }
    }
    if let Some(effort) = &options.configuration_update_effort {
        messages.push(configuration_update(effort));
    }
    Ok(messages)
}

// =============================================================================
// Tool conversion
// =============================================================================

/// Convert tools to Responses tool definitions.
pub fn convert_responses_tools(
    tools: &[Tool],
    options: &ConvertResponsesToolsOptions,
) -> anyhow::Result<Vec<Value>> {
    let supports_strict_mode = options.supports_strict_mode.unwrap_or(true);
    let mut result = Vec::with_capacity(tools.len());
    for tool in tools {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, options.supports_openai_grammar_tools)?
        {
            let mut definition = Map::new();
            definition.insert("type".into(), json!("custom"));
            definition.insert("name".into(), json!(tool.name));
            definition.insert("description".into(), json!(tool.description));
            definition.insert(
                "format".into(),
                json!({ "type": "grammar", "syntax": grammar.format, "definition": grammar.definition }),
            );
            if options.tool_search_result {
                definition.insert("defer_loading".into(), json!(true));
            }
            result.push(Value::Object(definition));
            continue;
        }
        let constrained_strict =
            resolve_json_schema_strict_sampling(tool, supports_strict_mode, None)?;
        let strict: Value = match constrained_strict {
            Some(value) => json!(value),
            None => match options.strict {
                DefaultToolStrict::False => json!(false),
                DefaultToolStrict::True => json!(true),
                DefaultToolStrict::Null => Value::Null,
            },
        };
        let mut definition = Map::new();
        definition.insert("type".into(), json!("function"));
        definition.insert("name".into(), json!(tool.name));
        definition.insert("description".into(), json!(tool.description));
        definition.insert(
            "parameters".into(),
            get_json_schema_tool_parameters(tool, Some(strict == json!(true))),
        );
        if options.tool_search_result {
            definition.insert("defer_loading".into(), json!(true));
        }
        if supports_strict_mode {
            definition.insert("strict".into(), strict);
        }
        result.push(Value::Object(definition));
    }
    Ok(result)
}

// =============================================================================
// Stream processing
// =============================================================================

/// Resolves the billed service tier from the response's and the request's tier.
pub type ResolveServiceTier = fn(Option<&str>, Option<&str>) -> Option<String>;
/// Applies service-tier pricing to usage.
pub type ApplyServiceTierPricing<'a> = &'a (dyn Fn(&mut Usage, Option<&str>) + Send + Sync);

#[derive(Default, Clone)]
pub struct OpenAIResponsesStreamOptions<'a> {
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    pub service_tier: Option<String>,
    pub grammar_tool_input_properties: Option<&'a HashMap<String, String>>,
    pub resolve_service_tier: Option<ResolveServiceTier>,
    pub apply_service_tier_pricing: Option<ApplyServiceTierPricing<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    kind: SlotKind,
    content_index: usize,
}

/// Streaming scratch state of an unfinished tool call (the TS `partialJson`/`customInput` fields).
#[derive(Debug, Clone, Default)]
struct ToolScratch {
    partial_json: Option<String>,
    custom_input: Option<(String, GrammarToolInputJsonBuffer)>,
}

fn str_of<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

fn js_display(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn truthy_str(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn u64_of(value: Option<&Value>) -> u64 {
    value
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
        .unwrap_or(0)
}

fn output_index_key(event: &Value) -> String {
    match event.get("output_index") {
        None => "undefined".to_string(),
        Some(value) => value.to_string(),
    }
}

/// Incremental processor for Responses stream events (the body of `processResponsesStream`).
pub struct ResponsesStreamProcessor<'a> {
    model: &'a Model,
    options: OpenAIResponsesStreamOptions<'a>,
    saw_terminal_response_event: bool,
    slots: HashMap<String, Slot>,
    scratch: HashMap<usize, ToolScratch>,
    reasoning_blocks_by_id: HashMap<String, usize>,
}

impl<'a> ResponsesStreamProcessor<'a> {
    pub fn new(model: &'a Model, options: OpenAIResponsesStreamOptions<'a>) -> Self {
        Self {
            model,
            options,
            saw_terminal_response_event: false,
            slots: HashMap::new(),
            scratch: HashMap::new(),
            reasoning_blocks_by_id: HashMap::new(),
        }
    }

    fn apply_message_phase_stop_reason(item: &Value, output: &mut AssistantMessage) {
        if str_of(item, "type") == Some("message") && str_of(item, "phase") == Some("final_answer")
        {
            output.stop_reason = StopReason::Stop;
        }
    }

    fn get_slot(&self, key: &str, kind: SlotKind) -> Option<Slot> {
        self.slots.get(key).copied().filter(|s| s.kind == kind)
    }

    fn create_slot(
        &mut self,
        key: &str,
        item: &Value,
        output: &mut AssistantMessage,
        sender: &AssistantMessageEventSender,
    ) -> Option<Slot> {
        match str_of(item, "type") {
            Some("reasoning") => {
                output
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent::new("")));
                let slot = Slot {
                    kind: SlotKind::Thinking,
                    content_index: output.content.len() - 1,
                };
                self.slots.insert(key.to_string(), slot);
                sender.push(AssistantMessageEvent::ThinkingStart {
                    content_index: slot.content_index,
                    partial: output.clone(),
                });
                Some(slot)
            }
            Some("message") => {
                Self::apply_message_phase_stop_reason(item, output);
                output
                    .content
                    .push(AssistantContent::Text(TextContent::new("")));
                let slot = Slot {
                    kind: SlotKind::Text,
                    content_index: output.content.len() - 1,
                };
                self.slots.insert(key.to_string(), slot);
                sender.push(AssistantMessageEvent::TextStart {
                    content_index: slot.content_index,
                    partial: output.clone(),
                });
                Some(slot)
            }
            Some("function_call") => {
                output.content.push(AssistantContent::ToolCall(ToolCall {
                    id: format!(
                        "{}|{}",
                        js_display(item.get("call_id")),
                        js_display(item.get("id"))
                    ),
                    name: str_of(item, "name").unwrap_or_default().to_string(),
                    arguments: Map::new(),
                    namespace: str_of(item, "namespace").map(str::to_string),
                    ..Default::default()
                }));
                let slot = Slot {
                    kind: SlotKind::ToolCall,
                    content_index: output.content.len() - 1,
                };
                self.scratch.insert(
                    slot.content_index,
                    ToolScratch {
                        partial_json: Some(
                            str_of(item, "arguments").unwrap_or_default().to_string(),
                        ),
                        custom_input: None,
                    },
                );
                self.slots.insert(key.to_string(), slot);
                sender.push(AssistantMessageEvent::ToolCallStart {
                    content_index: slot.content_index,
                    partial: output.clone(),
                });
                Some(slot)
            }
            Some("custom_tool_call") => {
                let name = str_of(item, "name").unwrap_or_default().to_string();
                let input_property = self
                    .options
                    .grammar_tool_input_properties
                    .and_then(|p| p.get(&name))
                    .cloned()
                    .unwrap_or_else(|| "input".to_string());
                let input = str_of(item, "input").unwrap_or_default().to_string();
                let mut arguments = Map::new();
                arguments.insert(input_property.clone(), json!(input));
                output.content.push(AssistantContent::ToolCall(ToolCall {
                    id: format!(
                        "{}|{}",
                        js_display(item.get("call_id")),
                        js_display(item.get("id"))
                    ),
                    name,
                    arguments,
                    namespace: str_of(item, "namespace").map(str::to_string),
                    ..Default::default()
                }));
                let slot = Slot {
                    kind: SlotKind::ToolCall,
                    content_index: output.content.len() - 1,
                };
                self.scratch.insert(
                    slot.content_index,
                    ToolScratch {
                        partial_json: None,
                        custom_input: Some((input_property, GrammarToolInputJsonBuffer::default())),
                    },
                );
                self.slots.insert(key.to_string(), slot);
                sender.push(AssistantMessageEvent::ToolCallStart {
                    content_index: slot.content_index,
                    partial: output.clone(),
                });
                Some(slot)
            }
            _ => None,
        }
    }

    fn custom_tool_call_input(output: &AssistantMessage, slot: Slot, property: &str) -> String {
        match output.content.get(slot.content_index) {
            Some(AssistantContent::ToolCall(call)) => call
                .arguments
                .get(property)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        }
    }

    /// `appendCustomToolCallInput`: returns the JSON delta, if any.
    fn append_custom_tool_call_input(
        &mut self,
        output: &mut AssistantMessage,
        slot: Slot,
        next_input: &str,
        close: bool,
    ) -> anyhow::Result<Option<String>> {
        let Some((property, buffer)) = self
            .scratch
            .get_mut(&slot.content_index)
            .and_then(|s| s.custom_input.as_mut())
        else {
            return Ok(None);
        };
        let delta = append_grammar_tool_input_json_delta(buffer, property, next_input, close)?;
        if let Some(AssistantContent::ToolCall(call)) = output.content.get_mut(slot.content_index) {
            let mut arguments = Map::new();
            arguments.insert(property.clone(), json!(next_input));
            call.arguments = arguments;
        }
        Ok(delta)
    }

    fn push_tool_call_delta(
        output: &AssistantMessage,
        sender: &AssistantMessageEventSender,
        slot: Slot,
        delta: Option<String>,
    ) {
        if let Some(delta) = delta {
            sender.push(AssistantMessageEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta,
                partial: output.clone(),
            });
        }
    }

    fn backfill_reasoning_signatures(
        &self,
        output: &mut AssistantMessage,
        response_output: &[Value],
    ) {
        for item in response_output {
            if str_of(item, "type") != Some("reasoning") {
                continue;
            }
            let Some(encrypted) = item.get("encrypted_content").filter(|v| truthy(v)) else {
                continue;
            };
            let Some(id) = str_of(item, "id") else {
                continue;
            };
            let Some(&index) = self.reasoning_blocks_by_id.get(id) else {
                continue;
            };
            let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) else {
                continue;
            };
            let Some(signature) = block
                .thinking_signature
                .as_deref()
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Ok(Value::Object(mut stored)) = serde_json::from_str::<Value>(signature) else {
                continue;
            };
            if stored.get("encrypted_content").is_some_and(truthy) {
                continue;
            }
            stored.insert("encrypted_content".into(), encrypted.clone());
            block.thinking_signature = Some(Value::Object(stored).to_string());
        }
    }

    fn finalize_response(
        &mut self,
        output: &mut AssistantMessage,
        response: &Value,
    ) -> anyhow::Result<()> {
        self.saw_terminal_response_event = true;
        if let Some(Value::Array(items)) = response.get("output") {
            self.backfill_reasoning_signatures(output, items);
        }
        if let Some(id) = truthy_str(response.get("id")) {
            output.response_id = Some(id);
        }
        if let Some(usage) = response.get("usage").filter(|u| truthy(u)) {
            let details = usage.get("input_tokens_details");
            let cached = u64_of(details.and_then(|d| d.get("cached_tokens")));
            let cache_write = u64_of(details.and_then(|d| d.get("cache_write_tokens")));
            output.usage = Usage {
                input: u64_of(usage.get("input_tokens"))
                    .saturating_sub(cached)
                    .saturating_sub(cache_write),
                output: u64_of(usage.get("output_tokens")),
                cache_read: cached,
                cache_write,
                reasoning: Some(u64_of(
                    usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens")),
                )),
                total_tokens: u64_of(usage.get("total_tokens")),
                ..Default::default()
            };
        }
        calculate_cost(&self.model.cost, &mut output.usage);
        if let Some(apply) = self.options.apply_service_tier_pricing {
            let response_tier = str_of(response, "service_tier");
            let request_tier = self.options.service_tier.as_deref();
            let tier = match self.options.resolve_service_tier {
                Some(resolve) => resolve(response_tier, request_tier),
                None => response_tier.or(request_tier).map(str::to_string),
            };
            apply(&mut output.usage, tier.as_deref());
        }
        let status = response.get("status").and_then(Value::as_str);
        let incomplete_reason = response
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty());
        output.raw_stop_reason = match (status, incomplete_reason) {
            (status, Some(reason)) => Some(format!("{}.{reason}", status.unwrap_or("undefined"))),
            (status, None) => status.map(str::to_string),
        };
        let (stop_reason, error_message) = map_stop_reason(status, incomplete_reason)?;
        output.stop_reason = stop_reason;
        output.error_message = error_message;
        if output.stop_reason == StopReason::Stop
            && output
                .content
                .iter()
                .any(|b| matches!(b, AssistantContent::ToolCall(_)))
        {
            output.stop_reason = StopReason::ToolUse;
        }
        Ok(())
    }

    /// Await the provider stream-event observer for one raw event. Call before
    /// [`Self::handle_event`].
    pub async fn observe(&self, event: &Value) {
        if let Some(observer) = &self.options.on_provider_stream_event {
            observer(event.clone(), self.model.clone()).await;
        }
    }

    /// Process one parsed stream event (after [`Self::observe`]).
    pub fn handle_event(
        &mut self,
        event: &Value,
        output: &mut AssistantMessage,
        sender: &AssistantMessageEventSender,
    ) -> anyhow::Result<()> {
        let event_type = str_of(event, "type").unwrap_or_default();
        let key = output_index_key(event);
        match event_type {
            "response.created" => {
                output.response_id = event
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    self.create_slot(&key, item, output, sender);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let Some(slot) = self.get_slot(&key, SlotKind::Thinking) else {
                    return Ok(());
                };
                let delta = js_display(event.get("delta"));
                if let Some(AssistantContent::Thinking(block)) =
                    output.content.get_mut(slot.content_index)
                {
                    block.thinking.push_str(&delta);
                }
                sender.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: slot.content_index,
                    delta,
                    partial: output.clone(),
                });
            }
            "response.reasoning_summary_part.done" => {
                let Some(slot) = self.get_slot(&key, SlotKind::Thinking) else {
                    return Ok(());
                };
                if let Some(AssistantContent::Thinking(block)) =
                    output.content.get_mut(slot.content_index)
                {
                    block.thinking.push_str("\n\n");
                }
                sender.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: slot.content_index,
                    delta: "\n\n".into(),
                    partial: output.clone(),
                });
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let Some(slot) = self.get_slot(&key, SlotKind::Text) else {
                    return Ok(());
                };
                let delta = js_display(event.get("delta"));
                if let Some(AssistantContent::Text(block)) =
                    output.content.get_mut(slot.content_index)
                {
                    block.text.push_str(&delta);
                }
                sender.push(AssistantMessageEvent::TextDelta {
                    content_index: slot.content_index,
                    delta,
                    partial: output.clone(),
                });
            }
            "response.function_call_arguments.delta" => {
                let Some(slot) = self.get_slot(&key, SlotKind::ToolCall) else {
                    return Ok(());
                };
                let Some(partial) = self
                    .scratch
                    .get_mut(&slot.content_index)
                    .and_then(|s| s.partial_json.as_mut())
                else {
                    return Ok(());
                };
                let delta = js_display(event.get("delta"));
                partial.push_str(&delta);
                let arguments = parse_streaming_json_object(Some(partial));
                if let Some(AssistantContent::ToolCall(call)) =
                    output.content.get_mut(slot.content_index)
                {
                    call.arguments = arguments;
                }
                Self::push_tool_call_delta(output, sender, slot, Some(delta));
            }
            "response.function_call_arguments.done" => {
                let Some(slot) = self.get_slot(&key, SlotKind::ToolCall) else {
                    return Ok(());
                };
                let Some(partial) = self
                    .scratch
                    .get_mut(&slot.content_index)
                    .and_then(|s| s.partial_json.as_mut())
                else {
                    return Ok(());
                };
                let previous = std::mem::take(partial);
                let arguments_text = js_display(event.get("arguments"));
                *partial = arguments_text.clone();
                let arguments = parse_streaming_json_object(Some(&arguments_text));
                if let Some(AssistantContent::ToolCall(call)) =
                    output.content.get_mut(slot.content_index)
                {
                    call.arguments = arguments;
                }
                if let Some(delta) = arguments_text.strip_prefix(previous.as_str())
                    && !delta.is_empty()
                {
                    Self::push_tool_call_delta(output, sender, slot, Some(delta.to_string()));
                }
            }
            "response.custom_tool_call_input.delta" => {
                let Some(slot) = self.get_slot(&key, SlotKind::ToolCall) else {
                    return Ok(());
                };
                let Some(property) = self
                    .scratch
                    .get(&slot.content_index)
                    .and_then(|s| s.custom_input.as_ref())
                    .map(|(p, _)| p.clone())
                else {
                    return Ok(());
                };
                let next = Self::custom_tool_call_input(output, slot, &property)
                    + &js_display(event.get("delta"));
                let delta = self.append_custom_tool_call_input(output, slot, &next, false)?;
                Self::push_tool_call_delta(output, sender, slot, delta);
            }
            "response.custom_tool_call_input.done" => {
                let Some(slot) = self.get_slot(&key, SlotKind::ToolCall) else {
                    return Ok(());
                };
                if self
                    .scratch
                    .get(&slot.content_index)
                    .and_then(|s| s.custom_input.as_ref())
                    .is_none()
                {
                    return Ok(());
                }
                let input = js_display(event.get("input"));
                let delta = self.append_custom_tool_call_input(output, slot, &input, true)?;
                Self::push_tool_call_delta(output, sender, slot, delta);
            }
            "response.output_item.done" => {
                let Some(item) = event.get("item") else {
                    return Ok(());
                };
                Self::apply_message_phase_stop_reason(item, output);
                let slot = match self.slots.get(&key).copied() {
                    Some(slot) => Some(slot),
                    None => self.create_slot(&key, item, output, sender),
                };
                let Some(slot) = slot else { return Ok(()) };
                match (str_of(item, "type"), slot.kind) {
                    (Some("reasoning"), SlotKind::Thinking) => {
                        let join = |field: &str| -> String {
                            match item.get(field) {
                                Some(Value::Array(parts)) => parts
                                    .iter()
                                    .map(|p| {
                                        p.get("text").and_then(Value::as_str).unwrap_or_default()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n\n"),
                                _ => String::new(),
                            }
                        };
                        let summary_text = join("summary");
                        let content_text = join("content");
                        let content = if let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(slot.content_index)
                        {
                            if !summary_text.is_empty() {
                                block.thinking = summary_text;
                            } else if !content_text.is_empty() {
                                block.thinking = content_text;
                            }
                            block.thinking_signature = Some(item.to_string());
                            block.thinking.clone()
                        } else {
                            String::new()
                        };
                        if let Some(id) = str_of(item, "id") {
                            self.reasoning_blocks_by_id
                                .insert(id.to_string(), slot.content_index);
                        }
                        sender.push(AssistantMessageEvent::ThinkingEnd {
                            content_index: slot.content_index,
                            content,
                            partial: output.clone(),
                        });
                        self.slots.remove(&key);
                    }
                    (Some("message"), SlotKind::Text) => {
                        let text = match item.get("content") {
                            Some(Value::Array(parts)) => parts
                                .iter()
                                .map(|c| {
                                    let field = if str_of(c, "type") == Some("output_text") {
                                        "text"
                                    } else {
                                        "refusal"
                                    };
                                    c.get(field).and_then(Value::as_str).unwrap_or_default()
                                })
                                .collect::<Vec<_>>()
                                .join(""),
                            _ => String::new(),
                        };
                        let signature =
                            encode_text_signature_v1(str_of(item, "id"), str_of(item, "phase"));
                        if let Some(AssistantContent::Text(block)) =
                            output.content.get_mut(slot.content_index)
                        {
                            block.text = text.clone();
                            block.text_signature = Some(signature);
                        }
                        sender.push(AssistantMessageEvent::TextEnd {
                            content_index: slot.content_index,
                            content: text,
                            partial: output.clone(),
                        });
                        self.slots.remove(&key);
                    }
                    (Some("function_call"), SlotKind::ToolCall) => {
                        let Some(partial) = self
                            .scratch
                            .get(&slot.content_index)
                            .and_then(|s| s.partial_json.clone())
                        else {
                            return Ok(());
                        };
                        let arguments_text = str_of(item, "arguments")
                            .filter(|a| !a.is_empty())
                            .map(str::to_string)
                            .unwrap_or(partial);
                        let parsed = parse_tool_call_arguments(Some(&arguments_text));
                        let tool_call = if let Some(AssistantContent::ToolCall(call)) =
                            output.content.get_mut(slot.content_index)
                        {
                            call.arguments = parsed.arguments;
                            if parsed.arguments_error.is_some() {
                                call.arguments_error = parsed.arguments_error;
                            }
                            if let Some(namespace) = str_of(item, "namespace") {
                                call.namespace = Some(namespace.to_string());
                            }
                            call.clone()
                        } else {
                            return Ok(());
                        };
                        self.scratch.remove(&slot.content_index);
                        sender.push(AssistantMessageEvent::ToolCallEnd {
                            content_index: slot.content_index,
                            tool_call,
                            partial: output.clone(),
                        });
                        self.slots.remove(&key);
                    }
                    (Some("custom_tool_call"), SlotKind::ToolCall) => {
                        let Some(property) = self
                            .scratch
                            .get(&slot.content_index)
                            .and_then(|s| s.custom_input.as_ref())
                            .map(|(p, _)| p.clone())
                        else {
                            return Ok(());
                        };
                        let input = match item.get("input") {
                            Some(Value::Null) | None => {
                                Self::custom_tool_call_input(output, slot, &property)
                            }
                            Some(value) => js_display(Some(value)),
                        };
                        let delta =
                            self.append_custom_tool_call_input(output, slot, &input, true)?;
                        Self::push_tool_call_delta(output, sender, slot, delta);
                        let tool_call = if let Some(AssistantContent::ToolCall(call)) =
                            output.content.get_mut(slot.content_index)
                        {
                            if let Some(namespace) = str_of(item, "namespace") {
                                call.namespace = Some(namespace.to_string());
                            }
                            call.clone()
                        } else {
                            return Ok(());
                        };
                        self.scratch.remove(&slot.content_index);
                        sender.push(AssistantMessageEvent::ToolCallEnd {
                            content_index: slot.content_index,
                            tool_call,
                            partial: output.clone(),
                        });
                        self.slots.remove(&key);
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                self.finalize_response(output, &response)?;
            }
            "error" => {
                anyhow::bail!(
                    "Error Code {}: {}",
                    js_display(event.get("code")),
                    js_display(event.get("message"))
                );
            }
            "response.failed" => {
                self.saw_terminal_response_event = true;
                let response = event.get("response");
                output.raw_stop_reason = response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let error = response.and_then(|r| r.get("error")).filter(|e| truthy(e));
                let details = response.and_then(|r| r.get("incomplete_details"));
                let message = if let Some(error) = error {
                    format!(
                        "{}: {}",
                        truthy_str(error.get("code")).unwrap_or_else(|| "unknown".into()),
                        truthy_str(error.get("message")).unwrap_or_else(|| "no message".into())
                    )
                } else if let Some(reason) = details.and_then(|d| truthy_str(d.get("reason"))) {
                    format!("incomplete: {reason}")
                } else {
                    "Unknown error (no error details in response)".to_string()
                };
                anyhow::bail!("{message}");
            }
            _ => {}
        }
        Ok(())
    }

    /// Checks after the event stream ended.
    pub fn finish(&self, output: &AssistantMessage) -> anyhow::Result<()> {
        if !self.saw_terminal_response_event {
            anyhow::bail!("OpenAI Responses stream ended before a terminal response event");
        }
        if output.stop_reason == StopReason::ToolUse {
            for (index, block) in output.content.iter().enumerate() {
                let AssistantContent::ToolCall(call) = block else {
                    continue;
                };
                if self.scratch.contains_key(&index) {
                    anyhow::bail!(
                        "OpenAI Responses stream completed with an unfinished tool call: {} ({})",
                        call.name,
                        call.id
                    );
                }
            }
        }
        Ok(())
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// Consume a stream of Responses events into `output`, pushing assistant events.
pub async fn process_responses_stream<S>(
    mut events: S,
    output: &mut AssistantMessage,
    sender: &AssistantMessageEventSender,
    model: &Model,
    options: OpenAIResponsesStreamOptions<'_>,
) -> anyhow::Result<()>
where
    S: Stream<Item = anyhow::Result<Value>> + Unpin,
{
    let mut processor = ResponsesStreamProcessor::new(model, options);
    while let Some(event) = events.next().await {
        let event = event?;
        processor.observe(&event).await;
        processor.handle_event(&event, output, sender)?;
    }
    processor.finish(output)
}

/// Map a Responses status to a stop reason and optional error message.
pub fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> anyhow::Result<(StopReason, Option<String>)> {
    // `if (!status)`: a missing or empty status is a normal stop.
    let Some(status) = status.filter(|s| !s.is_empty()) else {
        return Ok((StopReason::Stop, None));
    };
    Ok(match status {
        "completed" | "in_progress" | "queued" => (StopReason::Stop, None),
        "incomplete" => match incomplete_reason {
            Some("max_output_tokens") => (StopReason::Length, None),
            Some(reason) => (
                StopReason::Error,
                Some(format!("Response incomplete: {reason}")),
            ),
            None => (
                StopReason::Error,
                Some("Response incomplete without a provider reason".to_string()),
            ),
        },
        "failed" | "cancelled" => (StopReason::Error, None),
        other => anyhow::bail!("Unhandled stop reason: {other}"),
    })
}

/// Usage pricing multiplier for OpenAI service tiers.
pub fn apply_service_tier_multiplier(usage: &mut Usage, multiplier: f64) {
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}
