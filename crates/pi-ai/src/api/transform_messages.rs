//! Cross-provider message normalization (port of `api/transform-messages.ts`).

use std::collections::{HashMap, HashSet};

use crate::types::{
    AssistantContent, AssistantMessage, ImageContent, Message, Model, ModelInputModality, StopReason, TextContent,
    ToolCall, ToolResultMessage, UserContent, UserContentBlock,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

/// An error tool result split into a text-only result and the images to send after the tool results.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitErrorToolResult {
    pub content: Vec<TextContent>,
    pub caption: String,
    pub images: Vec<ImageContent>,
}

/// Split the images out of an error tool result, for APIs that reject images in error results.
pub fn split_error_tool_result_images(msg: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !msg.is_error {
        return None;
    }
    let images: Vec<ImageContent> = msg.content.iter().filter_map(|b| b.as_image().cloned()).collect();
    if images.is_empty() {
        return None;
    }
    Some(SplitErrorToolResult {
        content: replace_images_with_placeholder(&msg.content, ERROR_TOOL_IMAGE_PLACEHOLDER),
        caption: format!("Images from the error result of tool call {} ({}):", msg.tool_call_id, msg.tool_name),
        images,
    })
}

fn replace_images_with_placeholder(content: &[UserContentBlock], placeholder: &str) -> Vec<TextContent> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserContentBlock::Image(_) => {
                if !previous_was_placeholder {
                    result.push(TextContent::new(placeholder));
                }
                previous_was_placeholder = true;
            }
            UserContentBlock::Text(text) => {
                previous_was_placeholder = text.text == placeholder;
                result.push(text.clone());
            }
        }
    }
    result
}

fn text_blocks(content: Vec<TextContent>) -> Vec<UserContentBlock> {
    content.into_iter().map(UserContentBlock::Text).collect()
}

fn downgrade_unsupported_images(messages: Vec<Message>, model: &Model) -> Vec<Message> {
    if model.input.contains(&ModelInputModality::Image) {
        return messages;
    }
    messages
        .into_iter()
        .map(|msg| match msg {
            Message::User(mut user) => {
                if let UserContent::Blocks(blocks) = &user.content {
                    user.content = UserContent::Blocks(text_blocks(replace_images_with_placeholder(
                        blocks,
                        NON_VISION_USER_IMAGE_PLACEHOLDER,
                    )));
                }
                Message::User(user)
            }
            Message::ToolResult(mut result) => {
                result.content = text_blocks(replace_images_with_placeholder(&result.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER));
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
}

/// Callback normalizing tool call ids for the target model.
pub type NormalizeToolCallId<'a> = &'a dyn Fn(&str, &Model, &AssistantMessage) -> String;

/// Normalize a transcript for `model`: downgrade unsupported images, drop foreign thinking,
/// normalize tool call ids, skip errored turns and synthesize missing tool results.
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId>,
) -> Vec<Message> {
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let image_aware = downgrade_unsupported_images(messages.to_vec(), model);

    let transformed: Vec<Message> = image_aware
        .into_iter()
        .map(|msg| match msg {
            Message::System(_) | Message::User(_) => msg,
            Message::ToolResult(mut result) => {
                if let Some(normalized) = tool_call_id_map.get(&result.tool_call_id)
                    && *normalized != result.tool_call_id {
                        result.tool_call_id = normalized.clone();
                    }
                Message::ToolResult(result)
            }
            Message::Assistant(assistant) => {
                let is_same_model =
                    assistant.provider == model.provider && assistant.api == model.api && assistant.model == model.id;
                let mut content = Vec::with_capacity(assistant.content.len());
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if !is_same_model {
                                continue;
                            }
                            if thinking.redacted == Some(true)
                                || thinking.thinking_signature.as_ref().is_some_and(|s| !s.is_empty())
                            {
                                content.push(block.clone());
                                continue;
                            }
                            if thinking.is_blank() {
                                continue;
                            }
                            content.push(block.clone());
                        }
                        AssistantContent::Text(text) => {
                            if is_same_model {
                                content.push(block.clone());
                            } else {
                                content.push(AssistantContent::Text(TextContent {
                                    text: text.text.clone(),
                                    lone_surrogates_removed: text.lone_surrogates_removed,
                                    ..Default::default()
                                }));
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let mut normalized: ToolCall = tool_call.clone();
                            if !is_same_model && tool_call.thought_signature.is_some() {
                                normalized.thought_signature = None;
                            }
                            if !is_same_model
                                && let Some(normalize) = normalize_tool_call_id {
                                    let id = normalize(&tool_call.id, model, &assistant);
                                    if id != tool_call.id {
                                        tool_call_id_map.insert(tool_call.id.clone(), id.clone());
                                        normalized.id = id;
                                    }
                                }
                            content.push(AssistantContent::ToolCall(normalized));
                        }
                    }
                }
                let mut assistant = assistant;
                assistant.content = content;
                Message::Assistant(assistant)
            }
        })
        .collect();

    let mut result: Vec<Message> = Vec::with_capacity(transformed.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut existing_ids: HashSet<String> = HashSet::new();
    let mut held_system: Vec<Message> = Vec::new();

    let close_pending =
        |result: &mut Vec<Message>, pending: &mut Vec<ToolCall>, existing: &mut HashSet<String>, held: &mut Vec<Message>| {
            if !pending.is_empty() {
                for tc in pending.iter() {
                    if !existing.contains(&tc.id) {
                        result.push(Message::ToolResult(ToolResultMessage {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: vec![UserContentBlock::text("No result provided")],
                            is_error: true,
                            timestamp: crate::utils::now_ms(),
                            ..Default::default()
                        }));
                    }
                }
                pending.clear();
                existing.clear();
            }
            result.append(held);
        };

    for msg in transformed {
        match msg {
            Message::Assistant(assistant) => {
                close_pending(&mut result, &mut pending, &mut existing_ids, &mut held_system);
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }
                let tool_calls: Vec<ToolCall> = assistant.tool_calls().cloned().collect();
                if !tool_calls.is_empty() {
                    pending = tool_calls;
                    existing_ids = HashSet::new();
                }
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                existing_ids.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
            Message::System(system) => {
                if !pending.is_empty() {
                    held_system.push(Message::System(system));
                } else {
                    result.push(Message::System(system));
                }
            }
            Message::User(user) => {
                close_pending(&mut result, &mut pending, &mut existing_ids, &mut held_system);
                result.push(Message::User(user));
            }
        }
    }
    close_pending(&mut result, &mut pending, &mut existing_ids, &mut held_system);
    result
}
