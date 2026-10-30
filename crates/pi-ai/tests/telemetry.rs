//! Port of `test/telemetry.test.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use parking_lot::Mutex;
use pi_ai::api::lazy::{LazyStreamObserver, lazy_stream};
use pi_ai::auth::*;
use pi_ai::telemetry::{AI_TELEMETRY_SCHEMA, observe_model_request, observe_model_request_with_clock};
use pi_ai::*;
use pi_telemetry::{
    AttributeValue, InMemoryTelemetryContext, InstrumentOptions, MetricKind, MetricsRef, SpanAttributes, SpanError,
    SpanLink, SpanOptions, SpanOutcome, SpanRef, SpanStatus, TelemetryContext, TelemetryContextExt, TelemetrySpan,
    attributes,
};
use serde_json::json;

fn model() -> Model {
    Model {
        id: "model".into(),
        name: "Model".into(),
        api: "telemetry-test".into(),
        provider: "test".into(),
        base_url: "https://example.test".into(),
        reasoning: true,
        input: vec![ModelInputModality::Text],
        cost: ModelCost { input: 1.0, output: 2.0, cache_read: 3.0, cache_write: 4.0, ..Default::default() },
        context_window: 1000,
        max_tokens: 100,
        ..Default::default()
    }
}

fn handle() -> DeferredHandle {
    DeferredHandle {
        provider: "test".into(),
        model_id: "model".into(),
        api: "telemetry-test".into(),
        id: "deferred-secret".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    }
}

fn tool_call() -> ToolCall {
    ToolCall {
        id: "call-secret".into(),
        name: "lookup".into(),
        arguments: json!({ "value": "argument-secret" }).as_object().unwrap().clone(),
        ..Default::default()
    }
}

fn message(stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "telemetry-test".into(),
        provider: "test".into(),
        model: "model".into(),
        content: vec![
            AssistantContent::Text(TextContent { text: "text-secret".into(), ..Default::default() }),
            AssistantContent::Thinking(ThinkingContent { thinking: "thinking-secret".into(), ..Default::default() }),
            AssistantContent::ToolCall(tool_call()),
        ],
        usage: Usage {
            input: 11,
            output: 22,
            cache_read: 33,
            cache_write: 44,
            total_tokens: 110,
            cost: UsageCost { input: 0.1, output: 0.2, cache_read: 0.3, cache_write: 0.4, total: 1.0 },
            ..Default::default()
        },
        response_id: Some("response".into()),
        stop_reason,
        timestamp: now_ms(),
        // TS streams time the shared message object in place; preset it so copies compare equal.
        duration_ms: Some(5),
        ..Default::default()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn events(result: &AssistantMessage) -> Vec<AssistantMessageEvent> {
    let partial = result.clone();
    let mut events = vec![
        AssistantMessageEvent::Start { partial: partial.clone() },
        AssistantMessageEvent::TextDelta { content_index: 0, delta: "text-secret".into(), partial: partial.clone() },
        AssistantMessageEvent::TextEnd { content_index: 0, content: "text-secret".into(), partial: partial.clone() },
        AssistantMessageEvent::ThinkingEnd { content_index: 1, content: "thinking-secret".into(), partial: partial.clone() },
        AssistantMessageEvent::ToolCallDelta { content_index: 2, delta: "{\"value\":".into(), partial: partial.clone() },
        AssistantMessageEvent::ToolCallDelta {
            content_index: 2,
            delta: "\"argument-secret\"}".into(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolCallEnd { content_index: 2, tool_call: tool_call(), partial: partial.clone() },
    ];
    events.push(match result.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            AssistantMessageEvent::Error { reason: result.stop_reason, error: result.clone() }
        }
        StopReason::Pending => AssistantMessageEvent::Done { reason: StopReason::Deferred, message: result.clone() },
        reason => AssistantMessageEvent::Done { reason, message: result.clone() },
    });
    events
}

struct Respond {
    events: Vec<AssistantMessageEvent>,
    result: AssistantMessage,
}

impl Respond {
    fn respond(&self) -> AssistantMessageEventStream {
        let (sender, stream) = AssistantMessageEventStream::new();
        for event in &self.events {
            sender.push(event.clone());
        }
        sender.end(Some(self.result.clone()));
        stream
    }
}

impl ProviderStreams for Respond {
    fn stream(&self, _m: &Model, _c: &TranscriptContext, _o: StreamOptions) -> AssistantMessageEventStream {
        self.respond()
    }
    fn stream_simple(&self, _m: &Model, _c: &TranscriptContext, _o: StreamOptions) -> AssistantMessageEventStream {
        self.respond()
    }
    fn supports_fetch_deferred(&self) -> bool {
        true
    }
    fn fetch_deferred(&self, _m: &Model, _h: &DeferredHandle, _o: StreamOptions) -> AssistantMessageEventStream {
        self.respond()
    }
}

struct Ambient;
#[async_trait]
impl ApiKeyAuth for Ambient {
    fn name(&self) -> &str {
        "Test"
    }
    async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        Ok(Some(AuthResult::default()))
    }
}

