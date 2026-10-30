use std::any::Any;
use std::sync::{Arc, Weak};
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::TelemetryText;
use crate::attributes::{SpanAttributes, TelemetryCarrier};
use crate::context::{
    ContextRef, PropagationRef, SpanLink, SpanOptions, SpanOutcome, SpanRef, SpanStatus,
    TelemetryContext, TelemetryPropagation, TelemetrySpan,
};
use crate::js::number_to_json;
use crate::metrics::{
    CounterRef, HistogramRef, InstrumentOptions, MetricKind, MetricsRef, TelemetryCounter,
    TelemetryHistogram, TelemetryMetrics, TelemetryUpDownCounter, UpDownCounterRef,
};
use crate::store::{
    MetricQuery, StoredEvent, StoredLink, StoredMetric, StoredSpan, StoredTrace, TelemetryStore,
    TraceQuery, TraceQueryResult,
};

const SCOPE_KEY: &str = "pi-memory-scope";
const SPAN_KEY: &str = "pi-memory-span";
const TRACE_KEY: &str = "pi-memory-trace";

/// Wall clock (`Date.now()`) and monotonic clock (`performance.now()`) in milliseconds.
pub trait TelemetryClock: Send + Sync {
    fn now_ms(&self) -> f64;
    fn performance_now_ms(&self) -> f64;
}

static PROCESS_START: Lazy<Instant> = Lazy::new(Instant::now);

fn system_now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0)
}

fn system_performance_now_ms() -> f64 {
    PROCESS_START.elapsed().as_secs_f64() * 1000.0
}

/// The system clocks.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTelemetryClock;

impl TelemetryClock for SystemTelemetryClock {
    fn now_ms(&self) -> f64 {
        system_now_ms()
    }

    fn performance_now_ms(&self) -> f64 {
        system_performance_now_ms()
    }
}

/// A clock for tests: the wall clock is set explicitly; the monotonic clock follows the system
/// unless set.
#[derive(Debug, Default)]
pub struct ManualTelemetryClock {
    now: Mutex<Option<f64>>,
    performance: Mutex<Option<f64>>,
}

impl ManualTelemetryClock {
    pub fn new(now_ms: f64) -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(Some(now_ms)),
            performance: Mutex::new(None),
        })
    }

    pub fn set_now(&self, now_ms: f64) {
        *self.now.lock() = Some(now_ms);
    }

    pub fn set_performance_now(&self, now_ms: f64) {
        *self.performance.lock() = Some(now_ms);
    }
}

impl TelemetryClock for ManualTelemetryClock {
    fn now_ms(&self) -> f64 {
        self.now.lock().unwrap_or_else(system_now_ms)
    }

    fn performance_now_ms(&self) -> f64 {
        self.performance
            .lock()
            .unwrap_or_else(system_performance_now_ms)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryLink {
    pub context: TelemetryCarrier,
    pub attributes: SpanAttributes,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    /// Local parent ID, or `None` (`null`) for roots and parents owned by another recorder.
    pub parent_id: Option<u64>,
    pub context: TelemetryCarrier,
    pub parent_context: Option<TelemetryCarrier>,
    pub links: Vec<RecordedTelemetryLink>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_sequence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryMeasurement {
    pub kind: MetricKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::js::option_numbers"
    )]
    pub boundaries: Option<Vec<f64>>,
    #[serde(with = "crate::js::number")]
    pub value: f64,
    pub attributes: SpanAttributes,
    pub context: Option<TelemetryCarrier>,
}

struct MutableEvent {
    time_ms: f64,
    name: String,
    attributes: SpanAttributes,
}

struct MutableSpan {
    started_ms: f64,
    clock: f64,
    ended_ms: Option<f64>,
    id: u64,
    parent_id: Option<u64>,
    context: TelemetryCarrier,
    parent_context: Option<TelemetryCarrier>,
    links: Vec<RecordedTelemetryLink>,
    name: String,
    attributes: SpanAttributes,
    events: Vec<MutableEvent>,
    status: SpanStatus,
    explicit_status: bool,
    settled: bool,
    end_sequence: Option<u64>,
}

struct State {
    scope: String,
    spans: Vec<MutableSpan>,
    measurements: Vec<(f64, RecordedTelemetryMeasurement)>,
    next_span_id: u64,
    next_end_sequence: u64,
}

struct Parent {
    context: TelemetryCarrier,
    local_id: Option<u64>,
    /// Index of the parent span in this recorder, when it is one of its spans.
    span: Option<usize>,
}

fn carrier_get<'a>(carrier: &'a TelemetryCarrier, key: &str) -> Option<&'a TelemetryText> {
    carrier.get_str(key)
}

