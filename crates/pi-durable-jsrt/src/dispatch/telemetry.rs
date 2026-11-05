//! `@earendil-works/pi-telemetry` (`js/pi-telemetry/*.js`): `InMemoryTelemetryContext` and
//! `renderTrace` over the native pi-telemetry crate (`telemetry.*`). The recorder, spans,
//! carriers, links, measurements and the store queries are native; JS keeps the callback scoping
//! of `startSpan` and object identities. The recorder's clock reads the JS clocks the glue passes
//! with each call (`Date.now()`, `performance.now()`), so fake timers apply as upstream.

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_json::strict::JsonString;
use pi_telemetry::{
    AttributeValue, ContextRef, CounterRef, HistogramRef, InMemoryTelemetryContext,
    InstrumentOptions, MetricQuery, SpanAttributes, SpanError, SpanLink, SpanOptions, SpanOutcome,
    SpanRef, SpanStatus, StoredSpan, TelemetryCarrier, TelemetryClock, TelemetryText, TraceQuery,
    UpDownCounterRef, render_trace,
};

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Dispatch, Methods};
use crate::value::HostValue;

/// The telemetry method group.
pub struct TelemetryDispatch;

/// The JS clocks of the current call.
#[derive(Default)]
struct JsClock {
    now: Mutex<(f64, f64)>,
}

impl TelemetryClock for JsClock {
    fn now_ms(&self) -> f64 {
        self.now.lock().0
    }
    fn performance_now_ms(&self) -> f64 {
        self.now.lock().1
    }
}

/// One JS `InMemoryTelemetryContext`.
struct Recorder {
    memory: Arc<InMemoryTelemetryContext>,
    clock: Arc<JsClock>,
}

/// A native context a JS object stands for (an extracted parent).
struct Ctx(ContextRef);
/// A native span a JS span object stands for.
struct Span(SpanRef);

enum Instrument {
    Counter(CounterRef),
    UpDown(UpDownCounterRef),
    Histogram(HistogramRef),
}

struct Measuring {
    instrument: Instrument,
    clock: Arc<JsClock>,
}

fn tick(clock: &JsClock, args: &Args, i: usize) {
    let n = |k: usize| args.get(k).as_f64().unwrap_or(0.0);
    *clock.now.lock() = (n(i), n(i + 1));
}

fn text(value: &HostValue) -> Option<TelemetryText> {
    value.as_string().map(|s| TelemetryText::from(s.clone()))
}

fn utf8(value: &HostValue) -> String {
    value
        .as_string()
        .map(JsonString::to_string_lossy)
        .unwrap_or_default()
}

/// JS attributes (`undefined` members are dropped, as every upstream recorder does).
fn attributes(value: &HostValue) -> SpanAttributes {
    let mut out = IndexMap::new();
    let HostValue::Object(entries) = value else {
        return out;
    };
    for (key, value) in entries {
        let converted = match value {
            HostValue::String(s) => AttributeValue::String(s.clone().into()),
            HostValue::Number(n) => AttributeValue::Number(*n),
            HostValue::Bool(b) => AttributeValue::Bool(*b),
            HostValue::Array(items)
                if items.iter().all(|v| matches!(v, HostValue::Number(_))) && !items.is_empty() =>
            {
                AttributeValue::NumberArray(items.iter().filter_map(HostValue::as_f64).collect())
            }
            HostValue::Array(items)
                if items.iter().all(|v| matches!(v, HostValue::Bool(_))) && !items.is_empty() =>
            {
                AttributeValue::BoolArray(items.iter().filter_map(HostValue::as_bool).collect())
            }
            HostValue::Array(items) => {
                AttributeValue::StringArray(items.iter().filter_map(text).collect())
            }
            _ => continue,
        };
        out.insert(key.to_string_lossy(), converted);
    }
    out
}

fn scalar_attributes(value: Option<&HostValue>) -> Option<IndexMap<String, AttributeValue>> {
    value
        .filter(|v| matches!(v, HostValue::Object(_)))
        .map(attributes)
}

/// A link whose context is a native context or span (others are not recorded, as upstream).
fn link(value: &HostValue) -> Option<SpanLink> {
    let context = value.get("context")?;
    let context: ContextRef = if let Some(span) = context.as_native::<Span>() {
        span.0.clone()
    } else {
        context.as_native::<Ctx>()?.0.clone()
    };
    Some(SpanLink::with_attributes(
        context,
        value.get("attributes").map(attributes).unwrap_or_default(),
    ))
}

fn span_options(value: &HostValue) -> SpanOptions {
    let mut options = SpanOptions::new(utf8(value.get("name").unwrap_or(&HostValue::Undefined)))
        .attributes(value.get("attributes").map(attributes).unwrap_or_default());
    if let Some(HostValue::Array(links)) = value.get("links") {
        for l in links.iter().filter_map(link) {
            options = options.link(l);
        }
    }
    options
}