fn setup(result: AssistantMessage) -> (Models, Vec<AssistantMessageEvent>) {
    let models = create_models(CreateModelsOptions::default());
    let stream_events = events(&result);
    let provider = create_provider(CreateProviderOptions {
        id: "test".into(),
        models: vec![AnyModel::Chat(model())],
        auth: ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None },
        api: Some(ProviderApi::Single(Arc::new(Respond { events: stream_events.clone(), result }))),
        ..Default::default()
    })
    .unwrap();
    models.set_provider(provider);
    (models, stream_events)
}

async fn settled(telemetry: &InMemoryTelemetryContext) {
    for _ in 0..200 {
        if telemetry.get_spans().iter().all(|s| s.settled) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("spans did not settle");
}

fn context() -> Context {
    Context { messages: vec![], ..Default::default() }
}

fn with_parent(span: &SpanRef, reasoning: Option<ThinkingLevel>) -> StreamOptions {
    StreamOptions { telemetry: Some(span.clone()), reasoning, ..Default::default() }
}

fn attrs(value: serde_json::Value) -> SpanAttributes {
    serde_json::from_value(value).unwrap()
}

#[derive(Clone, Copy, Debug)]
enum Method {
    Stream,
    StreamSimple,
    StreamDeferred,
}

async fn observes(method: Method) {
    let telemetry = InMemoryTelemetryContext::new();
    let result = message(StopReason::Stop);
    let (models, stream_events) = setup(result.clone());
    telemetry
        .start_span("caller", |parent| async move {
            let options = with_parent(&parent, Some(ThinkingLevel::High));
            let mut stream = match method {
                Method::StreamDeferred => {
                    models.stream_deferred(&model(), &handle(), StreamOptions { telemetry: Some(parent.clone()), ..Default::default() })
                }
                Method::Stream => models.stream(&model(), &context(), options),
                Method::StreamSimple => models.stream_simple(&model(), &context(), options),
            };
            let mut received = Vec::new();
            while let Some(event) = stream.next().await {
                received.push(event);
            }
            assert_eq!(received, stream_events);
            assert_eq!(stream.result().await, result);
        })
        .await;
    settled(&telemetry).await;
    let spans = telemetry.get_spans();
    let (parent, span) = (&spans[0], &spans[1]);
    assert_eq!(span.name, if matches!(method, Method::StreamDeferred) { "pi.ai.poll" } else { "pi.ai.request" });
    assert_eq!(span.parent_id, Some(parent.id));
    let mut expected = json!({ "provider": "test", "api": "telemetry-test", "model": "model" });
    if !matches!(method, Method::StreamDeferred) {
        expected["reasoning"] = json!("high");
    }
    for (key, value) in [
        ("stop", json!("stop")),
        ("usage.input", json!(11)),
        ("usage.output", json!(22)),
        ("usage.cache_read", json!(33)),
        ("usage.cache_write", json!(44)),
        ("cost.total", json!(1)),
        ("response.id", json!("response")),
    ] {
        expected[key] = value;
    }
    let mut actual: Vec<(String, AttributeValue)> = span.attributes.clone().into_iter().collect();
    let mut expected: Vec<(String, AttributeValue)> = attrs(expected).into_iter().collect();
    actual.sort_by(|a, b| a.0.cmp(&b.0));
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(actual, expected);
    let events: Vec<(String, SpanAttributes)> = span.events.iter().map(|e| (e.name.clone(), e.attributes.clone())).collect();
    assert_eq!(
        events,
        vec![
            ("pi.ai.first_chunk".to_string(), attributes! {}),
            ("pi.ai.block".to_string(), attributes! { "type" => "text", "index" => 0, "chars" => 11 }),
            ("pi.ai.block".to_string(), attributes! { "type" => "thinking", "index" => 1, "chars" => 15 }),
            (
                "pi.ai.block".to_string(),
                attributes! { "type" => "tool_call", "index" => 2, "chars" => 27, "tool" => "lookup" }
            ),
        ]
    );
    let serialized = serde_json::to_string(span).unwrap();
    for secret in ["text-secret", "thinking-secret", "argument-secret", "call-secret", "deferred-secret"] {
        assert!(!serialized.contains(secret), "{secret} leaked: {serialized}");
    }
    let measurements = telemetry.get_metrics();
    assert_eq!(measurements.len(), 7);
    let counters: Vec<f64> =
        measurements.iter().filter(|m| m.kind == MetricKind::Counter).map(|m| m.value).collect();
    assert_eq!(counters, [11.0, 22.0, 33.0, 44.0, 1.0]);
    for measurement in &measurements {
        let mut keys: Vec<&str> = measurement.attributes.keys().map(String::as_str).collect();
        keys.sort();
        let expected: &[&str] = match measurement.name.as_str() {
            "pi.ai.tokens" => &["kind", "model", "provider"],
            "pi.ai.request.duration" => &["model", "provider", "stop"],
            _ => &["model", "provider"],
        };
        assert_eq!(keys, expected);
        if measurement.kind == MetricKind::Histogram {
            assert_eq!(measurement.context.as_ref(), Some(&span.context));
        }
    }
}

#[tokio::test]
async fn observes_stream_without_changing_delivered_events_or_results() {
    observes(Method::Stream).await;
}

#[tokio::test]
async fn observes_stream_simple_without_changing_delivered_events_or_results() {
    observes(Method::StreamSimple).await;
}

#[tokio::test]
async fn observes_stream_deferred_without_changing_delivered_events_or_results() {
    observes(Method::StreamDeferred).await;
}

#[tokio::test]
async fn reports_error_and_aborted_status_without_error_attributes_or_events() {
    for reason in [StopReason::Error, StopReason::Aborted] {
        let telemetry = InMemoryTelemetryContext::new();
        let result = AssistantMessage { error_message: Some("error-secret".into()), ..message(reason) };
        let (models, _) = setup(result.clone());
        let received = telemetry
            .start_span("caller", |span| async move {
                models.complete_simple(&model(), &context(), with_parent(&span, None)).await
            })
            .await;
        assert_eq!(received, result);
        settled(&telemetry).await;
        let span = &telemetry.get_spans()[1];
        if reason == StopReason::Error {
            assert_eq!(span.status, SpanStatus::error(SpanError::new("provider_error", "error-secret")));
        } else {
            assert_eq!(span.status, SpanStatus::Ok);
        }
        assert_eq!(span.attributes["stop"], AttributeValue::String(reason.as_str().into()));
        assert!(!serde_json::to_string(&span.attributes).unwrap().contains("error-secret"));
        assert!(!serde_json::to_string(&span.events).unwrap().contains("error-secret"));
        if reason == StopReason::Aborted {
            assert!(!serde_json::to_string(span).unwrap().contains("error-secret"));
        }
    }
}

#[tokio::test]
async fn bounded_error_messages_keep_a_split_surrogate_like_javascript_slice() {
    let telemetry = InMemoryTelemetryContext::new();
    let mut result = message(StopReason::Error);
    result.error_message = Some(format!("{}\u{1F600}tail", "x".repeat(499)));
    let (models, _) = setup(result);
    telemetry
        .start_span("caller", |span| async move { models.fetch_deferred(&model(), &handle(), with_parent(&span, None)).await })
        .await;
    settled(&telemetry).await;
    // `errorMessage.slice(0, 500)` keeps the high surrogate of U+1F600.
    let mut units: Vec<u16> = "x".repeat(499).encode_utf16().collect();
    units.push(0xd83d);
    let expected = pi_json::strict::JsonString::from_utf16(units);
    assert_eq!(telemetry.get_spans()[1].status, SpanStatus::error(SpanError::new("provider_error", expected)));
}

#[tokio::test]
async fn bounds_poll_error_messages_including_missing_messages() {
    for missing in [false, true] {
        let telemetry = InMemoryTelemetryContext::new();
        let mut result = message(StopReason::Error);
        if !missing {
            result.error_message = Some(format!("{}discarded-tail", "x".repeat(500)));
        }
        let (models, _) = setup(result.clone());
        let received = telemetry
            .start_span("caller", |span| async move {
                models.fetch_deferred(&model(), &handle(), with_parent(&span, None)).await
            })
            .await;
        assert_eq!(received, result);
        settled(&telemetry).await;
        let span = &telemetry.get_spans()[1];
        let expected = if missing { String::new() } else { "x".repeat(500) };
        assert_eq!(span.status, SpanStatus::error(SpanError::new("provider_error", expected)));
        assert!(!serde_json::to_string(span).unwrap().contains("discarded-tail"));
        assert!(!serde_json::to_string(&span.attributes).unwrap().contains("xxx"));
        assert!(!serde_json::to_string(&span.events).unwrap().contains("xxx"));
    }
}

#[tokio::test]
async fn includes_setup_failures_and_omits_non_scalar_native_reasoning() {
    let telemetry = InMemoryTelemetryContext::new();
    let models = create_models(CreateModelsOptions::default());
    let result = telemetry
        .start_span("caller", |span| async move {
            let options = StreamOptions { telemetry: Some(span), ..Default::default() }
                .with_extra("reasoning", json!({ "effort": "native-secret" }));
            models.complete(&model(), &context(), options).await
        })
        .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    settled(&telemetry).await;
    let span = &telemetry.get_spans()[1];
    assert!(!span.attributes.contains_key("reasoning"));
    let events: Vec<&str> = span.events.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(events, ["pi.ai.first_chunk"]);
    assert!(span.events[0].attributes.is_empty());
    assert_eq!(span.status, SpanStatus::error(SpanError::new("provider_error", "Unknown provider: test")));
    assert!(!serde_json::to_string(&span.attributes).unwrap().contains("Unknown provider"));
    assert!(!serde_json::to_string(span).unwrap().contains("native-secret"));
}

#[tokio::test]
async fn counts_interleaved_tool_deltas_independently() {
    let telemetry = InMemoryTelemetryContext::new();
    let result = message(StopReason::Stop);
    telemetry
        .start_span("caller", |span| async move {
            let observer = observe_model_request(&model(), Some(&with_parent(&span, None)), false);
            let on_event = observer.on_event.clone().unwrap();
            let call = ToolCall { id: "call".into(), name: "lookup".into(), ..Default::default() };
            for (index, delta) in [(2, "ab"), (3, "123"), (2, "c")] {
                on_event(&AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta: delta.into(),
                    partial: result.clone(),
                });
            }
            for index in [3, 2, 4] {
                on_event(&AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: call.clone(),
                    partial: result.clone(),
                });
            }
            (observer.on_result.unwrap())(Some(&result));
        })
        .await;
    settled(&telemetry).await;
    let events: Vec<SpanAttributes> = telemetry.get_spans()[1].events[1..].iter().map(|e| e.attributes.clone()).collect();
    assert_eq!(
        events,
        vec![
            attributes! { "type" => "tool_call", "index" => 3, "chars" => 3, "tool" => "lookup" },
            attributes! { "type" => "tool_call", "index" => 2, "chars" => 3, "tool" => "lookup" },
            attributes! { "type" => "tool_call", "index" => 4, "chars" => 0, "tool" => "lookup" },
        ]
    );
}

