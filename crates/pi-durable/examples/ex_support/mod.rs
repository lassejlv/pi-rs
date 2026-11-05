//! Shared helpers for the ported upstream examples. Public API only.
#![allow(dead_code)]
use pi_ai::{models::Models, providers::faux::*};
use pi_durable::harness::tool::text_content;
use pi_durable::*;
use std::sync::Arc;

pub type Ext = Extension<NativeTool, NativeSection>;
pub type Reg = Registry<NativeTool, NativeSection>;

pub fn json(source: &str) -> JsonValue {
    pi_chord::json::parse(source).expect("valid JSON")
}
pub fn obj(source: &str) -> JsonObject {
    JsonObject::try_from_value(json(source)).expect("JSON object")
}
pub fn text(value: &JsonValue) -> String {
    value
        .to_utf8()
        .map(|text| text.to_string())
        .unwrap_or_else(|| value.to_string())
}

/// A tool taking `{ path }` that answers `"<name> <path>"`.
pub fn path_tool(name: &str, description: &str) -> Arc<NativeTool> {
    let label = name.to_string();
    Arc::new(
        NativeTool::new(
        ToolDeclaration::new(
            name,
            description,
            json(r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#),
        ),
        Arc::new(move |args, _, _| {
            let path = args
                .get(&JsonString::from("path"))
                .map(text)
                .unwrap_or_default();
            let answer = format!("{label} {path}");
            Box::pin(async move {
                Ok(ToolExecutionResult {
                    output: Some(Arc::new(vec![text_content(answer.into())])),
                    ..Default::default()
                })
            })
        }),
        )
        // Application metadata, as upstream's `ToolRegistration & { snippet }`.
        .with_metadata(obj(&format!(r#"{{"snippet":"Use {name} for files."}}"#))),
    )
}

/// Names of the tools a resolved agent exposes.
pub fn tool_names(agent: &AgentView) -> Result<Vec<String>> {
    Ok(agent
        .typed::<NativeTool, NativeSection>()?
        .tools
        .iter()
        .map(|tool| tool.declaration.name.to_string())
        .collect())
}
pub fn tool_descriptions(agent: &AgentView) -> Result<Vec<String>> {
    Ok(agent
        .typed::<NativeTool, NativeSection>()?
        .tools
        .iter()
        .map(|tool| {
            format!(
                "{}: {}",
                tool.declaration.name, tool.declaration.description
            )
        })
        .collect())
}
pub fn extension_names(agent: &AgentView) -> Result<Vec<String>> {
    Ok(agent
        .typed::<NativeTool, NativeSection>()?
        .extensions
        .iter()
        .map(|extension| extension.name.to_string())
        .collect())
}

/// A scripted offline model collection with an instantaneous faux provider.
pub fn faux_models() -> (Models, FauxProviderHandle) {
    let models = Models::default();
    let faux = faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    models.set_provider(faux.provider.clone());
    (models, faux)
}
pub fn faux_agent() -> ConversationOptions {
    ConversationOptions {
        agent: Some(obj(r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#)),
        ..Default::default()
    }
}

/// One-line rendering of a request message, via its wire JSON.
pub fn show(message: &impl serde::Serialize) -> String {
    let value = serde_json::to_value(message).expect("message serializes");
    let content = &value["content"];
    match value["role"].as_str().unwrap_or("?") {
        "user" => format!("user: {}", content.as_str().unwrap_or("")),
        "system" => format!("system: {}", value["sections"]),
        "assistant" => format!(
            "assistant: {}",
            content
                .as_array()
                .into_iter()
                .flatten()
                .map(|part| match part["type"].as_str() {
                    Some("text") => part["text"].as_str().unwrap_or("").to_string(),
                    Some("toolCall") => format!("call({})", part["id"].as_str().unwrap_or("")),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "toolResult" => format!(
            "result({}){}",
            value["toolCallId"].as_str().unwrap_or(""),
            if value["isError"].as_bool() == Some(true) {
                " error"
            } else {
                ""
            }
        ),
        other => other.to_string(),
    }
}

/// The assistant text of the entry that answered a settled input submission.
pub async fn answer_text(
    conversation: &Conversation,
    settled: &SettledSubmissionRecord,
    context: &Context,
) -> Result<Option<String>> {
    let SubmissionState::InputDone { answer, .. } = settled.state.clone() else {
        return Ok(None);
    };
    let entry = conversation
        .commit(move |tx| async move { tx.entry(answer).await }, context)
        .await?;
    let Some(message) = entry
        .and_then(|entry| entry.model)
        .and_then(|m| m.into_iter().next())
    else {
        return Ok(None);
    };
    let value = serde_json::to_value(&message).expect("message serializes");
    Ok(value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|part| part["type"] == "text")
        .and_then(|part| part["text"].as_str().map(str::to_string)))
}
