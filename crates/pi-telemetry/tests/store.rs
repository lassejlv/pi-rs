//! Port of `test/store-conformance.ts` and `test/store.test.ts`.

use std::sync::Arc;

use pi_telemetry::{
    AttributeValue, InMemoryTelemetryContext, InstrumentOptions, ManualTelemetryClock, MetricKind,
    MetricQuery, SpanError, SpanLink, SpanOptions, SpanStatus, StoredEvent, StoredSpan,
    StoredTrace, TelemetryCarrier, TelemetryContextExt, TelemetryStore, TraceQuery, attributes,
};

struct StoreFixture {
    context: Arc<InMemoryTelemetryContext>,
    store: Arc<dyn TelemetryStore>,
}

impl StoreFixture {
    async fn flush(&self) {}
}

fn factory(clock: &Arc<ManualTelemetryClock>) -> StoreFixture {
    let context = InMemoryTelemetryContext::with_clock(clock.clone());
    StoreFixture {
        store: context.clone(),
        context,
    }
}

fn scalars(pairs: &[(&str, AttributeValue)]) -> Option<indexmap::IndexMap<String, AttributeValue>> {
    Some(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

#[tokio::test]
async fn returns_only_settled_traces_complete_span_snapshots_and_detached_results() {
    let clock = ManualTelemetryClock::new(20_000.0);
    let f = factory(&clock);
    let (release, gate) = tokio::sync::oneshot::channel::<()>();
    let active = tokio::spawn(f.context.start_span(
        SpanOptions::new("root").attributes(attributes! { "tags" => vec!["original"] }),
        |root| async move {
            root.add_event("event", attributes! { "tags" => vec!["original"] });
            let _ = gate.await;
            root.start_span("child", |_| async {}).await;
        },
    ));
    tokio::task::yield_now().await;
    f.flush().await;
    assert!(
        f.store
            .find_traces(TraceQuery::default())
            .await
            .unwrap()
            .traces
            .is_empty()
    );
    assert_eq!(f.store.get_trace(&"missing".into()).await.unwrap(), None);
    release.send(()).unwrap();
    active.await.unwrap();
    f.flush().await;
    let trace = f
        .store
        .find_traces(TraceQuery::default())
        .await
        .unwrap()
        .traces
        .remove(0);
    let mut root = trace
        .spans
        .iter()
        .find(|span| span.name == "root")
        .unwrap()
        .clone();
    let child = trace
        .spans
        .iter()
        .find(|span| span.name == "child")
        .unwrap();
    let id_pattern = regex_like(&root.id.to_string());
    assert!(id_pattern, "unexpected span id {}", root.id);
    assert_eq!(root.trace_id, root.id);
    assert_eq!(root.parent_id, None);
    assert_eq!(child.parent_id.as_ref(), Some(&root.id));
    assert_eq!(child.trace_id, root.trace_id);
    assert_eq!(root.started_ms, 20_000.0);
    assert!(root.ended_ms >= root.started_ms);
    assert!(root.events[0].time_ms >= 0.0);
    root.attributes
        .insert("tags".into(), AttributeValue::from(vec!["changed"]));
    root.events[0]
        .attributes
        .insert("tags".into(), AttributeValue::from(vec!["changed"]));
    let fresh = f.store.get_trace(&trace.trace_id).await.unwrap().unwrap();
    let fresh_root = fresh.spans.iter().find(|span| span.id == root.id).unwrap();
    assert_eq!(
        fresh_root.attributes["tags"],
        AttributeValue::from(vec!["original"])
    );
    assert_eq!(
        fresh_root.events[0].attributes["tags"],
        AttributeValue::from(vec!["original"])
    );
}

/// `^[0-9a-f-]{36}:[1-9][0-9]*$`
fn regex_like(id: &str) -> bool {
    let Some((scope, number)) = id.split_once(':') else {
        return false;
    };
    scope.len() == 36
        && scope
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || c == '-')
        && !number.is_empty()
        && !number.starts_with('0')
        && number.chars().all(|c| c.is_ascii_digit())
}

#[tokio::test]
async fn filters_by_first_span_name_and_start_any_error_and_all_attributes_on_one_span() {
    let clock = ManualTelemetryClock::new(10_000.0);
    let f = factory(&clock);
    let inner = clock.clone();
    f.context
        .start_span("old", |root| async move {
            inner.set_now(30_000.0);
            root.start_span(
                SpanOptions::new("recent-child").attributes(attributes! { "left" => 1 }),
                |_| async {},
            )
            .await;
            root.start_span(
                SpanOptions::new("split").attributes(attributes! { "right" => 2 }),
                |_| async {},
            )
            .await;
        })
        .await;
    clock.set_now(20_000.0);
    let inner = clock.clone();
    f.context
        .start_span("wanted", |root| async move {
            inner.set_now(20_001.0);
            root.start_span(
                SpanOptions::new("failure").attributes(attributes! {
                    "left" => 1, "right" => 2, "valid" => true, "odd.\"key" => "value"
                }),
                |child| async move { child.set_status(SpanStatus::Error(None)) },
            )
            .await;
        })
        .await;
    clock.set_now(40_000.0);
    f.context
        .start_span(
            SpanOptions::new("new").attributes(attributes! { "valid" => 1 }),
            |_| async {},
        )
        .await;
    f.flush().await;
    let names = |query: TraceQuery| {
        let store = f.store.clone();
        async move {
            store
                .find_traces(query)
                .await
                .unwrap()
                .traces
                .iter()
                .map(|trace| trace.spans[0].name.clone())
                .collect::<Vec<_>>()
        }
    };
    fn n(value: impl Into<AttributeValue>) -> AttributeValue {
        value.into()
    }
    assert_eq!(
        names(TraceQuery {
            name: Some("wanted".into()),
            attributes: scalars(&[("left", n(1)), ("right", n(2))]),
            errors: true,
            start_min: Some(20_000.0),
            start_max: Some(20_000.0),
            ..Default::default()
        })
        .await,
        vec!["wanted"]
    );
    assert!(
        names(TraceQuery {
            name: Some("failure".into()),
            ..Default::default()
        })
        .await
        .is_empty()
    );
    assert_eq!(
        names(TraceQuery {
            attributes: scalars(&[("left", n(1)), ("right", n(2))]),
            ..Default::default()
        })
        .await,
        vec!["wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            attributes: scalars(&[("valid", n(true)), ("odd.\"key", n("value"))]),
            ..Default::default()
        })
        .await,
        vec!["wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            attributes: scalars(&[("valid", n(1))]),
            ..Default::default()
        })
        .await,
        vec!["new"]
    );
    assert_eq!(
        names(TraceQuery {
            errors: true,
            ..Default::default()
        })
        .await,
        vec!["wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            start_min: Some(20_000.0),
            ..Default::default()
        })
        .await,
        vec!["new", "wanted"]
    );
    assert_eq!(
        names(TraceQuery {
            start_max: Some(10_000.0),
            ..Default::default()
        })
        .await,
        vec!["old"]
    );
}