/// `{ ...span, metrics: undefined }`: a span without the metrics capability.
struct TracingOnly(SpanRef);

impl TelemetryContext for TracingOnly {
    fn begin_span(&self, options: SpanOptions) -> SpanRef {
        self.0.begin_span(options)
    }
}

impl TelemetrySpan for TracingOnly {
    fn add_link(&self, link: SpanLink) {
        self.0.add_link(link)
    }
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        self.0.add_event(name, attributes)
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        self.0.set_attributes(attributes)
    }
    fn set_status(&self, status: SpanStatus) {
        self.0.set_status(status)
    }
    fn end(&self, outcome: SpanOutcome) {
        self.0.end(outcome)
    }
}

#[tokio::test]
async fn supports_a_tracing_only_span_without_metric_capabilities() {
    let adapter = InMemoryTelemetryContext::new();
    let (models, _) = setup(message(StopReason::Stop));
    adapter
        .start_span("caller", |span| async move {
            let telemetry: SpanRef = Arc::new(TracingOnly(span));
            models.complete_simple(&model(), &context(), StreamOptions { telemetry: Some(telemetry), ..Default::default() }).await
        })
        .await;
    settled(&adapter).await;
    assert_eq!(adapter.get_spans().len(), 2);
    assert!(adapter.get_metrics().is_empty());
}

