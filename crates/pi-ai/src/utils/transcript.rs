//! Transcript normalization and system-message replay (port of `utils/transcript.ts`).

use indexmap::IndexMap;

use super::text::{get_system_message_text, system_content_text};
use crate::types::{Context, Message, SystemContent, SystemMessage, Tool, ToolReference, TranscriptContext};

/// Build the leading system message for a prompt and tool set. `None` when both are empty.
pub fn create_initial_system_message(system_prompt: Option<&str>, tools: Option<&[Tool]>) -> Option<SystemMessage> {
    let has_system_prompt = system_prompt.is_some_and(|p| !p.is_empty());
    let has_tools = tools.is_some_and(|t| !t.is_empty());
    if !has_system_prompt && !has_tools {
        return None;
    }
    Some(SystemMessage {
        content: SystemContent::Text(system_prompt.unwrap_or("").to_string()),
        sections: None,
        tools_added: if has_tools { tools.map(<[Tool]>::to_vec) } else { None },
        tools_removed: None,
        timestamp: 0,
        // `{ role, content, toolsAdded?, timestamp }`
        timestamp_last: true,
        ..Default::default()
    })
}

/// Fold `Context.system_prompt` and `Context.tools` into a leading system message.
pub fn normalize_context(context: &Context) -> TranscriptContext {
    let initial = create_initial_system_message(context.system_prompt.as_deref(), context.tools.as_deref());
    let mut messages = Vec::with_capacity(context.messages.len() + 1);
    if let Some(initial) = initial {
        messages.push(Message::System(initial));
    }
    messages.extend(context.messages.iter().cloned());
    TranscriptContext::from_messages(messages)
}

/// Owned variant of [`normalize_context`].
pub fn normalize_context_owned(context: Context) -> TranscriptContext {
    let initial = create_initial_system_message(context.system_prompt.as_deref(), context.tools.as_deref());
    let mut messages = context.messages;
    if let Some(initial) = initial {
        messages.insert(0, Message::System(initial));
    }
    TranscriptContext::from_messages(messages)
}

/// Build a transcript from messages that already carry their system messages.
pub fn transcript_from_messages(messages: Vec<Message>) -> TranscriptContext {
    TranscriptContext::from_messages(messages)
}

fn system_messages(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(Message::as_system)
}

/// Return the leading system message, if the transcript starts with one.
pub fn get_initial_system_message(messages: &[Message]) -> Option<&SystemMessage> {
    messages.first().and_then(Message::as_system)
}

/// Drop the leading system message for APIs that carry the prompt outside the message list.
pub fn without_initial_system_message(messages: &[Message]) -> &[Message] {
    if get_initial_system_message(messages).is_some() { &messages[1..] } else { messages }
}

/// Resolve the tools available after applying every transcript delta in order.
pub fn get_current_tools(messages: &[Message]) -> Vec<Tool> {
    current_tools_of(system_messages(messages))
}

fn current_tools_of<'a>(messages: impl Iterator<Item = &'a SystemMessage>) -> Vec<Tool> {
    let mut tools: IndexMap<String, Tool> = IndexMap::new();
    for message in messages {
        for tool in message.tools_removed.iter().flatten() {
            tools.shift_remove(&tool.name);
        }
        for tool in message.tools_added.iter().flatten() {
            // Map.set on an existing key keeps its position.
            tools.insert(tool.name.clone(), tool.clone());
        }
    }
    tools.into_values().collect()
}

