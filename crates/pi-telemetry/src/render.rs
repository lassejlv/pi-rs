//! Plain-text trace rendering (`@earendil-works/pi-telemetry/render`).

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::TelemetryText;
use crate::attributes::SpanAttributes;
use crate::context::SpanStatus;
use crate::js::number_to_string;
use crate::store::StoredSpan;

fn format_attributes(attributes: Option<&SpanAttributes>) -> String {
    let Some(attributes) = attributes else {
        return String::new();
    };
    attributes
        .iter()
        .map(|(key, value)| format!(" {key}={}", value.to_json_string()))
        .collect()
}

/// Render stored spans as a chronological forest, treating absent parents as roots. Spans with
/// the same ID are deduplicated (the last one wins, at the position of the first).
pub fn render_trace(spans: &[StoredSpan]) -> String {
    let mut by_id: IndexMap<&TelemetryText, &StoredSpan> = IndexMap::new();
    for span in spans {
        by_id.insert(&span.id, span);
    }
    let mut ordered: Vec<&StoredSpan> = by_id.values().copied().collect();
    ordered.sort_by(|left, right| {
        left.started_ms
            .partial_cmp(&right.started_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut roots: Vec<&StoredSpan> = Vec::new();
    let mut children: HashMap<&TelemetryText, Vec<&StoredSpan>> = HashMap::new();
    for span in ordered {
        match span.parent_id.as_ref() {
            Some(parent) if by_id.contains_key(parent) => {
                children.entry(parent).or_default().push(span)
            }
            _ => roots.push(span),
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut pending: Vec<(&StoredSpan, usize)> =
        roots.into_iter().rev().map(|span| (span, 0)).collect();
    while let Some((span, depth)) = pending.pop() {
        let indent = "  ".repeat(depth);
        let mut error = String::new();
        if let SpanStatus::Error(details) = &span.status {
            error.push_str(" error");
            if let Some(details) = details {
                error.push_str(&format!(" {}: {}", details.name, details.message));
            }
        }
        lines.push(format!(
            "{indent}{}{} ({}ms){error}",
            span.name,
            format_attributes(Some(&span.attributes)),
            number_to_string(span.ended_ms - span.started_ms)
        ));
        for event in &span.events {
            lines.push(format!(
                "{indent}  · {}{} (+{}ms)",
                event.name,
                format_attributes(Some(&event.attributes)),
                number_to_string(event.time_ms)
            ));
        }
        for link in &span.links {
            lines.push(format!(
                "{indent}  → link trace {} span {}{}",
                link.trace_id,
                link.span_id,
                format_attributes(link.attributes.as_ref())
            ));
        }
        if let Some(descendants) = children.get(&span.id) {
            for descendant in descendants.iter().rev() {
                pending.push((descendant, depth + 1));
            }
        }
    }
    lines.join("\n")
}