#[tokio::test]
async fn defaults_to_no_op_telemetry() {
    let (models, _) = setup(message(StopReason::Stop));
    assert_eq!(models.complete_simple(&model(), &context(), StreamOptions::default()).await.stop_reason, StopReason::Stop);
    assert!(AI_TELEMETRY_SCHEMA.spans["pi.ai.request"].start_attributes["provider"].required);
}

#[tokio::test]
async fn records_low_cardinality_metrics_with_monotonic_durations() {
    let telemetry = InMemoryTelemetryContext::new();
    let result = message(StopReason::Stop);
    let clock = Arc::new(AtomicU64::new(100));
    let now = {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst) as f64)
    };
    telemetry
        .start_span("caller", |span| async move {
            let observer = observe_model_request_with_clock(&model(), Some(&with_parent(&span, None)), false, now);
            clock.store(125, Ordering::SeqCst);
            (observer.on_event.unwrap())(&AssistantMessageEvent::Start { partial: result.clone() });
            clock.store(175, Ordering::SeqCst);
            (observer.on_result.unwrap())(Some(&result));
        })
        .await;
    settled(&telemetry).await;
    let metrics = telemetry.get_metrics();
    let actual: Vec<(String, f64, SpanAttributes, Option<String>)> =
        metrics.iter().map(|m| (m.name.clone(), m.value, m.attributes.clone(), m.unit.clone())).collect();
    let mut expected = vec![
        (
            "pi.ai.first_chunk.duration".to_string(),
            25.0,
            attributes! { "provider" => "test", "model" => "model" },
            Some("ms".to_string()),
        ),
        (
            "pi.ai.request.duration".to_string(),
            75.0,
            attributes! { "provider" => "test", "model" => "model", "stop" => "stop" },
            Some("ms".to_string()),
        ),
    ];
    for (kind, value) in [("input", 11.0), ("output", 22.0), ("cache_read", 33.0), ("cache_write", 44.0)] {
        expected.push((
            "pi.ai.tokens".to_string(),
            value,
            attributes! { "provider" => "test", "model" => "model", "kind" => kind },
            Some("token".to_string()),
        ));
    }
    expected.push((
        "pi.ai.cost".to_string(),
        1.0,
        attributes! { "provider" => "test", "model" => "model" },
        Some("USD".to_string()),
    ));
    assert_eq!(actual, expected);
    assert!(metrics[..2].iter().all(|m| m.context.is_some()));
}

