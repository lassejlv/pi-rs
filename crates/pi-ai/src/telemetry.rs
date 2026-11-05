//! Model request telemetry (port of `telemetry.ts`).
//!
//! Every `Models::stream`/`stream_simple`/`stream_deferred` call observes its lazy stream: a
//! `pi.ai.request` (or `pi.ai.poll`) span under the caller's [`StreamOptions::telemetry`] parent,
//! block events without content, and low-cardinality metrics on the parent's metrics adapter.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};
use web_time::Instant;

use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_telemetry::{
    MetricKind, MetricsRef, SchemaTelemetryCounter, SchemaTelemetryHistogram, SpanError, SpanOptions, SpanOutcome,
    SpanRef, SpanStatus, TelemetryAttributeDefinition, TelemetryAttributeKind, TelemetryEventDefinition,
    TelemetryMetricDefinition, TelemetryMetrics, TelemetryParentDefinition, TelemetrySchemaDefinition,
    TelemetrySpanDefinition, TelemetrySpanStatusDefinition, TelemetryStartAttributeDefinition, attributes,
    create_typed_metrics, create_typed_span_starter, define_telemetry_schema, noop_telemetry_context,
    noop_telemetry_metrics,
};

use crate::api::lazy::LazyStreamObserver;
use crate::types::{AssistantMessage, AssistantMessageEvent, Model, StreamOptions};
use crate::utils::js_len;

const STOP_VALUES: [&str; 7] = ["pending", "stop", "length", "toolUse", "error", "aborted", "deferred"];

fn string_attr(description: &str) -> TelemetryAttributeDefinition {
    TelemetryAttributeDefinition::new(TelemetryAttributeKind::string(), description)
}

fn number_attr(description: &str) -> TelemetryAttributeDefinition {
    TelemetryAttributeDefinition::new(TelemetryAttributeKind::number(), description)
}

fn provider_attr() -> TelemetryStartAttributeDefinition {
    string_attr("Provider identifier.").required(true)
}

fn model_attr() -> TelemetryStartAttributeDefinition {
    string_attr("Requested model identifier.").required(true)
}

fn stop_attr() -> TelemetryAttributeDefinition {
    TelemetryAttributeDefinition::new(TelemetryAttributeKind::string_values(STOP_VALUES), "Normalized stop reason.")
}

fn request_span(description: &str) -> TelemetrySpanDefinition {
    let mut start_attributes = IndexMap::new();
    start_attributes.insert("provider".to_string(), provider_attr());
    start_attributes.insert("api".to_string(), string_attr("API identifier.").required(true));
    start_attributes.insert("model".to_string(), model_attr());
    start_attributes.insert("reasoning".to_string(), string_attr("Scalar requested reasoning level.").required(false));

    let mut end_attributes = IndexMap::new();
    end_attributes.insert("stop".to_string(), stop_attr());
    end_attributes.insert("usage.input".to_string(), number_attr("Uncached input tokens."));
    end_attributes.insert("usage.output".to_string(), number_attr("Output tokens."));
    end_attributes.insert("usage.cache_read".to_string(), number_attr("Cache-read tokens."));
    end_attributes.insert("usage.cache_write".to_string(), number_attr("Cache-write tokens."));
    end_attributes.insert("cost.total".to_string(), number_attr("Total request cost in USD."));
    end_attributes.insert(
        "response.id".to_string(),
        string_attr("Provider response identifier.").cardinality(pi_telemetry::TelemetryAttributeCardinality::High),
    );

    let mut block_attributes = IndexMap::new();
    block_attributes.insert(
        "type".to_string(),
        TelemetryAttributeDefinition::new(
            TelemetryAttributeKind::string_values(["text", "thinking", "tool_call"]),
            "Block kind.",
        )
        .required(true),
    );
    block_attributes.insert("index".to_string(), number_attr("Content block index.").required(true));
    block_attributes.insert("chars".to_string(), number_attr("UTF-16 character count.").required(true));
    block_attributes
        .insert("tool".to_string(), string_attr("Tool name, for tool-call blocks only.").required(false));

    let mut events = IndexMap::new();
    events.insert(
        "pi.ai.first_chunk".to_string(),
        TelemetryEventDefinition { description: "First normalized stream event delivered.".into(), attributes: IndexMap::new() },
    );
    events.insert(
        "pi.ai.block".to_string(),
        TelemetryEventDefinition {
            description: "A completed content block; content is never recorded.".into(),
            attributes: block_attributes,
        },
    );

    TelemetrySpanDefinition {
        description: description.to_string(),
        parents: TelemetryParentDefinition::Any,
        start_attributes,
        end_attributes,
        events: Some(events),
        status: TelemetrySpanStatusDefinition::new(
            "Stop reason is error: provider_error with at most 500 error-message characters. Aborted stays ok.",
        ),
    }
}

fn metric(
    kind: MetricKind,
    description: &str,
    unit: &str,
    attributes: Vec<(&str, TelemetryStartAttributeDefinition)>,
) -> TelemetryMetricDefinition {
    TelemetryMetricDefinition {
        kind,
        description: description.to_string(),
        unit: Some(unit.to_string()),
        attributes: attributes.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        boundaries: None,
    }
}

