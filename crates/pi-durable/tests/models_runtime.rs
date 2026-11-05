use pi_chord::{AbortController, context::with_abort_signal};
use pi_durable::{harness::models::*, *};
use std::sync::Arc;

#[tokio::test]
async fn request_scope_forwards_context_identity_and_cancellation_without_a_new_span() {
    let controller = AbortController::new();
    let telemetry = pi_telemetry::InMemoryTelemetryContext::new();
    let context = pi_chord::context::with_telemetry(
        telemetry.clone(),
        &with_abort_signal(controller.signal(), &Context::default()),
    );
    let mut options = pi_ai::StreamOptions::default();
    let scope = RequestScope::new(&mut options, controller.signal(), &context);
    assert!(Arc::ptr_eq(
        options.telemetry.as_ref().unwrap(),
        context.telemetry()
    ));
    assert!(telemetry.get_spans().is_empty());
    let token = options.signal.as_ref().unwrap().clone();
    controller.abort();
    tokio::time::timeout(std::time::Duration::from_secs(1), token.cancelled())
        .await
        .unwrap();
    drop(scope);
    let another = AbortController::new();
    let scope = RequestScope::new(&mut options, another.signal(), &Context::default());
    let token = options.signal.as_ref().unwrap().clone();
    assert!(!token.is_cancelled());
    drop(scope);
    assert!(token.is_cancelled());
    assert!(!another.signal().aborted());
}

#[tokio::test]
async fn pre_aborted_requests_enter_provider_with_a_cancelled_token() {
    let controller = AbortController::new();
    controller.abort();
    let mut options = pi_ai::StreamOptions::default();
    let _scope = RequestScope::new(&mut options, controller.signal(), &Context::default());
    assert!(options.signal.unwrap().is_cancelled());
}

#[test]
fn stored_options_preserve_known_fields_and_extra_provider_fields_at_the_sdk_boundary() {
    assert_eq!(provider_token_limit(8192.0).unwrap(), 8192);
    for unsupported in [-1.0, 1.5, f64::INFINITY, f64::NAN, u64::MAX as f64] {
        assert!(provider_token_limit(unsupported).is_err());
    }
    let source = r#"{"transport":"sse","timeoutMs":123,"maxRetries":2,"maxRetryDelayMs":900,"headers":{"custom":"value"},"metadata":{"key":true},"cacheRetention":"short","deferred":true,"customOption":{"value":"x"}}"#;
    let value = JsonObject::try_from_value(pi_chord::json::parse(source).unwrap()).unwrap();
    let options = provider_options(&value).unwrap();
    assert_eq!(options.timeout_ms, Some(123));
    assert_eq!(options.max_retries, Some(2));
    assert_eq!(options.max_retry_delay_ms, Some(900));
    assert_eq!(options.headers.unwrap()["custom"].as_deref(), Some("value"));
    assert!(options.metadata.unwrap()["key"].as_bool().unwrap());
    assert_eq!(options.extra["customOption"]["value"], "x");
    assert!(options.deferred.is_some());
    let value = JsonObject::try_from_value(
        pi_chord::json::parse(r#"{"metadata":{"key":"\ud800"}}"#).unwrap(),
    )
    .unwrap();
    assert!(provider_options(&value).is_err());
    assert_eq!(
        value.value()["metadata"]["key"]
            .as_string()
            .unwrap()
            .units(),
        &[0xd800]
    );
}