#[tokio::test]
async fn records_usage_for_a_poll_only_when_final() {
    for reason in [StopReason::Pending, StopReason::Deferred, StopReason::Stop, StopReason::Error] {
        let telemetry = InMemoryTelemetryContext::new();
        let (models, _) = setup(message(reason));
        telemetry
            .start_span("caller", |span| async move {
                models.fetch_deferred(&model(), &handle(), with_parent(&span, None)).await
            })
            .await;
        settled(&telemetry).await;
        let counters = telemetry.get_metrics().into_iter().filter(|m| m.kind == MetricKind::Counter).count();
        let expected = if matches!(reason, StopReason::Pending | StopReason::Deferred) { 0 } else { 5 };
        assert_eq!(counters, expected, "{reason:?}");
    }
}

/// Wraps the in-memory adapter's span so the metrics capability can be counted.
struct CountingSpan {
    inner: SpanRef,
    metrics: MetricsRef,
}

impl TelemetryContext for CountingSpan {
    fn metrics(&self) -> Option<MetricsRef> {
        Some(self.metrics.clone())
    }
    fn begin_span(&self, options: SpanOptions) -> SpanRef {
        self.inner.begin_span(options)
    }
}

impl TelemetrySpan for CountingSpan {
    fn add_link(&self, link: SpanLink) {
        self.inner.add_link(link)
    }
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        self.inner.add_event(name, attributes)
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        self.inner.set_attributes(attributes)
    }
    fn set_status(&self, status: SpanStatus) {
        self.inner.set_status(status)
    }
    fn end(&self, outcome: SpanOutcome) {
        self.inner.end(outcome)
    }
}

