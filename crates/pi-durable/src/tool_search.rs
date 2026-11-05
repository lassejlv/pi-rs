//! Deferred tool discovery. Loads persist on the conversation through addTools.
use crate::{
    harness::{
        declaration::ToolDeclaration,
        registry::{Extension, ToolCaller},
        tool::{
            NativeTool, ReplayPolicy, ToolControl, ToolExecutionResult, ToolValue, text_content,
        },
    },
    search::{SearchDocument, rank_documents},
    *,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
pub const DEFAULT_TOOL_SEARCH_LIMIT: f64 = 8.0;
const DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolSearchToolInput {
    pub query: JsonString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolSearchToolDetails {
    pub loaded: Vec<JsonString>,
}

fn whitespace(unit: u16) -> bool {
    matches!(unit,0x09..=0x0d|0x20|0xa0|0x1680|0x2000..=0x200a|0x2028|0x2029|0x202f|0x205f|0x3000|0xfeff)
}
fn trimmed(text: &JsonString) -> &[u16] {
    let units = text.units();
    let begin = units
        .iter()
        .position(|&u| !whitespace(u))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|&u| !whitespace(u))
        .map_or(begin, |i| i + 1);
    &units[begin..end]
}
fn schema_text(schema: &JsonValue, parts: &mut Vec<JsonString>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(description) = object
        .get(&JsonString::from("description"))
        .and_then(JsonValue::as_string)
    {
        parts.push(description.clone());
    }
    if let Some(properties) = object
        .get(&JsonString::from("properties"))
        .and_then(JsonValue::as_object)
    {
        // JSON Schema property names follow ECMAScript's numeric-key ordering.
        let mut keys: Vec<_> = properties.keys().collect();
        let index = |name: &JsonString| -> Option<u32> {
            let name = name.to_string_checked().ok()?;
            let index = name.parse::<u32>().ok()?;
            (index != u32::MAX && index.to_string() == name).then_some(index)
        };
        keys.sort_by(|left, right| match (index(left), index(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        for name in keys {
            parts.push(name.clone());
            schema_text(&properties[name], parts);
        }
    }
    if let Some(items) = object.get(&JsonString::from("items")) {
        schema_text(items, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object
            .get(&JsonString::from(key))
            .and_then(JsonValue::as_array)
        {
            for variant in variants {
                schema_text(variant, parts);
            }
        }
    }
}
pub fn search_text(tool: &ToolDeclaration) -> JsonString {
    let spaced = JsonString::from_utf16(
        tool.name
            .units()
            .iter()
            .map(|&u| if u == 95 { 32 } else { u })
            .collect(),
    );
    let mut parts = vec![tool.name.clone(), spaced, tool.description.clone()];
    schema_text(&tool.parameters, &mut parts);
    crate::harness::tool::join(
        parts.into_iter().filter(|part| !trimmed(part).is_empty()),
        " ",
    )
}
fn validate_input(args: &ToolSearchToolInput) -> Result<f64> {
    if trimmed(&args.query).is_empty() {
        return Err(Error::message("query must not be empty"));
    }
    let max = args.limit.unwrap_or(DEFAULT_TOOL_SEARCH_LIMIT);
    if !max.is_finite() || max.fract() != 0.0 || max <= 0.0 {
        return Err(Error::message("limit must be a positive integer"));
    }
    Ok(max)
}
pub fn execute_search(
    args: ToolSearchToolInput,
    candidates: &[ToolDeclaration],
) -> Result<ToolExecutionResult> {
    let max = validate_input(&args)?;
    search_result(args.query, max, candidates)
}
fn search_result(
    query: JsonString,
    max: f64,
    candidates: &[ToolDeclaration],
) -> Result<ToolExecutionResult> {
    let documents: Vec<_> = candidates
        .iter()
        .map(|tool| SearchDocument {
            name: tool.name.clone(),
            text: search_text(tool),
        })
        .collect();
    let loaded = rank_documents(&query, &documents, max);
    let text = if loaded.is_empty() {
        "No matching tools found.".into()
    } else {
        let mut text = JsonString::from(format!(
            "Loaded {} tool{}. They are available from your next call:\n",
            loaded.len(),
            if loaded.len() == 1 { "" } else { "s" }
        ));
        let lines = loaded.iter().map(|name| {
            let description = candidates
                .iter()
                .find(|tool| tool.name == *name)
                .map(|tool| trimmed(&tool.description))
                .unwrap_or_default();
            let end = description
                .iter()
                .position(|&u| u == 10)
                .unwrap_or(description.len());
            let end = if end > 0 && end < description.len() && description[end - 1] == 13 {
                end - 1
            } else {
                end
            };
            crate::harness::tool::join(
                [
                    "- ".into(),
                    name.clone(),
                    ": ".into(),
                    JsonString::from_utf16(description[..end].to_vec()),
                ],
                "",
            )
        });
        text.append(&crate::harness::tool::join(lines, "\n"));
        text
    };
    let control = (!loaded.is_empty()).then(|| ToolControl {
        add_tools: Some(loaded.clone()),
        ..Default::default()
    });
    Ok(ToolExecutionResult {
        output: Some(Arc::new(vec![text_content(text)])),
        details: Some(ToolValue::Json(wire::encode(&ToolSearchToolDetails {
            loaded,
        }))),
        control,
        ..Default::default()
    })
}
pub fn create_tool_search_tool() -> NativeTool {
    let parameters = wire::encode(
        &serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Search query for deferred tools."},"limit":{"type":"number","description":"Maximum number of tools to return. Defaults to 8."}},"required":["query"]}),
    );
    let mut tool = NativeTool::new(
        ToolDeclaration::new(TOOL_SEARCH_TOOL_NAME, DESCRIPTION, parameters),
        Arc::new(|args, api, context| {
            Box::pin(async move {
                let args: ToolSearchToolInput =
                    wire::decode(args.value()).map_err(Error::message)?;
                // Validation precedes agent resolution, including host callbacks.
                let max = validate_input(&args)?;
                let candidates = api.agent(&context).await?.deferred_tools()?;
                search_result(args.query, max, &candidates)
            })
        }),
    );
    tool.callers = Some(vec![ToolCaller::Model]);
    tool.replay = ReplayPolicy::Safe;
    tool
}
pub fn create_tool_search_extension<S>() -> Extension<NativeTool, S> {
    let mut extension = Extension::new("tool-search");
    extension.tools.push(Arc::new(create_tool_search_tool()));
    extension
}
