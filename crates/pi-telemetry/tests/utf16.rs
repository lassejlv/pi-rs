use pi_telemetry::*;
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Unicode {
    attributes_json: String,
    event_attributes_json: String,
    status_json: String,
    diagnostic_input: String,
}
#[derive(Deserialize)]
struct Fixture {
    unicode: Unicode,
}
fn fixture() -> Unicode {
    serde_json::from_str::<Fixture>(include_str!("fixtures/utf16-reference.json"))
        .unwrap()
        .unicode
}
#[test]
fn attributes_events_and_statuses_preserve_source_utf16_json() {
    let case = fixture();
    let context = InMemoryTelemetryContext::new();
    let attributes: SpanAttributes = serde_json::from_str(&case.attributes_json).unwrap();
    let span = context.begin_span(SpanOptions::new("test.unicode").attributes(attributes));
    span.add_event(
        "test.event",
        serde_json::from_str(&case.event_attributes_json).unwrap(),
    );
    span.set_status(serde_json::from_str(&case.status_json).unwrap());
    span.end(SpanOutcome::Ok);
    let spans = context.get_spans();
    let span = &spans[0];
    assert_eq!(
        serde_json::to_string(&span.attributes).unwrap(),
        case.attributes_json
    );
    assert_eq!(
        serde_json::to_string(&span.events[0].attributes).unwrap(),
        case.event_attributes_json
    );
    assert_eq!(
        serde_json::to_string(&span.status).unwrap(),
        case.status_json
    );
}
#[test]
fn bounded_source_diagnostics_can_end_in_a_surrogate_half() {
    let case = fixture();
    let status: SpanStatus = serde_json::from_str(&case.status_json).unwrap();
    assert_eq!(serde_json::to_string(&status).unwrap(), case.status_json);
    assert_eq!(
        case.diagnostic_input.encode_utf16().take(500).last(),
        Some(0xd83d)
    );
    let bounded = SpanError::new("task_faulted", case.diagnostic_input)
        .message
        .prefix(500);
    assert_eq!(bounded.units().len(), 500);
    assert_eq!(bounded.as_str(), None);
    assert_eq!(
        serde_json::to_string(&SpanStatus::error(SpanError::new("task_faulted", bounded))).unwrap(),
        case.status_json,
    );
}
