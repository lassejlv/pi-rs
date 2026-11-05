use super::*;
use pi_telemetry::{InMemoryTelemetryContext, SpanOptions, SpanOutcome, TelemetryContext};

#[test]
fn reports_record_only_the_source_whitelist_and_type_metric() {
    let telemetry = InMemoryTelemetryContext::new();
    let span = telemetry.begin_span(SpanOptions::new("caller"));
    let context = pi_chord::context::with_telemetry(span.clone(), &Context::default());
    let report = safe_report(None, telemetry.clone());
    report(
        Error::message("private-error-payload"),
        TaskReport {
            kind: "host_error",
            task: Some(super::super::scheduler::ReportTask {
                id: serde_json::from_str("1").unwrap(),
                kind: "private-task-kind".into(),
                conversation_id: serde_json::from_str("2").unwrap(),
            }),
            reason: None,
            callback: Some("models"),
            extension: None,
            conversation: None,
        },
        Some(context),
    );
    span.end(SpanOutcome::Ok);
    let spans = telemetry.get_spans();
    assert_eq!(spans[0].events[0].name, "pi.durable.report");
    assert_eq!(
        spans[0].events[0].attributes,
        pi_telemetry::attributes! {"type"=>"host_error","task.id"=>1u64}
    );
    let metrics = telemetry.get_metrics();
    assert_eq!(metrics.len(), 1);
    assert_eq!(
        metrics[0].attributes,
        pi_telemetry::attributes! {"type"=>"host_error"}
    );
    assert_eq!(metrics[0].value, 1.0);
    assert!(!serde_json::to_string(&spans).unwrap().contains("private"));
}