/// `${scope}:${span}`, keeping the carrier's UTF-16 units.
fn carrier_id(carrier: &TelemetryCarrier) -> TelemetryText {
    let part = |key| {
        carrier_get(carrier, key).map_or_else(
            || "undefined".encode_utf16().collect(),
            |text| text.units().to_vec(),
        )
    };
    let mut units = part(SCOPE_KEY);
    units.push(u16::from(b':'));
    units.extend(part(SPAN_KEY));
    pi_json::strict::JsonString::from_utf16(units).into()
}

/// `[startedMs, traceId]` as `JSON.stringify` wrote it, keeping the trace ID's UTF-16 units.
fn parse_cursor(cursor: &str) -> anyhow::Result<(f64, TelemetryText)> {
    let value = pi_json::strict::parse(cursor).map_err(anyhow::Error::msg)?;
    match value.as_array().map(Vec::as_slice) {
        Some([started, pi_json::strict::Value::String(trace_id)]) if started.is_number() => {
            Ok((started.as_f64().unwrap_or(f64::NAN), trace_id.into()))
        }
        _ => anyhow::bail!("invalid trace cursor"),
    }
}

/// Identity of a context created by any in-memory recorder; survives settlement.
fn carrier_of(context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
    let any: &dyn Any = context;
    if let Some(span) = any.downcast_ref::<InMemorySpan>() {
        return Some(span.context.clone());
    }
    if let Some(parent) = any.downcast_ref::<InMemoryParent>() {
        return Some(parent.context.clone());
    }
    None
}

fn copy_link(link: &SpanLink) -> Option<RecordedTelemetryLink> {
    carrier_of(link.context.as_ref()).map(|context| RecordedTelemetryLink {
        context,
        attributes: link.attributes.clone().unwrap_or_default(),
    })
}

/// Backend-neutral reference implementation that records spans in process memory.
/// Create a fresh instance to isolate tests or independent recording scopes.
pub struct InMemoryTelemetryContext {
    this: Weak<InMemoryTelemetryContext>,
    clock: Arc<dyn TelemetryClock>,
    state: Mutex<State>,
}

impl InMemoryTelemetryContext {
    pub fn new() -> Arc<Self> {
        Self::with_clock(Arc::new(SystemTelemetryClock))
    }

