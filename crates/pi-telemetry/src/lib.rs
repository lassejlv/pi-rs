//! Vendor-neutral telemetry contracts and typed schema utilities for pi.
//!
//! Port of `packages/telemetry`. Applications hand an explicit parent [`TelemetryContext`] to the
//! code they call; that code opens child spans with [`TelemetryContextExt::start_span`], records
//! events, attributes and status on the [`TelemetrySpan`] it receives, and the span settles when
//! the callback's future completes. Adapters (OpenTelemetry, SQLite, the in-memory reference
//! implementation) implement the traits; [`noop_telemetry_context`] is used when an application
//! provides none.

mod attributes;
mod context;
pub mod js;
mod memory;
mod metrics;
mod noop;
pub mod render;
mod schema;
mod store;
pub mod testing;

pub use attributes::{AttributeValue, IntoAttributeValue, SpanAttributes, TelemetryCarrier};
pub use context::{
    ContextRef, PropagationRef, SpanError, SpanFuture, SpanLink, SpanOptions, SpanOutcome, SpanRef,
    SpanStatus, TelemetryContext, TelemetryContextExt, TelemetryPropagation, TelemetrySpan,
    same_ref,
};
pub use memory::{
    InMemoryTelemetryContext, ManualTelemetryClock, RecordedTelemetryEvent, RecordedTelemetryLink,
    RecordedTelemetryMeasurement, RecordedTelemetrySpan, SystemTelemetryClock, TelemetryClock,
};
pub use metrics::{
    CounterRef, HistogramRef, InstrumentOptions, MetricKind, MetricsRef, SchemaTelemetryCounter,
    SchemaTelemetryHistogram, TelemetryCounter, TelemetryHistogram, TelemetryMetrics,
    TelemetryUpDownCounter, TypedTelemetryMetrics, UpDownCounterRef, create_typed_metrics,
};
pub use noop::{noop_telemetry_context, noop_telemetry_metrics, noop_telemetry_propagation};
pub use render::render_trace;
pub use schema::{
    TelemetryAttributeCardinality, TelemetryAttributeDefinition, TelemetryAttributeKind,
    TelemetryEventAttributeDefinition, TelemetryEventDefinition,
    TelemetryMetricAttributeDefinition, TelemetryMetricDefinition, TelemetryParentDefinition,
    TelemetrySchemaDefinition, TelemetrySpanDefinition, TelemetrySpanStatusDefinition,
    TelemetryStartAttributeDefinition, TypedSpanStarter, create_typed_span_starter,
    define_telemetry_schema,
};
pub use store::{
    MetricQuery, StoredEvent, StoredLink, StoredMetric, StoredSpan, StoredTrace, TelemetryStore,
    TraceQuery, TraceQueryResult,
};

mod text;
pub use text::TelemetryText;