/// Pi-owned request metadata, bounded error status, events, and low-cardinality metrics.
pub static AI_TELEMETRY_SCHEMA: LazyLock<TelemetrySchemaDefinition> = LazyLock::new(|| {
    let mut spans = IndexMap::new();
    spans.insert(
        "pi.ai.request".to_string(),
        request_span("One logical model request, including authentication and provider setup."),
    );
    spans.insert(
        "pi.ai.poll".to_string(),
        request_span("One deferred-response poll, not the deferred request's lifetime."),
    );
    let mut metrics = IndexMap::new();
    metrics.insert(
        "pi.ai.request.duration".to_string(),
        metric(
            MetricKind::Histogram,
            "Logical request or poll duration.",
            "ms",
            vec![("provider", provider_attr()), ("model", model_attr()), ("stop", stop_attr().required(true))],
        ),
    );
    metrics.insert(
        "pi.ai.first_chunk.duration".to_string(),
        metric(
            MetricKind::Histogram,
            "Time to the first normalized stream event.",
            "ms",
            vec![("provider", provider_attr()), ("model", model_attr())],
        ),
    );
    metrics.insert(
        "pi.ai.tokens".to_string(),
        metric(
            MetricKind::Counter,
            "Reported model token usage; pending and deferred polls are excluded.",
            "token",
            vec![
                ("provider", provider_attr()),
                ("model", model_attr()),
                (
                    "kind",
                    TelemetryAttributeDefinition::new(
                        TelemetryAttributeKind::string_values(["input", "output", "cache_read", "cache_write"]),
                        "Token usage category.",
                    )
                    .required(true),
                ),
            ],
        ),
    );
    metrics.insert(
        "pi.ai.cost".to_string(),
        metric(
            MetricKind::Counter,
            "Reported model cost; pending and deferred polls are excluded.",
            "USD",
            vec![("provider", provider_attr()), ("model", model_attr())],
        ),
    );
    define_telemetry_schema(TelemetrySchemaDefinition { version: 1, spans, metrics: Some(metrics) })
});

#[derive(Clone)]
struct Instruments {
    request_duration: SchemaTelemetryHistogram,
    first_chunk_duration: SchemaTelemetryHistogram,
    tokens: SchemaTelemetryCounter,
    cost: SchemaTelemetryCounter,
}

fn create_instruments(adapter: MetricsRef) -> Instruments {
    let metrics = create_typed_metrics(adapter, &[&AI_TELEMETRY_SCHEMA]);
    Instruments {
        request_duration: metrics.histogram("pi.ai.request.duration"),
        first_chunk_duration: metrics.histogram("pi.ai.first_chunk.duration"),
        tokens: metrics.counter("pi.ai.tokens"),
        cost: metrics.counter("pi.ai.cost"),
    }
}

/// Instruments per metrics adapter object (TS: a `WeakMap`).
type InstrumentCache = Vec<(Weak<dyn TelemetryMetrics>, Instruments)>;
static INSTRUMENT_CACHE: LazyLock<Mutex<InstrumentCache>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn instruments_for(adapter: MetricsRef) -> Instruments {
    let mut cache = INSTRUMENT_CACHE.lock();
    cache.retain(|(weak, _)| weak.strong_count() > 0);
    if let Some((_, instruments)) =
        cache.iter().find(|(weak, _)| std::ptr::addr_eq(weak.as_ptr(), Arc::as_ptr(&adapter)))
    {
        return instruments.clone();
    }
    let instruments = create_instruments(adapter.clone());
    cache.push((Arc::downgrade(&adapter), instruments.clone()));
    instruments
}

/// Monotonic milliseconds (`performance.now()`); injectable for tests.
pub type TelemetryNow = Arc<dyn Fn() -> f64 + Send + Sync>;

fn default_now() -> TelemetryNow {
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    Arc::new(|| ORIGIN.elapsed().as_secs_f64() * 1000.0)
}

struct ObserverState {
    span: SpanRef,
    first: bool,
    tool_chars: HashMap<usize, usize>,
    finished: bool,
}

impl Drop for ObserverState {
    fn drop(&mut self) {
        // TS leaves the span open forever when no result arrives; Rust settles it like a dropped
        // span callback (see `pi_telemetry::SpanFuture`).
        if !self.finished {
            self.span.end(SpanOutcome::Failed(Some(SpanError::new("AbortError", "The span callback was cancelled"))));
        }
    }
}

/// The scalar requested reasoning level: `reasoning` (simple options) or a string
/// API-specific `reasoning` option. Non-scalar native reasoning is omitted.
fn scalar_reasoning(options: Option<&StreamOptions>) -> Option<String> {
    let options = options?;
    if let Some(level) = options.reasoning {
        return serde_json::to_value(level).ok().and_then(|v| v.as_str().map(str::to_string));
    }
    options.extra.get("reasoning").and_then(|v| v.as_str()).map(str::to_string)
}

