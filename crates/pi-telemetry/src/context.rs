use std::any::Any;
use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};

use crate::attributes::{SpanAttributes, TelemetryCarrier};
use crate::metrics::MetricsRef;

/// Shared handle to a telemetry context (a root, an extracted remote parent, or a span).
pub type ContextRef = Arc<dyn TelemetryContext>;
/// Shared handle to a span.
pub type SpanRef = Arc<dyn TelemetrySpan>;
/// Shared handle to a propagation capability.
pub type PropagationRef = Arc<dyn TelemetryPropagation>;

/// Whether two handles name the same object (JavaScript `===` on adapter objects).
pub fn same_ref<A: ?Sized, B: ?Sized>(a: &Arc<A>, b: &Arc<B>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

/// Optional transport capability implemented by the application's telemetry adapter.
pub trait TelemetryPropagation: Any + Send + Sync {
    fn inject(&self, context: &dyn TelemetryContext) -> Option<TelemetryCarrier>;
    /// Absent or unknown carriers use the root; recognized identifiers are opaque strings.
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> ContextRef;
}

/// A link from a span to another context, possibly in another trace.
#[derive(Clone)]
pub struct SpanLink {
    pub context: ContextRef,
    pub attributes: Option<SpanAttributes>,
}

impl SpanLink {
    pub fn new(context: ContextRef) -> Self {
        Self {
            context,
            attributes: None,
        }
    }

    pub fn with_attributes(context: ContextRef, attributes: SpanAttributes) -> Self {
        Self {
            context,
            attributes: Some(attributes),
        }
    }
}

impl std::fmt::Debug for SpanLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanLink")
            .field("attributes", &self.attributes)
            .finish_non_exhaustive()
    }
}

/// Options for a new span.
#[derive(Clone, Debug, Default)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: Option<SpanAttributes>,
    pub links: Vec<SpanLink>,
}

impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: None,
            links: Vec::new(),
        }
    }

    pub fn attributes(mut self, attributes: SpanAttributes) -> Self {
        self.attributes = Some(attributes);
        self
    }

    pub fn link(mut self, link: SpanLink) -> Self {
        self.links.push(link);
        self
    }
}

impl From<&str> for SpanOptions {
    fn from(name: &str) -> Self {
        SpanOptions::new(name)
    }
}

impl From<String> for SpanOptions {
    fn from(name: String) -> Self {
        SpanOptions::new(name)
    }
}

/// Error details of a failed span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanError {
    pub name: String,
    pub message: crate::TelemetryText,
}

impl SpanError {
    pub fn new(name: impl Into<String>, message: impl Into<crate::TelemetryText>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }
}

/// `{ status: "ok" }` or `{ status: "error", error?: { name, message } }`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SpanStatus {
    #[default]
    Ok,
    Error(Option<SpanError>),
}

impl SpanStatus {
    pub fn is_error(&self) -> bool {
        matches!(self, SpanStatus::Error(_))
    }

    pub fn error(error: SpanError) -> Self {
        SpanStatus::Error(Some(error))
    }
}

#[derive(Serialize, Deserialize)]
struct SpanStatusRepr {
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<SpanError>,
}

impl Serialize for SpanStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SpanStatus::Ok => SpanStatusRepr {
                status: "ok".into(),
                error: None,
            }
            .serialize(serializer),
            SpanStatus::Error(error) => SpanStatusRepr {
                status: "error".into(),
                error: error.clone(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SpanStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = SpanStatusRepr::deserialize(deserializer)?;
        match repr.status.as_str() {
            "ok" => Ok(SpanStatus::Ok),
            "error" => Ok(SpanStatus::Error(repr.error)),
            other => Err(serde::de::Error::custom(format!(
                "invalid span status {other}"
            ))),
        }
    }
}

/// How a span's callback finished; adapters apply it unless a status was set explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanOutcome {
    /// The callback returned.
    Ok,
    /// The callback failed (threw). `None` when the failure carries no error details.
    Failed(Option<SpanError>),
}

/// Adapter contract for trusted typed inputs; malformed input behavior is unspecified.
pub trait TelemetryContext: Any + Send + Sync {
    /// Add an event to this context's active span. Root and extracted parent
    /// contexts have no local span and ignore it. This distinct native name
    /// preserves the existing `TelemetrySpan::add_event` adapter contract.
    fn record_event(&self, _name: &str, _attributes: SpanAttributes) {}
    /// Add a link to this context's active span, as `TelemetryContext.addLink`.
    /// Root and extracted parent contexts have no local span and ignore it.
    fn record_link(&self, _link: SpanLink) {}
    /// Update an active span through a context handle. Roots and remote parents ignore it.
    fn update_attributes(&self, _attributes: SpanAttributes) {}

    /// Adapter capabilities remain available on child spans and extracted parents.
    fn metrics(&self) -> Option<MetricsRef> {
        None
    }

    fn propagation(&self) -> Option<PropagationRef> {
        None
    }

    /// Admit a child span. Low-level primitive behind [`TelemetryContextExt::start_span`]: the
    /// caller must [`end`](TelemetrySpan::end) the span exactly once.
    fn begin_span(&self, options: SpanOptions) -> SpanRef;
}