    pub fn with_clock(clock: Arc<dyn TelemetryClock>) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            clock,
            state: Mutex::new(State {
                scope: uuid::Uuid::new_v4().to_string(),
                spans: Vec::new(),
                measurements: Vec::new(),
                next_span_id: 1,
                next_end_sequence: 1,
            }),
        })
    }

    fn arc(&self) -> Arc<InMemoryTelemetryContext> {
        self.this
            .upgrade()
            .expect("InMemoryTelemetryContext is alive while borrowed")
    }

    fn start(&self, parent: Option<Parent>, options: SpanOptions) -> SpanRef {
        let root = self.arc();
        let mut state = self.state.lock();
        if let Some(index) = parent.as_ref().and_then(|parent| parent.span)
            && state.spans[index].settled
        {
            return Arc::new(InertSpan { root: root.clone() });
        }
        let attributes = copy_attributes(options.attributes.as_ref());
        let links = options.links.iter().filter_map(copy_link).collect();
        let id = state.next_span_id;
        state.next_span_id += 1;
        let trace = parent
            .as_ref()
            .and_then(|parent| carrier_get(&parent.context, TRACE_KEY).cloned())
            .unwrap_or_else(|| format!("{}:{id}", state.scope).into());
        let mut context = TelemetryCarrier::new();
        context.insert(SCOPE_KEY.into(), state.scope.clone().into());
        context.insert(SPAN_KEY.into(), id.to_string().into());
        context.insert(TRACE_KEY.into(), trace);
        state.spans.push(MutableSpan {
            started_ms: self.clock.now_ms(),
            clock: self.clock.performance_now_ms(),
            ended_ms: None,
            id,
            parent_id: parent.as_ref().and_then(|parent| parent.local_id),
            context: context.clone(),
            parent_context: parent.map(|parent| parent.context),
            links,
            name: options.name,
            attributes,
            events: Vec::new(),
            status: SpanStatus::Ok,
            explicit_status: false,
            settled: false,
            end_sequence: None,
        });
        let index = state.spans.len() - 1;
        drop(state);
        Arc::new(InMemorySpan {
            root,
            index,
            context,
        })
    }

    fn with_open_span(
        &self,
        index: usize,
        update: impl FnOnce(&mut MutableSpan, &dyn TelemetryClock),
    ) {
        let mut state = self.state.lock();
        let span = &mut state.spans[index];
        if span.settled {
            return;
        }
        update(span, self.clock.as_ref());
    }

    fn end_span(&self, index: usize, outcome: SpanOutcome) {
        let mut state = self.state.lock();
        let sequence = state.next_end_sequence;
        let span = &mut state.spans[index];
        if span.settled {
            return;
        }
        if let SpanOutcome::Failed(error) = outcome
            && !span.explicit_status
        {
            span.status = SpanStatus::Error(error);
        }
        span.settled = true;
        span.ended_ms = Some(span.started_ms + self.clock.performance_now_ms() - span.clock);
        span.end_sequence = Some(sequence);
        state.next_end_sequence += 1;
    }

    fn measurement_recorder(
        &self,
        kind: MetricKind,
        name: &str,
        options: InstrumentOptions,
    ) -> Arc<Instrument> {
        Arc::new(Instrument {
            root: self.arc(),
            kind,
            name: name.to_string(),
            description: options.description,
            unit: options.unit,
            boundaries: if kind == MetricKind::Histogram {
                options.boundaries
            } else {
                None
            },
        })
    }

    fn stored_spans(&self) -> Vec<StoredSpan> {
        let state = self.state.lock();
        let mut spans: Vec<StoredSpan> = state
            .spans
            .iter()
            .filter(|span| span.settled)
            .map(|span| StoredSpan {
                id: carrier_id(&span.context),
                trace_id: carrier_get(&span.context, TRACE_KEY)
                    .cloned()
                    .unwrap_or_else(|| "".into()),
                parent_id: span.parent_context.as_ref().map(carrier_id),
                name: span.name.clone(),
                started_ms: span.started_ms,
                ended_ms: span.ended_ms.unwrap_or(span.started_ms),
                attributes: span.attributes.clone(),
                events: span
                    .events
                    .iter()
                    .map(|event| StoredEvent {
                        name: event.name.clone(),
                        time_ms: event.time_ms,
                        attributes: event.attributes.clone(),
                    })
                    .collect(),
                links: span
                    .links
                    .iter()
                    .map(|link| StoredLink {
                        trace_id: carrier_get(&link.context, TRACE_KEY)
                            .cloned()
                            .unwrap_or_else(|| "".into()),
                        span_id: carrier_id(&link.context),
                        attributes: Some(link.attributes.clone()),
                    })
                    .collect(),
                status: span.status.clone(),
            })
            .collect();
        spans.sort_by(|a, b| {
            a.started_ms
                .partial_cmp(&b.started_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        spans
    }

    /// Returns detached measurements in recording order, without aggregation.
    pub fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement> {
        self.state
            .lock()
            .measurements
            .iter()
            .map(|(_, measurement)| measurement.clone())
            .collect()
    }

    /// Returns detached snapshots in span-start order.
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        self.state
            .lock()
            .spans
            .iter()
            .map(|span| RecordedTelemetrySpan {
                id: span.id,
                parent_id: span.parent_id,
                context: span.context.clone(),
                parent_context: span.parent_context.clone(),
                links: span.links.clone(),
                name: span.name.clone(),
                attributes: span.attributes.clone(),
                events: span
                    .events
                    .iter()
                    .map(|event| RecordedTelemetryEvent {
                        name: event.name.clone(),
                        attributes: event.attributes.clone(),
                    })
                    .collect(),
                status: span.status.clone(),
                settled: span.settled,
                end_sequence: span.end_sequence,
            })
            .collect()
    }

    /// Returns a detached carrier for a known span or extracted parent, including settled spans.
    pub fn inject(&self, context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
        carrier_of(context)
    }

    /// Missing carrier identities fall back to this recorder's root context.
    pub fn extract(&self, carrier: Option<&TelemetryCarrier>) -> ContextRef {
        let identity = carrier.and_then(|carrier| {
            Some((
                carrier_get(carrier, SCOPE_KEY)?.clone(),
                carrier_get(carrier, SPAN_KEY)?.clone(),
                carrier_get(carrier, TRACE_KEY)?.clone(),
            ))
        });
        let Some((scope, span, trace)) = identity else {
            return self.arc();
        };
        let local_id = if scope.as_str() == Some(self.state.lock().scope.as_str()) {
            span.as_str().and_then(|span| span.parse::<u64>().ok())
        } else {
            None
        };
        let mut context = TelemetryCarrier::new();
        context.insert(SCOPE_KEY.into(), scope);
        context.insert(SPAN_KEY.into(), span);
        context.insert(TRACE_KEY.into(), trace);
        Arc::new(InMemoryParent {
            root: self.arc(),
            context,
            local_id,
        })
    }

    pub fn counter(&self, name: &str, options: InstrumentOptions) -> CounterRef {
        self.measurement_recorder(MetricKind::Counter, name, options)
    }

    pub fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> UpDownCounterRef {
        self.measurement_recorder(MetricKind::UpDownCounter, name, options)
    }

    pub fn histogram(&self, name: &str, options: InstrumentOptions) -> HistogramRef {
        self.measurement_recorder(MetricKind::Histogram, name, options)
    }

    pub fn get_trace(&self, trace_id: &TelemetryText) -> Option<StoredTrace> {
        let spans: Vec<StoredSpan> = self
            .stored_spans()
            .into_iter()
            .filter(|span| &span.trace_id == trace_id)
            .collect();
        if spans.is_empty() {
            None
        } else {
            Some(StoredTrace {
                trace_id: trace_id.clone(),
                spans,
            })
        }
    }

    pub fn find_traces(&self, query: &TraceQuery) -> anyhow::Result<TraceQueryResult> {
        let limit = query.limit.unwrap_or(20);
        let after = match &query.cursor {
            Some(cursor) => Some(parse_cursor(cursor)?),
            None => None,
        };
        let mut grouped: IndexMap<TelemetryText, Vec<StoredSpan>> = IndexMap::new();
        for span in self.stored_spans() {
            grouped.entry(span.trace_id.clone()).or_default().push(span);
        }
        let mut matches: Vec<StoredTrace> = grouped
            .into_iter()
            .map(|(trace_id, spans)| StoredTrace { trace_id, spans })
            .filter(|trace| {
                let first = &trace.spans[0];
                query.name.as_ref().is_none_or(|name| &first.name == name)
                    && query.start_min.is_none_or(|min| first.started_ms >= min)
                    && query.start_max.is_none_or(|max| first.started_ms <= max)
                    && after.as_ref().is_none_or(|(started, trace_id)| {
                        first.started_ms < *started
                            || (first.started_ms == *started && &trace.trace_id > trace_id)
                    })
                    && (!query.errors || trace.spans.iter().any(|span| span.status.is_error()))
                    && query.attributes.as_ref().is_none_or(|attributes| {
                        trace.spans.iter().any(|span| {
                            attributes.iter().all(|(key, value)| {
                                span.attributes
                                    .get(key)
                                    .is_some_and(|actual| actual.strict_equals(value))
                            })
                        })
                    })
            })
            .collect();
        matches.sort_by(|a, b| {
            b.spans[0]
                .started_ms
                .partial_cmp(&a.spans[0].started_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.trace_id.cmp(&b.trace_id))
        });
        let more = matches.len() > limit;
        matches.truncate(limit);
        let cursor = if more {
            matches.last().map(|last| {
                format!(
                    "[{},{}]",
                    number_to_json(last.spans[0].started_ms),
                    pi_json::strict::stringify(&last.trace_id.clone().into())
                )
            })
        } else {
            None
        };
        Ok(TraceQueryResult {
            traces: matches,
            cursor,
        })
    }

    /// Read raw measurements as count-one rows, without changing [`get_metrics`](Self::get_metrics) snapshots.
    pub fn find_metrics(&self, query: &MetricQuery) -> Vec<StoredMetric> {
        self.state
            .lock()
            .measurements
            .iter()
            .filter(|(interval, measurement)| {
                measurement.name == query.name
                    && query.start_min.is_none_or(|min| *interval >= min)
                    && query.start_max.is_none_or(|max| *interval <= max)
                    && query.attributes.as_ref().is_none_or(|attributes| {
                        attributes.iter().all(|(key, value)| {
                            measurement
                                .attributes
                                .get(key)
                                .is_some_and(|actual| actual.strict_equals(value))
                        })
                    })
            })
            .map(|(interval, measurement)| StoredMetric {
                interval_ms: *interval,
                name: measurement.name.clone(),
                kind: measurement.kind,
                unit: measurement.unit.clone(),
                attributes: measurement.attributes.clone(),
                count: 1.0,
                sum: measurement.value,
                min: measurement.value,
                max: measurement.value,
            })
            .collect()
    }

    fn metrics_ref(&self) -> MetricsRef {
        self.arc()
    }

    fn propagation_ref(&self) -> PropagationRef {
        self.arc()
    }
}

fn copy_attributes(attributes: Option<&SpanAttributes>) -> SpanAttributes {
    attributes.cloned().unwrap_or_default()
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn metrics(&self) -> Option<MetricsRef> {
        Some(self.metrics_ref())
    }

    fn propagation(&self) -> Option<PropagationRef> {
        Some(self.propagation_ref())
    }

    fn begin_span(&self, options: SpanOptions) -> SpanRef {
        self.start(None, options)
    }
}

impl TelemetryPropagation for InMemoryTelemetryContext {
    fn inject(&self, context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
        InMemoryTelemetryContext::inject(self, context)
    }

    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> ContextRef {
        InMemoryTelemetryContext::extract(self, carrier)
    }
}

impl TelemetryMetrics for InMemoryTelemetryContext {
    fn counter(&self, name: &str, options: InstrumentOptions) -> CounterRef {
        InMemoryTelemetryContext::counter(self, name, options)
    }

    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> UpDownCounterRef {
        InMemoryTelemetryContext::up_down_counter(self, name, options)
    }

    fn histogram(&self, name: &str, options: InstrumentOptions) -> HistogramRef {
        InMemoryTelemetryContext::histogram(self, name, options)
    }
}

#[async_trait]
impl TelemetryStore for InMemoryTelemetryContext {
    async fn find_traces(&self, query: TraceQuery) -> anyhow::Result<TraceQueryResult> {
        InMemoryTelemetryContext::find_traces(self, &query)
    }

    async fn get_trace(&self, trace_id: &TelemetryText) -> anyhow::Result<Option<StoredTrace>> {
        Ok(InMemoryTelemetryContext::get_trace(self, trace_id))
    }

    async fn find_metrics(&self, query: MetricQuery) -> anyhow::Result<Vec<StoredMetric>> {
        Ok(InMemoryTelemetryContext::find_metrics(self, &query))
    }
}

struct Instrument {
    root: Arc<InMemoryTelemetryContext>,
    kind: MetricKind,
    name: String,
    description: Option<String>,
    unit: Option<String>,
    boundaries: Option<Vec<f64>>,
}

impl Instrument {
    fn record_measurement(
        &self,
        value: f64,
        attributes: SpanAttributes,
        context: Option<&dyn TelemetryContext>,
    ) {
        let carrier = context.and_then(carrier_of);
        let interval = (self.root.clock.now_ms() / 10_000.0).floor() * 10_000.0;
        self.root.state.lock().measurements.push((
            interval,
            RecordedTelemetryMeasurement {
                kind: self.kind,
                name: self.name.clone(),
                description: self.description.clone(),
                unit: self.unit.clone(),
                boundaries: self.boundaries.clone(),
                value,
                attributes,
                context: carrier,
            },
        ));
    }
}

impl TelemetryCounter for Instrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.record_measurement(value, attributes, None);
    }
}