/// Replay every system message into one leading system message holding the current prompt and tools.
pub fn get_current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    let mut content_removed = false;
    let mut sections: IndexMap<String, (String, bool)> = IndexMap::new();
    let mut timestamp: Option<i64> = None;
    for message in system_messages(messages) {
        if timestamp.is_none() {
            timestamp = Some(message.timestamp);
        }
        let text = system_content_text(&message.content);
        if !message.content_is_empty() {
            content.push(text);
            content_removed |= message.content_lone_surrogates_removed();
        }
        if let Some(message_sections) = &message.sections {
            for (name, value) in message_sections {
                match value {
                    None => {
                        sections.shift_remove(name);
                    }
                    Some(value) => {
                        let removed = message.section_lone_surrogates_removed(name);
                        sections.insert(name.clone(), (value.clone(), removed));
                    }
                }
            }
        }
    }
    let sections_removed = sections.iter().filter(|(_, (_, removed))| *removed).map(|(name, _)| name.clone()).collect();
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: SystemContent::Text(content.join("\n\n")),
        sections: if sections.is_empty() {
            None
        } else {
            Some(sections.into_iter().map(|(k, (v, _))| (k, Some(v))).collect())
        },
        tools_added: if tools.is_empty() { None } else { Some(tools) },
        tools_removed: None,
        timestamp: timestamp.unwrap_or(0),
        // `{ role, content, sections?, toolsAdded?, timestamp }`
        timestamp_last: true,
        content_lone_surrogates_removed: content_removed,
        sections_lone_surrogates_removed: sections_removed,
    })
}

/// Render the current system prompt text after replaying every system message.
pub fn get_current_system_prompt(messages: &[Message]) -> String {
    get_current_system_message(messages).map(|m| get_system_message_text(&m)).unwrap_or_default()
}

/// Rebuild the transcript for APIs without mid-conversation system messages.
pub fn collapse_system_messages(context: &TranscriptContext) -> TranscriptContext {
    let head = get_current_system_message(&context.messages);
    let mut messages: Vec<Message> = Vec::with_capacity(context.messages.len());
    if let Some(head) = head {
        messages.push(Message::System(head));
    }
    messages.extend(context.messages.iter().filter(|m| !matches!(m, Message::System(_))).cloned());
    TranscriptContext::from_messages(messages)
}

/// Keep later system messages in place when the model accepts them; otherwise collapse them.
pub fn resolve_transcript(context: &TranscriptContext, supports_mid_convo_system_messages: Option<bool>) -> TranscriptContext {
    if supports_mid_convo_system_messages == Some(true) {
        context.clone()
    } else {
        collapse_system_messages(context)
    }
}

/// Strip executable and display-only fields from a tool.
pub fn to_tool_declaration(tool: &Tool) -> Tool {
    Tool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
        constrained_sampling: tool.constrained_sampling.clone(),
        typebox: tool.typebox,
    }
}

/// Whether two tools declare the same interface to the model.
pub fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    serde_json::to_string(&to_tool_declaration(left)).ok() == serde_json::to_string(&to_tool_declaration(right)).ok()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolStateChanges {
    pub tools_added: Vec<Tool>,
    pub tools_removed: Vec<ToolReference>,
}

/// Compare two complete tool states. A changed definition is a removal followed by an addition.
pub fn get_tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let previous_tools: std::collections::HashMap<&str, &Tool> = previous.iter().map(|t| (t.name.as_str(), t)).collect();
    let current_tools: std::collections::HashMap<&str, &Tool> = current.iter().map(|t| (t.name.as_str(), t)).collect();
    ToolStateChanges {
        tools_added: current
            .iter()
            .filter(|tool| match previous_tools.get(tool.name.as_str()) {
                None => true,
                Some(prev) => !declarations_equal(prev, tool),
            })
            .map(to_tool_declaration)
            .collect(),
        tools_removed: previous
            .iter()
            .filter(|tool| match current_tools.get(tool.name.as_str()) {
                None => true,
                Some(cur) => !declarations_equal(tool, cur),
            })
            .map(|tool| ToolReference { name: tool.name.clone() })
            .collect(),
    }
}

/// Every definition referenced by transcript tool state, in first-declaration order.
pub fn get_declared_tools(messages: &[Message]) -> Vec<Tool> {
    let mut definitions: IndexMap<String, Tool> = IndexMap::new();
    for message in system_messages(messages) {
        for tool in message.tools_added.iter().flatten() {
            definitions.insert(tool.name.clone(), tool.clone());
        }
    }
    definitions.into_values().collect()
}

/// Whether a tool name was declared twice with different definitions.
#[deprecated(note = "No built-in transport needs this anymore.")]
pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut declared: std::collections::HashMap<String, Tool> = std::collections::HashMap::new();
    for message in system_messages(messages) {
        for tool in message.tools_added.iter().flatten() {
            if let Some(previous) = declared.get(&tool.name)
                && !declarations_equal(previous, tool) {
                    return true;
                }
            declared.insert(tool.name.clone(), tool.clone());
        }
    }
    false
}