/// A recording span; calls after settlement are inert.
pub trait TelemetrySpan: TelemetryContext {
    fn add_link(&self, link: SpanLink);
    fn add_event(&self, name: &str, attributes: SpanAttributes);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
    /// Settle the span. Applies `outcome` unless a status was set explicitly. Later calls are ignored.
    fn end(&self, outcome: SpanOutcome);
}

/// Future of a span callback: settles the span when the callback's future completes.
///
/// The span is admitted and the callback is called synchronously, when the future is created, as
/// in the TypeScript contract. Dropping the future before it completes settles the span with an
/// `AbortError` status; a panic settles it with a `Panic` status.
#[must_use = "futures do nothing unless awaited"]
pub struct SpanFuture<Fut: Future, C = fn(&<Fut as Future>::Output) -> SpanOutcome> {
    future: Pin<Box<Fut>>,
    span: Option<SpanRef>,
    classify: Option<C>,
}

impl<Fut: Future, C: FnOnce(&Fut::Output) -> SpanOutcome> SpanFuture<Fut, C> {
    pub fn new(span: SpanRef, future: Fut, classify: C) -> Self {
        Self {
            future: Box::pin(future),
            span: Some(span),
            classify: Some(classify),
        }
    }
}

impl<Fut: Future, C: FnOnce(&Fut::Output) -> SpanOutcome> Unpin for SpanFuture<Fut, C> {}

impl<Fut: Future, C: FnOnce(&Fut::Output) -> SpanOutcome> Future for SpanFuture<Fut, C> {
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match this.future.as_mut().poll(cx) {
            Poll::Ready(output) => {
                if let (Some(span), Some(classify)) = (this.span.take(), this.classify.take()) {
                    span.end(classify(&output));
                }
                Poll::Ready(output)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Fut: Future, C> Drop for SpanFuture<Fut, C> {
    fn drop(&mut self) {
        if let Some(span) = self.span.take() {
            let error = if std::thread::panicking() {
                SpanError::new("Panic", "The span callback panicked")
            } else {
                SpanError::new("AbortError", "The span callback was cancelled")
            };
            span.end(SpanOutcome::Failed(Some(error)));
        }
    }
}

fn classify_ok<T>(_: &T) -> SpanOutcome {
    SpanOutcome::Ok
}

fn classify_result<T, E: Display>(result: &Result<T, E>) -> SpanOutcome {
    match result {
        Ok(_) => SpanOutcome::Ok,
        Err(error) => SpanOutcome::Failed(Some(SpanError::new("Error", error.to_string()))),
    }
}

/// Callback-style span helpers available on every context.
pub trait TelemetryContextExt {
    /// Run `callback` in a child span. The span settles with status `ok` (unless set explicitly)
    /// when the returned future completes.
    fn start_span<F, Fut>(&self, options: impl Into<SpanOptions>, callback: F) -> SpanFuture<Fut>
    where
        F: FnOnce(SpanRef) -> Fut,
        Fut: Future;

    /// Like [`start_span`](Self::start_span) for fallible callbacks: an `Err` settles the span with
    /// an `Error` status carrying the error's message (unless a status was set explicitly). The
    /// error value is returned unchanged.
    fn try_start_span<F, Fut, T, E>(
        &self,
        options: impl Into<SpanOptions>,
        callback: F,
    ) -> SpanFuture<Fut>
    where
        F: FnOnce(SpanRef) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Display;

    /// Run `callback` in a child span and decide the outcome with `classify`.
    fn start_span_with<F, Fut, C>(
        &self,
        options: impl Into<SpanOptions>,
        callback: F,
        classify: C,
    ) -> SpanFuture<Fut, C>
    where
        F: FnOnce(SpanRef) -> Fut,
        Fut: Future,
        C: FnOnce(&Fut::Output) -> SpanOutcome;
}

impl<X: TelemetryContext + ?Sized> TelemetryContextExt for X {
    fn start_span<F, Fut>(&self, options: impl Into<SpanOptions>, callback: F) -> SpanFuture<Fut>
    where
        F: FnOnce(SpanRef) -> Fut,
        Fut: Future,
    {
        self.start_span_with(
            options,
            callback,
            classify_ok::<Fut::Output> as fn(&Fut::Output) -> SpanOutcome,
        )
    }

    fn try_start_span<F, Fut, T, E>(
        &self,
        options: impl Into<SpanOptions>,
        callback: F,
    ) -> SpanFuture<Fut>
    where
        F: FnOnce(SpanRef) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Display,
    {
        self.start_span_with(
            options,
            callback,
            classify_result::<T, E> as fn(&Result<T, E>) -> SpanOutcome,
        )
    }

    fn start_span_with<F, Fut, C>(
        &self,
        options: impl Into<SpanOptions>,
        callback: F,
        classify: C,
    ) -> SpanFuture<Fut, C>
    where
        F: FnOnce(SpanRef) -> Fut,
        Fut: Future,
        C: FnOnce(&Fut::Output) -> SpanOutcome,
    {
        let span = self.begin_span(options.into());
        // Settles the span if the callback panics before returning its future.
        let guard = PanicGuard(Some(span.clone()));
        let future = callback(span.clone());
        guard.disarm();
        SpanFuture::new(span, future, classify)
    }
}

struct PanicGuard(Option<SpanRef>);

impl PanicGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if let Some(span) = self.0.take() {
            span.end(SpanOutcome::Failed(Some(SpanError::new(
                "Panic",
                "The span callback panicked",
            ))));
        }
    }
}
