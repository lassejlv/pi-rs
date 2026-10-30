//! Compaction range selection, request sizing and model-driven summarization.
use super::{
    context::{ContextView, order_tool_results},
    settings::Settings,
};
use crate::{wire, *};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
mod task;
pub use task::*;

use super::types::ModelRef;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPolicy {
    pub enabled: bool,
    pub reserve_tokens: f64,
    pub keep_recent_tokens: f64,
    pub background_tokens: f64,
}
pub fn compaction_policy(settings: &Settings, model: &ModelRef) -> Result<CompactionPolicy> {
    let mut key = model.provider.clone();
    key.append(&"/".into());
    key.append(&model.model_id);
    let base = settings.compaction.value();
    let overrides = wire::get(base, "modelOverrides").and_then(JsonValue::as_object);
    let selected = overrides.and_then(|values| values.get(&key));
    let mut policy = wire::object();
    for field in [
        "enabled",
        "reserveTokens",
        "keepRecentTokens",
        "backgroundTokens",
    ] {
        if let Some(value) = selected
            .and_then(|value| wire::get(value, field))
            .filter(|v| !v.is_null())
            .or_else(|| wire::get(base, field))
        {
            wire::put(&mut policy, field, value);
        }
    }
    wire::decode(&policy).map_err(Error::message)
}
fn role(message: &JsonValue, name: &str) -> bool {
    message["role"].string_equals(name)
}
fn string(value: &JsonValue) -> JsonString {
    value.as_string().cloned().unwrap_or_default()
}
fn joined(parts: impl IntoIterator<Item = JsonString>, separator: &str) -> JsonString {
    let mut out = JsonString::default();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            out.append(&separator.into());
        }
        out.append(&part);
    }
    out
}
fn content_text(content: &JsonValue) -> JsonString {
    if let Some(text) = content.as_string() {
        return text.clone();
    }
    joined(
        content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"].string_equals("text"))
            .filter_map(|block| block["text"].as_string().cloned()),
        "\n",
    )
}
fn tokens(chars: usize) -> f64 {
    (chars as f64 / 3.5).ceil()
}
/// Same estimate as pi-ai, operating on lossless messages before provider conversion.
pub fn estimate_message_tokens(message: &JsonValue) -> f64 {
    let content = &message["content"];
    if role(message, "system") {
        let mut parts = vec![content_text(content)];
        if let Some(sections) = message["sections"].as_object() {
            parts.extend(
                pi_chord::json::object_entries(sections)
                    .filter_map(|(_, value)| value.as_string().cloned()),
            );
        }
        let mut count = tokens(
            joined(
                parts.into_iter().filter(|part| !part.units().is_empty()),
                "\n\n",
            )
            .units()
            .len(),
        );
        for key in ["toolsAdded", "toolsRemoved"] {
            if let Some(tools) = message[key].as_array().filter(|tools| !tools.is_empty()) {
                count += tokens(wire::encode(tools).to_string().encode_utf16().count());
            }
        }
        return count;
    }
    if role(message, "user") || role(message, "toolResult") {
        return tokens(if let Some(text) = content.as_string() {
            text.units().len()
        } else {
            content
                .as_array()
                .into_iter()
                .flatten()
                .map(|block| {
                    if block["type"].string_equals("text") {
                        string(&block["text"]).units().len()
                    } else {
                        4800
                    }
                })
                .sum()
        });
    }
    tokens(
        content
            .as_array()
            .into_iter()
            .flatten()
            .map(|block| {
                if block["type"].string_equals("text") {
                    string(&block["text"]).units().len()
                } else if block["type"].string_equals("thinking") {
                    string(&block["thinking"]).units().len()
                } else {
                    string(&block["name"]).units().len()
                        + block["arguments"].to_string().encode_utf16().count()
                }
            })
            .sum(),
    )
}
fn usage_tokens(message: &JsonValue) -> f64 {
    let usage = &message["usage"];
    let total = usage["totalTokens"].as_f64().unwrap_or(0.0);
    if total != 0.0 {
        return total;
    }
    ["input", "output", "cacheRead", "cacheWrite"]
        .iter()
        .map(|key| usage[*key].as_f64().unwrap_or(0.0))
        .sum()
}
pub fn select_cut(view: &ContextView, keep_recent_tokens: f64) -> Option<usize> {
    let start = usize::from(view.head.is_some());
    let candidates = (start..view.contributions.len())
        .filter(|index| is_candidate(&view.contributions, *index))
        .collect::<Vec<_>>();
    let mut kept = 0.0;
    for index in (start..view.contributions.len()).rev() {
        kept += view.contributions[index]
            .iter()
            .map(|message| estimate_message_tokens(message))
            .sum::<f64>();
        if kept < keep_recent_tokens {
            continue;
        }
        let cut = candidates
            .iter()
            .find(|candidate| **candidate >= index)
            .or_else(|| candidates.last())
            .copied()?;
        return (start..cut)
            .any(|index| !view.contributions[index].is_empty())
            .then_some(cut);
    }
    None
}
fn is_candidate(contributions: &[Vec<Arc<JsonValue>>], index: usize) -> bool {
    let Some(first) = contributions[index].first() else {
        return false;
    };
    if role(first, "assistant") {
        return true;
    }
    if !role(first, "user") {
        return false;
    }
    let calls = contributions[..index]
        .iter()
        .rev()
        .find_map(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| role(message, "assistant"))
        })
        .map(|message| {
            message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"].string_equals("toolCall"))
                .map(|block| string(&block["id"]))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    if calls.is_empty() {
        return true;
    }
    for (after, messages) in contributions.iter().enumerate().skip(index) {
        for (position, message) in messages.iter().enumerate() {
            if role(message, "assistant") && (after > index || position > 0) {
                return true;
            }
            if role(message, "toolResult") && calls.contains(&string(&message["toolCallId"])) {
                return false;
            }
        }
    }
    true
}
pub fn summarized_messages(view: &ContextView, cut: usize) -> Vec<Arc<JsonValue>> {
    order_tool_results(
        &view.contributions[..cut]
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>(),
    )
}
pub fn estimate_context(view: &ContextView, extra: &[Arc<JsonValue>]) -> f64 {
    let measured = view
        .entries
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| view.head.as_ref().is_none_or(|head| entry.id > head.id))
        .find_map(|(index, _)| {
            view.contributions[index]
                .iter()
                .rev()
                .find(|message| role(message, "assistant") && usage_tokens(message) > 0.0)
        });
    let from = measured
        .and_then(|measured| {
            view.messages
                .iter()
                .rposition(|message| Arc::ptr_eq(message, measured))
                .map(|position| position + 1)
        })
        .unwrap_or(0);
    measured.map_or(0.0, |message| usage_tokens(message))
        + view.messages[from..]
            .iter()
            .chain(extra)
            .map(|message| estimate_message_tokens(message))
            .sum::<f64>()
}
pub fn serialize_conversation(messages: &[Arc<JsonValue>]) -> JsonString {
    let mut parts = Vec::new();
    for message in messages {
        if role(message, "user") || role(message, "toolResult") {
            let text = content_text(&message["content"]);
            if text.units().is_empty() {
                continue;
            }
            let text = if role(message, "toolResult") && text.units().len() > 2000 {
                let mut truncated = JsonString::from_utf16(text.units()[..2000].to_vec());
                truncated.append(
                    &format!(
                        "\n\n[... {} more characters truncated]",
                        text.units().len() - 2000
                    )
                    .into(),
                );
                truncated
            } else {
                text
            };
            parts.push(joined(
                [
                    if role(message, "user") {
                        "[User]: ".into()
                    } else {
                        "[Tool result]: ".into()
                    },
                    text,
                ],
                "",
            ));
        } else if role(message, "assistant") {
            let blocks = message["content"].as_array().into_iter().flatten();
            let text = blocks
                .clone()
                .filter(|block| block["type"].string_equals("text"))
                .map(|block| string(&block["text"]))
                .collect::<Vec<_>>();
            if !text.is_empty() {
                parts.push(joined(["[Assistant]: ".into(), joined(text, "\n")], ""));
            }
            let calls = blocks
                .filter(|block| block["type"].string_equals("toolCall"))
                .map(|block| {
                    let args = block["arguments"]
                        .as_object()
                        .map(|args| {
                            pi_chord::json::object_entries(args)
                                .map(|(key, value)| {
                                    joined([key.clone(), "=".into(), value.to_string().into()], "")
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    joined(
                        [
                            string(&block["name"]),
                            "(".into(),
                            joined(args, ", "),
                            ")".into(),
                        ],
                        "",
                    )
                })
                .collect::<Vec<_>>();
            if !calls.is_empty() {
                parts.push(joined(
                    ["[Assistant tool calls]: ".into(), joined(calls, "; ")],
                    "",
                ));
            }
        }
    }
    joined(parts, "\n\n")
}