struct CountingMetrics {
    inner: Arc<InMemoryTelemetryContext>,
    histograms: Mutex<Vec<String>>,
    counters: Mutex<Vec<String>>,
}

impl pi_telemetry::TelemetryMetrics for CountingMetrics {
    fn counter(&self, name: &str, options: InstrumentOptions) -> pi_telemetry::CounterRef {
        self.counters.lock().push(name.to_string());
        self.inner.counter(name, options)
    }
    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> pi_telemetry::UpDownCounterRef {
        pi_telemetry::TelemetryMetrics::up_down_counter(&*self.inner, name, options)
    }
    fn histogram(&self, name: &str, options: InstrumentOptions) -> pi_telemetry::HistogramRef {
        self.histograms.lock().push(name.to_string());
        self.inner.histogram(name, options)
    }
}

#[tokio::test]
async fn creates_instruments_once_per_metrics_object_across_requests() {
    let telemetry = InMemoryTelemetryContext::new();
    let counting = Arc::new(CountingMetrics {
        inner: telemetry.clone(),
        histograms: Mutex::default(),
        counters: Mutex::default(),
    });
    let metrics: MetricsRef = counting.clone();
    let (models, _) = setup(message(StopReason::Stop));
    let wrap = |span: SpanRef| -> SpanRef { Arc::new(CountingSpan { inner: span, metrics: metrics.clone() }) };
    for call in 0..3 {
        let models = models.clone();
        let wrap = &wrap;
        telemetry
            .start_span("caller", |span| async move {
                let options = StreamOptions { telemetry: Some(wrap(span)), ..Default::default() };
                match call {
                    0 => models.complete_simple(&model(), &context(), options).await,
                    1 => models.fetch_deferred(&model(), &handle(), options).await,
                    _ => models.complete(&model(), &context(), options).await,
                }
            })
            .await;
    }
    settled(&telemetry).await;
    assert_eq!(*counting.histograms.lock(), ["pi.ai.request.duration", "pi.ai.first_chunk.duration"]);
    assert_eq!(*counting.counters.lock(), ["pi.ai.tokens", "pi.ai.cost"]);
    assert_eq!(telemetry.get_metrics().len(), 21);
}

// ---- lazyStream observation ----

#[tokio::test]
async fn lazy_stream_observes_after_push_once_per_terminal_result() {
    let result = message(StopReason::Stop);
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let observed_result = result.clone();
    let source_result = result.clone();
    let on_event_order = order.clone();
    let on_result_order = order.clone();
    let stream = lazy_stream(
        &model(),
        Box::pin(async move {
            let (sender, stream) = AssistantMessageEventStream::new();
            sender.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message: source_result.clone() });
            sender.end(Some(source_result));
            Ok(stream)
        }),
        Some(LazyStreamObserver {
            on_event: Some(Arc::new(move |_| on_event_order.lock().push("event"))),
            on_result: Some(Arc::new(move |received| {
                assert_eq!(received, Some(&observed_result));
                on_result_order.lock().push("result");
            })),
        }),
    );
    assert_eq!(stream.result().await, result);
    for _ in 0..100 {
        if order.lock().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(*order.lock(), ["event", "result"]);
}

#[tokio::test]
async fn lazy_stream_reports_setup_failure_events_and_the_result_once() {
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let (on_event, on_result) = (observed.clone(), observed.clone());
    let stream = lazy_stream(
        &model(),
        Box::pin(async { Err(anyhow::anyhow!("setup-secret")) }),
        Some(LazyStreamObserver {
            on_event: Some(Arc::new(move |event| on_event.lock().push(event.type_name().to_string()))),
            on_result: Some(Arc::new(move |result| {
                on_result.lock().push(result.map(|r| r.stop_reason.as_str().to_string()).unwrap_or("missing".into()))
            })),
        }),
    );
    assert_eq!(stream.result().await.error_message.as_deref(), Some("setup-secret"));
    for _ in 0..100 {
        if observed.lock().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(*observed.lock(), ["error", "error"]);
}
