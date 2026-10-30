use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::TelemetryText;
use crate::attributes::{AttributeValue, SpanAttributes};
use crate::context::SpanStatus;
use crate::metrics::MetricKind;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub name: String,
    #[serde(with = "crate::js::number")]
    pub time_ms: f64,
    pub attributes: SpanAttributes,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredLink {
    pub trace_id: TelemetryText,
    pub span_id: TelemetryText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<SpanAttributes>,
}

/// A settled span snapshot, independent of the recording adapter. Event times are relative to span start.
/// IDs are JavaScript strings: an extracted carrier can supply any, including unpaired surrogates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSpan {
    pub id: TelemetryText,
    pub trace_id: TelemetryText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<TelemetryText>,
    pub name: String,
    #[serde(with = "crate::js::number")]
    pub started_ms: f64,
    #[serde(with = "crate::js::number")]
    pub ended_ms: f64,
    pub attributes: SpanAttributes,
    pub events: Vec<StoredEvent>,
    pub links: Vec<StoredLink>,
    pub status: SpanStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredTrace {
    pub trace_id: TelemetryText,
    pub spans: Vec<StoredSpan>,
}

/// Filters for [`TelemetryStore::find_traces`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceQuery {
    /// Name of the earliest stored span, with span ID breaking timestamp ties.
    pub name: Option<String>,
    /// All equalities must match one span; that span may be anywhere in the trace. Only scalar
    /// values can match.
    pub attributes: Option<IndexMap<String, AttributeValue>>,
    /// Only traces with an error span.
    pub errors: bool,
    /// Inclusive bounds on the earliest stored span's start time.
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    /// Positive page size; defaults to 20.
    pub limit: Option<usize>,
    /// Opaque continuation from the previous page; retain the same filters.
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceQueryResult {
    pub traces: Vec<StoredTrace>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricQuery {
    pub name: String,
    /// Inclusive bounds on interval start time.
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    pub attributes: Option<IndexMap<String, AttributeValue>>,
}

impl MetricQuery {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMetric {
    #[serde(with = "crate::js::number")]
    pub interval_ms: f64,
    pub name: String,
    pub kind: MetricKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub attributes: SpanAttributes,
    #[serde(with = "crate::js::number")]
    pub count: f64,
    #[serde(with = "crate::js::number")]
    pub sum: f64,
    #[serde(with = "crate::js::number")]
    pub min: f64,
    #[serde(with = "crate::js::number")]
    pub max: f64,
}

/// Read settled traces without depending on a storage backend.
#[async_trait]
pub trait TelemetryStore: Send + Sync {
    async fn find_traces(&self, query: TraceQuery) -> anyhow::Result<TraceQueryResult>;
    async fn get_trace(&self, trace_id: &TelemetryText) -> anyhow::Result<Option<StoredTrace>>;
    async fn find_metrics(&self, query: MetricQuery) -> anyhow::Result<Vec<StoredMetric>>;
}