/// Observe one logical model request (or deferred poll).
pub fn observe_model_request(requested_model: &Model, options: Option<&StreamOptions>, poll: bool) -> LazyStreamObserver {
    observe_model_request_with_clock(requested_model, options, poll, default_now())
}

/// [`observe_model_request`] with an explicit monotonic clock.
pub fn observe_model_request_with_clock(
    requested_model: &Model,
    options: Option<&StreamOptions>,
    poll: bool,
    now: TelemetryNow,
) -> LazyStreamObserver {
    let started = now();
    let provider = requested_model.provider.clone();
    let model_id = requested_model.id.clone();
    let telemetry = options.and_then(|o| o.telemetry.clone());
    let adapter = telemetry.as_ref().and_then(|t| t.metrics()).unwrap_or_else(noop_telemetry_metrics);
    let Instruments { request_duration, first_chunk_duration, tokens, cost } = instruments_for(adapter);

    let start_attributes = attributes! {
        "provider" => provider.as_str(),
        "model" => model_id.as_str(),
        "api" => requested_model.api.as_str(),
        "reasoning" => scalar_reasoning(options),
    };
    let parent = telemetry.unwrap_or_else(|| noop_telemetry_context());
    let starter = create_typed_span_starter(parent, &[&AI_TELEMETRY_SCHEMA]);
    let name = if poll { "pi.ai.poll" } else { "pi.ai.request" };
    let span = starter.context().begin_span(SpanOptions::new(name).attributes(start_attributes));
    let state =
        Arc::new(Mutex::new(ObserverState { span: span.clone(), first: true, tool_chars: HashMap::new(), finished: false }));

    let base = move || attributes! { "provider" => provider.as_str(), "model" => model_id.as_str() };
    let base_for_result = base.clone();

    let event_state = state.clone();
    let event_now = now.clone();
    let on_event = Arc::new(move |event: &AssistantMessageEvent| {
        let mut state = event_state.lock();
        if state.first {
            state.first = false;
            state.span.add_event("pi.ai.first_chunk", attributes! {});
            first_chunk_duration.record(event_now() - started, base(), Some(state.span.as_ref()));
        }
        match event {
            AssistantMessageEvent::ToolCallDelta { content_index, delta, .. } => {
                *state.tool_chars.entry(*content_index).or_insert(0) += js_len(delta);
            }
            AssistantMessageEvent::TextEnd { content_index, content, .. }
            | AssistantMessageEvent::ThinkingEnd { content_index, content, .. } => {
                let kind = if matches!(event, AssistantMessageEvent::TextEnd { .. }) { "text" } else { "thinking" };
                state.span.add_event(
                    "pi.ai.block",
                    attributes! { "type" => kind, "index" => *content_index, "chars" => js_len(content) },
                );
            }
            AssistantMessageEvent::ToolCallEnd { content_index, tool_call, .. } => {
                let chars = state.tool_chars.remove(content_index).unwrap_or(0);
                state.span.add_event(
                    "pi.ai.block",
                    attributes! {
                        "type" => "tool_call",
                        "index" => *content_index,
                        "chars" => chars,
                        "tool" => tool_call.name.as_str(),
                    },
                );
            }
            _ => {}
        }
    });

    let result_state = state;
    let on_result = Arc::new(move |message: Option<&AssistantMessage>| {
        let mut state = result_state.lock();
        if state.finished {
            return;
        }
        if let Some(message) = message {
            let stop = message.stop_reason.as_str();
            let usage = &message.usage;
            state.span.set_attributes(attributes! {
                "stop" => stop,
                "usage.input" => usage.input,
                "usage.output" => usage.output,
                "usage.cache_read" => usage.cache_read,
                "usage.cache_write" => usage.cache_write,
                "cost.total" => usage.cost.total,
                "response.id" => message.response_id.as_deref(),
            });
            if stop == "error" {
                // `slice(0, 500)` may split a surrogate pair; keep the unit as JavaScript does.
                let error_message = pi_json::strict::JsonString::from_utf16(
                    message
                        .error_message
                        .as_deref()
                        .map(|m| m.encode_utf16().take(500).collect::<Vec<u16>>())
                        .unwrap_or_default(),
                );
                state.span.set_status(SpanStatus::error(SpanError::new("provider_error", error_message)));
            }
            let mut duration_attributes = base_for_result();
            duration_attributes.insert("stop".into(), stop.into());
            request_duration.record(now() - started, duration_attributes, Some(state.span.as_ref()));
            if !poll || (stop != "pending" && stop != "deferred") {
                for (kind, value) in [
                    ("input", usage.input),
                    ("output", usage.output),
                    ("cache_read", usage.cache_read),
                    ("cache_write", usage.cache_write),
                ] {
                    let mut attrs = base_for_result();
                    attrs.insert("kind".into(), kind.into());
                    tokens.add(value as f64, attrs);
                }
                cost.add(usage.cost.total, base_for_result());
            }
        }
        state.finished = true;
        state.span.end(SpanOutcome::Ok);
    });

    LazyStreamObserver { on_event: Some(on_event), on_result: Some(on_result) }
}