fn status(value: &HostValue) -> SpanStatus {
    let error = value
        .get("status")
        .and_then(HostValue::as_string)
        .is_some_and(|s| s.equals_str("error"));
    if !error {
        return SpanStatus::Ok;
    }
    SpanStatus::Error(
        value
            .get("error")
            .filter(|e| matches!(e, HostValue::Object(_)))
            .map(|e| {
                SpanError::new(
                    utf8(e.get("name").unwrap_or(&HostValue::Undefined)),
                    e.get("message")
                        .and_then(HostValue::as_string)
                        .cloned()
                        .unwrap_or_default(),
                )
            }),
    )
}

fn carrier_value(carrier: &TelemetryCarrier) -> HostValue {
    HostValue::Object(
        carrier
            .iter()
            .map(|(k, v)| {
                (
                    k.clone().into_json_string(),
                    HostValue::String(v.clone().into_json_string()),
                )
            })
            .collect(),
    )
}

fn strict<T: serde::Serialize>(value: &T) -> HostResult<HostValue> {
    pi_chord::json::to_value(value)
        .map(|v| HostValue::from_strict(&v))
        .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))
}

fn recorder(args: &Args) -> HostResult<Arc<Recorder>> {
    args.native::<Recorder>(0)
}

/// The native context a JS context stands for: a span, an extracted parent, or the recorder.
fn parent(recorder: &Recorder, value: &HostValue) -> ContextRef {
    if let Some(span) = value.as_native::<Span>() {
        span.0.clone()
    } else if let Some(ctx) = value.as_native::<Ctx>() {
        ctx.0.clone()
    } else {
        recorder.memory.clone()
    }
}

fn instrument_options(value: &HostValue) -> InstrumentOptions {
    let mut options = InstrumentOptions::new();
    if let Some(description) = value.get("description").and_then(HostValue::as_string) {
        options = options.description(description.to_string_lossy());
    }
    if let Some(unit) = value.get("unit").and_then(HostValue::as_string) {
        options = options.unit(unit.to_string_lossy());
    }
    if let Some(HostValue::Array(bounds)) = value.get("boundaries") {
        options = options.boundaries(
            bounds
                .iter()
                .filter_map(HostValue::as_f64)
                .collect::<Vec<_>>(),
        );
    }
    options
}

