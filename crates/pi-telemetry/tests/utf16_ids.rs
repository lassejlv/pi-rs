//! Stored span and trace IDs come from carriers, which an adapter or a persisted
//! durable trace can fill with any JavaScript string. Expected values: the pinned
//! TypeScript `InMemoryTelemetryContext` under Node with `Date.now = () => 1000`.

use pi_telemetry::{
    InMemoryTelemetryContext, ManualTelemetryClock, TelemetryCarrier, TelemetryContextExt,
    TelemetryText, TraceQuery,
};

fn text(literal: &str) -> TelemetryText {
    pi_json::strict::parse(literal)
        .unwrap()
        .as_string()
        .unwrap()
        .into()
}

#[tokio::test]
async fn stored_ids_keep_lone_surrogates_and_javascript_order() {
    let telemetry = InMemoryTelemetryContext::with_clock(ManualTelemetryClock::new(1000.0));
    for (scope, span, trace) in [
        (r#""s\ud800""#, r#""7\udc00""#, r#""t\udfff""#),
        (r#""s""#, r#""1""#, r#""￿""#),
        (r#""s""#, r#""2""#, r#""𐀀""#),
    ] {
        let carrier: TelemetryCarrier = [
            ("pi-memory-scope", text(scope)),
            ("pi-memory-span", text(span)),
            ("pi-memory-trace", text(trace)),
        ]
        .into_iter()
        .collect();
        telemetry
            .extract(Some(&carrier))
            .start_span("a", |_| async {})
            .await;
    }
    let mut pages = Vec::new();
    let mut cursor = None;
    loop {
        let page = telemetry
            .find_traces(&TraceQuery {
                limit: Some(1),
                cursor: cursor.clone(),
                ..Default::default()
            })
            .unwrap();
        let trace = &page.traces[0];
        pages.push((
            trace.trace_id.clone(),
            trace.spans[0].parent_id.clone().unwrap(),
            page.cursor.clone(),
        ));
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    // JavaScript compares code units: U+10000 (D800 DC00) sorts before U+FFFF.
    assert_eq!(
        pages,
        [
            (
                text(r#""t\udfff""#),
                text(r#""s\ud800:7\udc00""#),
                Some(r#"[1000,"t\udfff"]"#.to_string())
            ),
            (
                text(r#""𐀀""#),
                "s:2".into(),
                Some("[1000,\"\u{10000}\"]".to_string())
            ),
            (text(r#""￿""#), "s:1".into(), None),
        ]
    );
    let trace = telemetry.get_trace(&text(r#""t\udfff""#)).unwrap();
    assert_eq!(trace.spans[0].parent_id, Some(text(r#""s\ud800:7\udc00""#)));
}