/// Whether tool history contains a removal or same-name redeclaration.
pub fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    let systems: Vec<&SystemMessage> = system_messages(messages).collect();
    find_last_non_additive_tool_change(&systems) >= 0
}

fn find_last_non_additive_tool_change(system_messages: &[&SystemMessage]) -> isize {
    let mut declared = std::collections::HashSet::new();
    let mut last: isize = -1;
    for (index, message) in system_messages.iter().enumerate() {
        if message.tools_removed.as_ref().is_some_and(|r| !r.is_empty()) {
            last = index as isize;
        }
        for tool in message.tools_added.iter().flatten() {
            if declared.contains(&tool.name) {
                last = index as isize;
            }
            declared.insert(tool.name.clone());
        }
    }
    last
}

/// How a transport can express tool changes after the first request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptToolSupport {
    None,
    Additions,
    Changes,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptTools {
    /// Tools sent in the top-level request field.
    pub request_tools: Vec<Tool>,
    /// Number of leading system messages whose tool changes `request_tools` already holds.
    pub folded_system_messages: usize,
}

/// Split tool declarations between the top-level request field and in-place changes.
pub fn resolve_transcript_tools(messages: &[Message], support: TranscriptToolSupport) -> TranscriptTools {
    let systems: Vec<&SystemMessage> = system_messages(messages).collect();
    match support {
        TranscriptToolSupport::None => TranscriptTools {
            request_tools: current_tools_of(systems.iter().copied()),
            folded_system_messages: systems.len(),
        },
        TranscriptToolSupport::Changes => TranscriptTools { request_tools: Vec::new(), folded_system_messages: 0 },
        TranscriptToolSupport::Additions => {
            let initial = if get_initial_system_message(messages).is_some() { 1 } else { 0 };
            let folded = std::cmp::max(initial, (find_last_non_additive_tool_change(&systems) + 1) as usize);
            let folded = folded.min(systems.len());
            TranscriptTools {
                request_tools: current_tools_of(systems[..folded].iter().copied()),
                folded_system_messages: folded,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        serde_json::from_value(serde_json::json!({ "name": name, "description": "", "parameters": {} })).unwrap()
    }

    #[test]
    fn system_message_key_order_follows_the_ts_construction() {
        // `createInitialSystemMessage`: `{ role, content, toolsAdded, timestamp }`.
        let initial = create_initial_system_message(Some("p"), Some(&[tool("a")])).unwrap();
        assert_eq!(
            serde_json::to_string(&Message::System(initial.clone())).unwrap(),
            r#"{"role":"system","content":"p","toolsAdded":[{"name":"a","description":"","parameters":{}}],"timestamp":0}"#
        );
        // `getCurrentSystemMessage`: `{ role, content, sections?, toolsAdded?, timestamp }`.
        let current = get_current_system_message(&[Message::System(initial)]).unwrap();
        assert!(serde_json::to_string(&current).unwrap().ends_with(r#"}}],"timestamp":0}"#));
        // The agent loop's tool-change messages put `timestamp` first; parsing keeps either order.
        for json in [
            r#"{"role":"system","content":"","sections":{"a":null},"timestamp":1,"toolsRemoved":[{"name":"a"}]}"#,
            r#"{"role":"system","content":"","toolsRemoved":[{"name":"a"}],"timestamp":1}"#,
            r#"{"role":"system","content":"x","timestamp":2}"#,
        ] {
            let message: Message = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&message).unwrap(), json);
        }
        // Key order does not affect equality.
        let a: SystemMessage = serde_json::from_str(r#"{"content":"","toolsRemoved":[{"name":"a"}],"timestamp":1}"#).unwrap();
        let b: SystemMessage = serde_json::from_str(r#"{"content":"","timestamp":1,"toolsRemoved":[{"name":"a"}]}"#).unwrap();
        assert!(a.timestamp_last && !b.timestamp_last);
        assert_eq!(a, b);
    }
}