impl TelemetryUpDownCounter for Instrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.record_measurement(value, attributes, None);
    }
}

impl TelemetryHistogram for Instrument {
    fn record(
        &self,
        value: f64,
        attributes: SpanAttributes,
        context: Option<&dyn TelemetryContext>,
    ) {
        self.record_measurement(value, attributes, context);
    }
}

/// A recording span of an in-memory recorder.
struct InMemorySpan {
    root: Arc<InMemoryTelemetryContext>,
    index: usize,
    context: TelemetryCarrier,
}

impl TelemetryContext for InMemorySpan {
    fn update_attributes(&self, attributes: SpanAttributes) {
        self.set_attributes(attributes);
    }
    fn record_event(&self, name: &str, attributes: SpanAttributes) {
        TelemetrySpan::add_event(self, name, attributes);
    }
    fn record_link(&self, link: SpanLink) {
        TelemetrySpan::add_link(self, link);
    }

    fn metrics(&self) -> Option<MetricsRef> {
        Some(self.root.metrics_ref())
    }

    fn propagation(&self) -> Option<PropagationRef> {
        Some(self.root.propagation_ref())
    }

    fn begin_span(&self, options: SpanOptions) -> SpanRef {
        let local_id = self.root.state.lock().spans[self.index].id;
        self.root.start(
            Some(Parent {
                context: self.context.clone(),
                local_id: Some(local_id),
                span: Some(self.index),
            }),
            options,
        )
    }
}