#[tokio::test]
async fn pages_complete_traces_without_duplicates_when_trace_starts_tie() {
    let clock = ManualTelemetryClock::new(20_000.0);
    let f = factory(&clock);
    for index in 0..25 {
        clock.set_now(20_000.0);
        let inner = clock.clone();
        f.context
            .start_span(
                SpanOptions::new("root").attributes(attributes! { "index" => index }),
                |root| async move {
                    inner.set_now(20_001.0);
                    root.start_span("child", |_| async {}).await;
                },
            )
            .await;
    }
    f.flush().await;
    let first = f.store.find_traces(TraceQuery::default()).await.unwrap();
    assert_eq!(first.traces.len(), 20);
    assert!(first.cursor.is_some());
    let rest = f
        .store
        .find_traces(TraceQuery {
            cursor: first.cursor.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(rest.traces.len(), 5);
    assert_eq!(rest.cursor, None);
    let all = f
        .store
        .find_traces(TraceQuery {
            limit: Some(100),
            ..Default::default()
        })
        .await
        .unwrap()
        .traces;
    let combined: Vec<StoredTrace> = first
        .traces
        .iter()
        .chain(rest.traces.iter())
        .cloned()
        .collect();
    assert_eq!(combined, all);
    assert!(all.iter().all(|trace| trace.spans.len() == 2));
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = f
            .store
            .find_traces(TraceQuery {
                name: Some("root".into()),
                limit: Some(3),
                cursor,
                ..Default::default()
            })
            .await
            .unwrap();
        ids.extend(page.traces.iter().map(|trace| trace.trace_id.clone()));
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        ids,
        all.iter()
            .map(|trace| trace.trace_id.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn inherits_remote_trace_ids_and_preserves_remote_parents_and_cross_trace_links() {
    let clock = ManualTelemetryClock::new(20_000.0);
    let sender = factory(&clock);
    let receiver = factory(&clock);
    let inner = sender.context.clone();
    let carrier = sender
        .context
        .start_span("sender", |span| async move {
            inner.inject(span.as_ref()).unwrap()
        })
        .await;
    sender.flush().await;
    let sent = sender
        .store
        .find_traces(TraceQuery::default())
        .await
        .unwrap()
        .traces
        .remove(0);
    let wire: TelemetryCarrier =
        serde_json::from_str(&serde_json::to_string(&carrier).unwrap()).unwrap();
    let remote = receiver.context.extract(Some(&wire));
    remote.start_span("receiver", |_| async {}).await;
    receiver
        .context
        .start_span(
            SpanOptions::new("linked").link(SpanLink::with_attributes(
                remote.clone(),
                attributes! { "reason" => "background" },
            )),
            |_| async {},
        )
        .await;
    receiver.flush().await;
    let continued = receiver
        .store
        .get_trace(&sent.trace_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(continued.spans.len(), 1);
    assert_eq!(continued.spans[0].trace_id, sent.trace_id);
    assert_eq!(
        continued.spans[0].parent_id.as_ref(),
        Some(&sent.spans[0].id)
    );
    assert_eq!(continued.spans[0].name, "receiver");
    assert_ne!(continued.spans[0].id, sent.spans[0].id);
    let linked = receiver
        .store
        .find_traces(TraceQuery {
            name: Some("linked".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .traces
        .remove(0);
    assert_ne!(linked.trace_id, sent.trace_id);
    assert_eq!(linked.spans[0].links.len(), 1);
    assert_eq!(linked.spans[0].links[0].trace_id, sent.trace_id);
    assert_eq!(linked.spans[0].links[0].span_id, sent.spans[0].id);
    assert_eq!(
        linked.spans[0].links[0].attributes,
        Some(attributes! { "reason" => "background" })
    );
    // Incomplete carriers fall back to roots. (Non-string values cannot be represented in Rust carriers.)
    for key in carrier.keys() {
        let mut incomplete = carrier.clone();
        incomplete.shift_remove(key);
        receiver
            .context
            .extract(Some(&incomplete))
            .start_span("fallback", |_| async {})
            .await;
    }
    receiver.flush().await;
    let fallbacks = receiver
        .store
        .find_traces(TraceQuery {
            name: Some("fallback".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .traces;
    assert_eq!(fallbacks.len(), carrier.len());
    assert!(
        fallbacks
            .iter()
            .all(|trace| trace.spans[0].parent_id.is_none() && trace.trace_id == trace.spans[0].id)
    );
}

#[tokio::test]
async fn accepts_external_string_identities_without_imposing_an_id_format() {
    let clock = ManualTelemetryClock::new(20_000.0);
    let f = factory(&clock);
    let inner = f.context.clone();
    let carrier = f
        .context
        .start_span("source", |span| async move {
            inner.inject(span.as_ref()).unwrap()
        })
        .await;
    let external: TelemetryCarrier = carrier
        .keys()
        .map(|key| (key.clone(), format!("external-{key}")))
        .collect();
    let parent = f.context.extract(Some(&external));
    assert_eq!(f.context.inject(parent.as_ref()), Some(external.clone()));
    parent.start_span("continued", |_| async {}).await;
    f.flush().await;
    let trace = f
        .store
        .find_traces(TraceQuery {
            name: Some("continued".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .traces
        .remove(0);
    assert!(trace.trace_id.to_string().starts_with("external-"));
    assert!(
        trace.spans[0]
            .parent_id
            .as_ref()
            .unwrap()
            .to_string()
            .starts_with("external-")
    );
}

#[tokio::test]
async fn queries_metric_names_inclusive_interval_bounds_typed_attribute_equality_and_detached_values()
 {
    let clock = ManualTelemetryClock::new(20_001.0);
    let f = factory(&clock);
    let counter = f
        .context
        .counter("hits", InstrumentOptions::new().unit("By"));
    counter.add(2.0, attributes! { "method" => "read", "valid" => true });
    counter.add(3.0, attributes! { "valid" => true, "method" => "read" });
    counter.add(11.0, attributes! { "method" => "write", "valid" => 1 });
    f.context
        .counter("other", InstrumentOptions::new())
        .add(100.0, attributes! { "method" => "read", "valid" => true });
    clock.set_now(30_001.0);
    counter.add(7.0, attributes! { "method" => "read", "valid" => true });
    clock.set_now(40_001.0);
    f.flush().await;
    fn n(value: impl Into<AttributeValue>) -> AttributeValue {
        value.into()
    }
    let query = MetricQuery {
        name: "hits".into(),
        start_min: Some(20_000.0),
        start_max: Some(20_000.0),
        attributes: scalars(&[("method", n("read")), ("valid", n(true))]),
    };
    let mut rows = f.store.find_metrics(query.clone()).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.count).sum::<f64>(), 2.0);
    assert_eq!(rows.iter().map(|row| row.sum).sum::<f64>(), 5.0);
    assert_eq!(
        rows.iter().map(|row| row.min).fold(f64::INFINITY, f64::min),
        2.0
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.max)
            .fold(f64::NEG_INFINITY, f64::max),
        3.0
    );
    assert!(rows.iter().all(|row| row.kind == MetricKind::Counter
        && row.unit.as_deref() == Some("By")
        && row.interval_ms == 20_000.0));
    rows[0].attributes.insert("method".into(), n("changed"));
    assert_eq!(
        f.store.find_metrics(query).await.unwrap()[0].attributes["method"],
        n("read")
    );
    assert!(
        f.store
            .find_metrics(MetricQuery::new("missing"))
            .await
            .unwrap()
            .is_empty()
    );
    let valid_one = f
        .store
        .find_metrics(MetricQuery {
            attributes: scalars(&[("valid", n(1))]),
            ..MetricQuery::new("hits")
        })
        .await
        .unwrap();
    assert_eq!(valid_one.iter().map(|row| row.sum).sum::<f64>(), 11.0);
    let later = f
        .store
        .find_metrics(MetricQuery {
            start_min: Some(30_000.0),
            ..MetricQuery::new("hits")
        })
        .await
        .unwrap();
    assert_eq!(later.iter().map(|row| row.sum).sum::<f64>(), 7.0);
}

#[tokio::test]
async fn records_wall_clock_boundaries_and_relative_event_times() {
    let clock = ManualTelemetryClock::new(1000.0);
    clock.set_performance_now(100.0);
    let telemetry = InMemoryTelemetryContext::with_clock(clock.clone());
    let (inner, inner_clock) = (telemetry.clone(), clock.clone());
    let id = telemetry
        .start_span("timed", |span| async move {
            let carrier = inner.inject(span.as_ref()).unwrap();
            inner_clock.set_performance_now(110.0);
            span.add_event("event", attributes! { "count" => 1 });
            inner_clock.set_performance_now(125.0);
            pi_telemetry::TelemetryText::from(format!(
                "{}:{}",
                carrier["pi-memory-scope"], carrier["pi-memory-span"]
            ))
        })
        .await;
    assert_eq!(
        telemetry.get_trace(&id),
        Some(StoredTrace {
            trace_id: id.clone(),
            spans: vec![StoredSpan {
                id: id.clone(),
                trace_id: id.clone(),
                parent_id: None,
                name: "timed".into(),
                started_ms: 1000.0,
                ended_ms: 1025.0,
                attributes: attributes! {},
                events: vec![StoredEvent {
                    name: "event".into(),
                    time_ms: 10.0,
                    attributes: attributes! { "count" => 1 }
                }],
                links: vec![],
                status: SpanStatus::Ok,
            }],
        })
    );
    let snapshot = serde_json::to_value(&telemetry.get_spans()[0]).unwrap();
    assert_eq!(
        snapshot["events"],
        serde_json::json!([{ "name": "event", "attributes": { "count": 1 } }])
    );
    assert!(snapshot.get("startedMs").is_none());
}

#[tokio::test]
async fn maps_raw_metric_measurements_to_count_one_rows() {
    let clock = ManualTelemetryClock::new(20_001.0);
    let telemetry = InMemoryTelemetryContext::with_clock(clock);
    telemetry
        .counter("calls", InstrumentOptions::new())
        .add(2.0, attributes! {});
    telemetry
        .counter("calls", InstrumentOptions::new())
        .add(3.0, attributes! {});
    let rows: Vec<(f64, f64, f64)> = telemetry
        .find_metrics(&MetricQuery::new("calls"))
        .iter()
        .map(|row| (row.interval_ms, row.count, row.sum))
        .collect();
    assert_eq!(rows, vec![(20_000.0, 1.0, 2.0), (20_000.0, 1.0, 3.0)]);
    assert_eq!(
        serde_json::to_value(telemetry.get_metrics()).unwrap(),
        serde_json::json!([
            { "kind": "counter", "name": "calls", "value": 2, "attributes": {}, "context": null },
            { "kind": "counter", "name": "calls", "value": 3, "attributes": {}, "context": null },
        ])
    );
}

#[tokio::test]
async fn detaches_link_attributes_and_error_details() {
    let telemetry = InMemoryTelemetryContext::new();
    let target = telemetry
        .start_span("target", |span| async move { span })
        .await;
    telemetry
        .start_span(
            SpanOptions::new("source").link(SpanLink::with_attributes(
                target,
                attributes! { "tags" => vec!["original"] },
            )),
            |span| async move {
                span.set_status(SpanStatus::error(SpanError::new("Expected", "original")))
            },
        )
        .await;
    let mut trace = telemetry
        .find_traces(&TraceQuery {
            name: Some("source".into()),
            ..Default::default()
        })
        .unwrap()
        .traces
        .remove(0);
    let span = &mut trace.spans[0];
    span.links[0]
        .attributes
        .as_mut()
        .unwrap()
        .insert("tags".into(), AttributeValue::from(vec!["changed"]));
    if let SpanStatus::Error(Some(error)) = &mut span.status {
        error.message = "changed".into();
    }
    let fresh = telemetry
        .get_trace(&trace.trace_id)
        .unwrap()
        .spans
        .remove(0);
    assert_eq!(
        fresh.links[0].attributes.as_ref().unwrap()["tags"],
        AttributeValue::from(vec!["original"])
    );
    assert_eq!(
        fresh.status,
        SpanStatus::error(SpanError::new("Expected", "original"))
    );
}