impl Dispatch for TelemetryDispatch {
    fn register(&self, m: &mut Methods) {
        // (now, perf) -> recorder
        m.sync("telemetry.memory", |_, args: Args| {
            let clock = Arc::new(JsClock::default());
            tick(&clock, &args, 0);
            Ok(HostValue::native(Recorder {
                memory: InMemoryTelemetryContext::with_clock(clock.clone()),
                clock,
            }))
        });
        // (recorder, now, perf, parent, options) -> span
        m.sync("telemetry.begin", |_, args: Args| {
            let recorder = recorder(&args)?;
            tick(&recorder.clock, &args, 1);
            let span = parent(&recorder, args.get(3)).begin_span(span_options(args.get(4)));
            Ok(HostValue::native(Span(span)))
        });
        // (recorder, now, perf, span, failed, error?)
        m.sync("telemetry.end", |_, args: Args| {
            let recorder = recorder(&args)?;
            tick(&recorder.clock, &args, 1);
            let span = args.native::<Span>(3)?;
            let outcome = if args.get(4).as_bool() == Some(true) {
                SpanOutcome::Failed(match args.get(5) {
                    HostValue::Object(_) => Some(SpanError::new(
                        utf8(args.get(5).get("name").unwrap_or(&HostValue::Undefined)),
                        args.get(5)
                            .get("message")
                            .and_then(HostValue::as_string)
                            .cloned()
                            .unwrap_or_default(),
                    )),
                    _ => None,
                })
            } else {
                SpanOutcome::Ok
            };
            span.0.end(outcome);
            Ok(HostValue::Undefined)
        });
        // (recorder, now, perf, span, op, ...)
        m.sync("telemetry.update", |_, args: Args| {
            let recorder = recorder(&args)?;
            tick(&recorder.clock, &args, 1);
            let span = args.native::<Span>(3)?;
            match utf8(args.get(4)).as_str() {
                "event" => span
                    .0
                    .add_event(&utf8(args.get(5)), attributes(args.get(6))),
                "attributes" => span.0.set_attributes(attributes(args.get(5))),
                "status" => span.0.set_status(status(args.get(5))),
                "link" => {
                    if let Some(l) = link(args.get(5)) {
                        span.0.add_link(l);
                    }
                }
                other => {
                    return Err(HostError::type_error(format!(
                        "unknown span update {other}"
                    )));
                }
            }
            Ok(HostValue::Undefined)
        });
        // (recorder, context) -> carrier | undefined
        m.sync("telemetry.inject", |_, args: Args| {
            let recorder = recorder(&args)?;
            let context = args.get(1);
            let carrier = if let Some(span) = context.as_native::<Span>() {
                recorder.memory.inject(span.0.as_ref())
            } else if let Some(ctx) = context.as_native::<Ctx>() {
                recorder.memory.inject(ctx.0.as_ref())
            } else {
                None
            };
            Ok(carrier.as_ref().map(carrier_value).unwrap_or_default())
        });
        // (recorder, carrier) -> context (the glue checks the carrier's shape first)
        m.sync("telemetry.extract", |_, args: Args| {
            let recorder = recorder(&args)?;
            let carrier: TelemetryCarrier = match args.get(1) {
                HostValue::Object(entries) => entries
                    .iter()
                    .filter_map(|(k, v)| {
                        v.as_string().map(|v| {
                            (
                                TelemetryText::from(k.clone()),
                                TelemetryText::from(v.clone()),
                            )
                        })
                    })
                    .collect(),
                _ => TelemetryCarrier::new(),
            };
            Ok(HostValue::native(Ctx(recorder
                .memory
                .extract(Some(&carrier)))))
        });
        // (recorder, kind, name, options) -> instrument
        m.sync("telemetry.instrument", |_, args: Args| {
            let recorder = recorder(&args)?;
            let name = utf8(args.get(2));
            let options = instrument_options(args.get(3));
            let instrument = match utf8(args.get(1)).as_str() {
                "counter" => Instrument::Counter(recorder.memory.counter(&name, options)),
                "upDownCounter" => {
                    Instrument::UpDown(recorder.memory.up_down_counter(&name, options))
                }
                _ => Instrument::Histogram(recorder.memory.histogram(&name, options)),
            };
            Ok(HostValue::native(Measuring {
                instrument,
                clock: recorder.clock.clone(),
            }))
        });
        // (instrument, now, perf, value, attributes, context?)
        m.sync("telemetry.record", |_, args: Args| {
            let measuring = args.native::<Measuring>(0)?;
            tick(&measuring.clock, &args, 1);
            let value = args.get(3).as_f64().unwrap_or(f64::NAN);
            let attrs = attributes(args.get(4));
            match &measuring.instrument {
                Instrument::Counter(c) => c.add(value, attrs),
                Instrument::UpDown(c) => c.add(value, attrs),
                Instrument::Histogram(h) => {
                    let context = args.get(5);
                    let exemplar: Option<ContextRef> =
                        if let Some(span) = context.as_native::<Span>() {
                            Some(span.0.clone())
                        } else {
                            context.as_native::<Ctx>().map(|c| c.0.clone())
                        };
                    h.record(value, attrs, exemplar.as_deref());
                }
            }
            Ok(HostValue::Undefined)
        });
        m.sync("telemetry.spans", |_, args: Args| {
            strict(&recorder(&args)?.memory.get_spans())
        });
        m.sync("telemetry.metrics", |_, args: Args| {
            strict(&recorder(&args)?.memory.get_metrics())
        });
        m.sync("telemetry.getTrace", |_, args: Args| {
            let trace = recorder(&args)?
                .memory
                .get_trace(&text(args.get(1)).unwrap_or_else(|| TelemetryText::from("")));
            trace
                .as_ref()
                .map(strict)
                .transpose()
                .map(Option::unwrap_or_default)
        });
        m.sync("telemetry.findTraces", |_, args: Args| {
            let query = args.get(1);
            let n = |key: &str| query.get(key).and_then(HostValue::as_f64);
            let query = TraceQuery {
                name: query
                    .get("name")
                    .and_then(HostValue::as_string)
                    .map(JsonString::to_string_lossy),
                attributes: scalar_attributes(query.get("attributes")),
                errors: query.get("errors").and_then(HostValue::as_bool) == Some(true),
                start_min: n("startMin"),
                start_max: n("startMax"),
                limit: n("limit").map(|l| l as usize),
                cursor: query
                    .get("cursor")
                    .and_then(HostValue::as_string)
                    .map(JsonString::to_string_lossy),
            };
            let result = recorder(&args)?
                .memory
                .find_traces(&query)
                .map_err(|e| HostError::new(ErrorClass::Error, e.to_string()))?;
            let mut fields = vec![("traces", strict(&result.traces)?)];
            if let Some(cursor) = result.cursor {
                fields.push(("cursor", HostValue::string(&cursor)));
            }
            Ok(HostValue::object(fields))
        });
        m.sync("telemetry.findMetrics", |_, args: Args| {
            let query = args.get(1);
            let n = |key: &str| query.get(key).and_then(HostValue::as_f64);
            let query = MetricQuery {
                name: utf8(query.get("name").unwrap_or(&HostValue::Undefined)),
                start_min: n("startMin"),
                start_max: n("startMax"),
                attributes: scalar_attributes(query.get("attributes")),
            };
            strict(&recorder(&args)?.memory.find_metrics(&query))
        });
        // (spans) -> text
        m.sync("telemetry.renderTrace", |_, args: Args| {
            let spans = args.get(0).to_strict_omitting_undefined()?;
            let spans: Vec<StoredSpan> =
                crate::dispatch::env::decode(&spans).map_err(HostError::type_error)?;
            Ok(HostValue::string(&render_trace(&spans)))
        });
    }
}