impl TelemetrySpan for InMemorySpan {
    fn add_link(&self, link: SpanLink) {
        let copy = copy_link(&link);
        self.root.with_open_span(self.index, |span, _| {
            if let Some(copy) = copy {
                span.links.push(copy);
            }
        });
    }

    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        self.root.with_open_span(self.index, |span, clock| {
            span.events.push(MutableEvent {
                name: name.to_string(),
                time_ms: clock.performance_now_ms() - span.clock,
                attributes,
            });
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        self.root.with_open_span(self.index, |span, _| {
            for (name, value) in attributes {
                span.attributes.insert(name, value);
            }
        });
    }

    fn set_status(&self, status: SpanStatus) {
        self.root.with_open_span(self.index, |span, _| {
            span.status = status;
            span.explicit_status = true;
        });
    }

    fn end(&self, outcome: SpanOutcome) {
        self.root.end_span(self.index, outcome);
    }
}

/// A parent extracted from a carrier.
struct InMemoryParent {
    root: Arc<InMemoryTelemetryContext>,
    context: TelemetryCarrier,
    local_id: Option<u64>,
}

impl TelemetryContext for InMemoryParent {
    fn metrics(&self) -> Option<MetricsRef> {
        Some(self.root.metrics_ref())
    }

    fn propagation(&self) -> Option<PropagationRef> {
        Some(self.root.propagation_ref())
    }

    fn begin_span(&self, options: SpanOptions) -> SpanRef {
        self.root.start(
            Some(Parent {
                context: self.context.clone(),
                local_id: self.local_id,
                span: None,
            }),
            options,
        )
    }
}

/// Child of a settled span: records nothing but keeps the adapter's capabilities.
struct InertSpan {
    root: Arc<InMemoryTelemetryContext>,
}

impl TelemetryContext for InertSpan {
    fn metrics(&self) -> Option<MetricsRef> {
        Some(self.root.metrics_ref())
    }

    fn propagation(&self) -> Option<PropagationRef> {
        Some(self.root.propagation_ref())
    }

    fn begin_span(&self, _options: SpanOptions) -> SpanRef {
        Arc::new(InertSpan {
            root: self.root.clone(),
        })
    }
}

impl TelemetrySpan for InertSpan {
    fn add_link(&self, _link: SpanLink) {}
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}
    fn end(&self, _outcome: SpanOutcome) {}
}
