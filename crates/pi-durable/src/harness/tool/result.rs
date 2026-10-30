use super::*;
use crate::{
    harness::{
        live::{NestedToolSummary, SlotProgress},
        usage::{UsageBucket, record_usage},
    },
    wire, *,
};
use pi_chord::output::{OutputLimits, Retain, bound_output};
use std::sync::Arc;

pub(crate) fn join(parts: impl IntoIterator<Item = JsonString>, separator: &str) -> JsonString {
    let mut text = JsonString::default();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            text.append(&separator.into());
        }
        text.append(&part);
    }
    text
}
pub fn text_content(text: JsonString) -> Arc<JsonObject> {
    let mut value = wire::object();
    wire::put(&mut value, "type", &"text");
    wire::put(&mut value, "text", &text);
    Arc::new(JsonObject::try_from_value(value).expect("text content object"))
}
pub fn harness_error(
    code: impl Into<JsonString>,
    message: impl Into<JsonString>,
) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Arc::new(Vec::new())),
        is_error: Some(true),
        diagnostics: Some(vec![tool_diagnostic(code.into(), message.into())]),
        ..Default::default()
    }
}
pub(crate) fn tool_diagnostic(code: JsonString, message: JsonString) -> ToolDiagnostic {
    ToolDiagnostic {
        severity: DiagnosticSeverity::Error,
        message,
        code: Some(code),
    }
}
pub(crate) fn truncated(bytes: usize, lines: usize, retain: Option<Retain>) -> ToolDiagnostic {
    let kept = match retain {
        None => "",
        Some(Retain::Head) => " to its beginning",
        Some(Retain::Tail) => " to its end",
    };
    ToolDiagnostic {
        severity: DiagnosticSeverity::Warn,
        code: Some("truncated".into()),
        message: format!("Output truncated{kept}: {lines} lines, {bytes} bytes dropped").into(),
    }
}
pub fn from_slot(
    slot: Option<&SlotProgress>,
    code: impl Into<JsonString>,
    message: impl Into<JsonString>,
) -> ToolExecutionResult {
    let mut diagnostics = slot
        .and_then(|slot| slot.diagnostics.clone())
        .unwrap_or_default();
    let dropped = slot.and_then(|slot| slot.dropped_bytes).unwrap_or(0);
    if dropped > 0 {
        diagnostics.push(truncated(
            dropped as usize,
            slot.and_then(|slot| slot.dropped_lines).unwrap_or(0) as usize,
            None,
        ));
    }
    diagnostics.push(tool_diagnostic(code.into(), message.into()));
    let output = slot
        .and_then(|slot| slot.output.clone())
        .filter(|text| !text.units().is_empty())
        .map(|text| vec![text_content(text)])
        .unwrap_or_default();
    ToolExecutionResult {
        output: Some(Arc::new(output)),
        is_error: Some(true),
        details: slot.and_then(|slot| slot.details.clone()).map(Into::into),
        diagnostics: Some(diagnostics),
        ..Default::default()
    }
}
pub fn nested_result(
    task_id: TaskId,
    result: &ToolExecutionResult,
    duration_ms: Option<f64>,
) -> Result<NestedToolExecutionResult> {
    Ok(NestedToolExecutionResult {
        task_id,
        structured_output: result
            .structured_output
            .as_ref()
            .map(ToolValue::copy_json)
            .transpose()?,
        is_error: result.is_error.unwrap_or(false),
        details: result
            .details
            .as_ref()
            .map(ToolValue::copy_json)
            .transpose()?,
        diagnostics: result.diagnostics.clone().unwrap_or_default(),
        usage: result.usage.clone(),
        duration_ms,
    })
}
pub fn fallback_result(
    task_id: TaskId,
    name: &JsonString,
    outcome: &TaskOutcome<ToolTaskResult>,
) -> NestedToolExecutionResult {
    let text = match outcome {
        TaskOutcome::Faulted { error } => join(
            [
                "Tool ".into(),
                name.clone(),
                " failed: ".into(),
                error.message.clone(),
            ],
            "",
        ),
        TaskOutcome::Orphaned { reason } => join(
            [
                "Tool ".into(),
                name.clone(),
                " could not resume: ".into(),
                reason.clone(),
            ],
            "",
        ),
        _ => join(
            [
                "Tool ".into(),
                name.clone(),
                " ended without a result".into(),
            ],
            "",
        ),
    };
    let status: JsonString = wire::field(&wire::encode(outcome), "status").expect("outcome status");
    nested_result(task_id, &harness_error(status, text), None)
        .expect("native error result is strict JSON")
}
pub fn output_value(output: &ToolContent) -> Result<JsonValue> {
    match output.as_slice() {
        [] => Ok(wire::encode(&"")),
        [only] if only.value()["type"].string_equals("text") => Ok(wire::encode(
            &wire::field::<JsonString>(only.value(), "text").map_err(Error::message)?,
        )),
        [only] => Ok(only.value().clone()),
        _ => Ok(wire::encode(output)),
    }
}
pub fn summary_of(result: &NestedToolExecutionResult, output: &ToolContent) -> NestedToolSummary {
    let messages = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    let text = if result.is_error {
        join(
            if messages.is_empty() {
                output
                    .iter()
                    .filter(|item| item.value()["type"].string_equals("text"))
                    .filter_map(|item| item.value()["text"].as_string().cloned())
                    .collect()
            } else {
                messages
            },
            "\n",
        )
    } else {
        JsonString::default()
    };
    NestedToolSummary {
        is_error: result.is_error,
        duration_ms: result.duration_ms,
        usage: result.usage.clone(),
        error: (!text.units().is_empty())
            .then(|| JsonString::from_utf16(text.units()[..text.units().len().min(500)].to_vec())),
    }
}
pub fn render_diagnostics(diagnostics: &[ToolDiagnostic]) -> JsonString {
    let lines = diagnostics.iter().map(|diagnostic| {
        let severity = match diagnostic.severity {
            DiagnosticSeverity::Info => "info",
            DiagnosticSeverity::Warn => "warn",
            DiagnosticSeverity::Error => "error",
        };
        join(
            [format!("[{severity}] ").into(), diagnostic.message.clone()],
            "",
        )
    });
    join(
        [
            "<harness>\n".into(),
            join(lines, "\n"),
            "\n</harness>".into(),
        ],
        "",
    )
}
pub async fn append_tool_result(
    tx: &Transaction,
    conversation: ConversationId,
    call: &ToolCall,
    result: &ToolExecutionResult,
    timestamp: f64,
    duration_ms: Option<f64>,
) -> Result<EntryRecord> {
    let diagnostics = result.diagnostics.clone().unwrap_or_default();
    let mut content = result
        .output
        .as_ref()
        .map(|output| output.as_ref().clone())
        .unwrap_or_default();
    if !diagnostics.is_empty() {
        content.push(text_content(render_diagnostics(&diagnostics)));
    }
    let mut message = wire::object();
    wire::put(&mut message, "role", &"toolResult");
    wire::put(&mut message, "toolCallId", &call.id);
    wire::put(&mut message, "toolName", &call.name);
    wire::put(&mut message, "content", &content);
    let details = result
        .details
        .as_ref()
        .map(ToolValue::copy_json)
        .transpose()?;
    wire::put_optional(&mut message, "details", &details);
    wire::put_optional(&mut message, "usage", &result.usage);
    wire::put(&mut message, "isError", &result.is_error.unwrap_or(false));
    wire::put_optional(&mut message, "durationMs", &duration_ms);
    wire::put(&mut message, "timestamp", &timestamp);
    if let Some(usage) = &result.usage {
        record_usage(
            tx,
            conversation,
            UsageBucket::Tools,
            call.name.clone(),
            usage,
        )
        .await?;
    }
    let mut data = wire::object();
    wire::put(&mut data, "diagnostics", &diagnostics);
    let mut entry = EntryDraft::new(TOOL_RESULT_ENTRY.kind().clone());
    entry.model = Some(vec![message]);
    entry.data = Some(data);
    tx.append_entry(conversation, entry).await
}
pub struct BoundedContent {
    pub content: ToolContent,
    pub dropped_bytes: usize,
    pub dropped_lines: usize,
}
pub fn bound_content(content: &ToolContent, limits: OutputLimits) -> BoundedContent {
    let texts = content
        .iter()
        .filter(|item| item.value()["type"].string_equals("text"))
        .collect::<Vec<_>>();
    let bounded = bound_output(
        &join(
            texts
                .iter()
                .filter_map(|item| item.value()["text"].as_string().cloned()),
            "",
        ),
        limits,
    );
    if bounded.dropped_bytes == 0 {
        return BoundedContent {
            content: content.clone(),
            dropped_bytes: 0,
            dropped_lines: 0,
        };
    }
    let keep = match limits.retain {
        Retain::Head => texts.first(),
        Retain::Tail => texts.last(),
    };
    let mut result = Vec::new();
    for item in content.iter() {
        if !item.value()["type"].string_equals("text") {
            result.push(item.clone());
        } else if keep.is_some_and(|keep| Arc::ptr_eq(item, keep)) {
            let mut item = item.as_ref().clone();
            item.insert("text".into(), wire::encode(&bounded.text));
            result.push(Arc::new(item));
        }
    }
    BoundedContent {
        content: Arc::new(result),
        dropped_bytes: bounded.dropped_bytes,
        dropped_lines: bounded.dropped_lines,
    }
}
pub fn check_key(key: &JsonString) -> Result<()> {
    let units = key.units();
    let integer = units.first().is_some_and(|unit| (49..=57).contains(unit))
        && units.iter().all(|unit| (48..=57).contains(unit));
    if units.is_empty() || units.contains(&47) || key.equals_str("__proto__") || integer {
        return Err(Error::message(format!(
            "Nested call key {} must be non-empty, without \"/\", not \"__proto__\", and not a positive integer",
            wire::encode(key)
        )));
    }
    Ok(())
}
