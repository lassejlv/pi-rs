use pi_chord::context::*;
use pi_telemetry::{InMemoryTelemetryContext, SpanLink, SpanOptions, TelemetryContextExt};
use std::sync::Arc;

#[test]
fn roots_and_typed_value_layers_are_distinct_and_immutable() {
    let first = create_context_key::<String>("first");
    let same_name = create_context_key::<String>("first");
    let second = create_context_key::<u64>("second");
    let one = with_context_value(&first, "one".into(), &BACKGROUND_CONTEXT);
    let two = with_context_value(&second, 2, &one);
    let replaced = with_context_value(&first, "updated".into(), &two);
    assert!(BACKGROUND_CONTEXT.value(&first).is_none());
    assert!(one.value(&same_name).is_none());
    assert!(one.value(&second).is_none());
    assert_eq!(&*two.value(&first).unwrap(), "one");
    assert_eq!(*two.value(&second).unwrap(), 2);
    assert_eq!(&*replaced.value(&first).unwrap(), "updated");
    assert_eq!(
        replaced.to_string(),
        "[Context BACKGROUND_CONTEXT].WithValue(first).WithValue(second).WithValue(first)"
    );
    assert_eq!(TODO_CONTEXT.to_string(), "[Context TODO_CONTEXT]");
    assert!(TODO_CONTEXT.abort_signal().is_none());
    assert!(!Arc::ptr_eq(
        &one.value(&first).unwrap(),
        &replaced.value(&first).unwrap()
    ));
}

#[tokio::test]
async fn cancellation_is_inherited_and_child_abort_does_not_affect_siblings() {
    let (parent, parent_controller) = with_cancel(&BACKGROUND_CONTEXT);
    let (child, child_controller) = with_cancel(&parent);
    let (sibling, _) = with_cancel(&parent);
    child_controller.abort_with_reason(AbortReason::Error(AbortError::new("child")));
    assert_eq!(child.check().unwrap_err().message, "child");
    assert!(parent.check().is_ok());
    assert!(sibling.check().is_ok());
    parent_controller.abort_with_reason(AbortReason::Error(AbortError::new("parent")));
    sibling.abort_signal().unwrap().cancelled().await;
    assert_eq!(sibling.check().unwrap_err().message, "parent");
    assert_eq!(child.check().unwrap_err().message, "child");
    child_controller.abort();
    assert_eq!(child.check().unwrap_err().message, "child");
}

#[tokio::test]
async fn composite_signal_keeps_the_first_abort_in_time() {
    let first = AbortController::new();
    let second = AbortController::new();
    let combined = AbortSignal::any([first.signal(), second.signal()]);
    second.abort_with_reason(AbortReason::Error(AbortError::new("second")));
    first.abort();
    combined.cancelled().await;
    assert_eq!(combined.check().unwrap_err().message, "second");
}

#[test]
fn cleanup_masks_cancellation_without_losing_values_or_telemetry() {
    let (parent, controller) = with_cancel(&BACKGROUND_CONTEXT);
    let key = create_context_key::<String>("value");
    let parent = with_context_value(&key, "preserved".into(), &parent);
    let cleanup = without_abort_signal(&parent);
    controller.abort();
    assert!(parent.check().is_err());
    assert!(cleanup.abort_signal().is_none());
    assert_eq!(&*cleanup.value(&key).unwrap(), "preserved");
    assert!(Arc::ptr_eq(cleanup.telemetry(), parent.telemetry()));
}

#[tokio::test]
async fn cancelled_waiter_does_not_cancel_underlying_work() {
    let (context, controller) = with_cancel(&BACKGROUND_CONTEXT);
    let (release, gate) = tokio::sync::oneshot::channel();
    let (finished, finish) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        await_with_context(
            async move {
                gate.await.unwrap();
                finished.send(42).unwrap();
            },
            &context,
        )
        .await
    });
    tokio::task::yield_now().await;
    controller.abort_with_reason(AbortReason::Error(AbortError::new("cancelled")));
    assert_eq!(waiter.await.unwrap().unwrap_err().to_string(), "cancelled");
    release.send(()).unwrap();
    assert_eq!(finish.await.unwrap(), 42);
    assert_eq!(
        await_with_context(async { 42 }, &BACKGROUND_CONTEXT)
            .await
            .unwrap(),
        42
    );
}

#[tokio::test]
async fn non_error_abort_reason_is_preserved_but_waiter_gets_abort_error() {
    let (context, controller) = with_cancel(&BACKGROUND_CONTEXT);
    let value = pi_chord::json::parse(r#""raw reason""#).unwrap();
    controller.abort_with_reason(AbortReason::Value(value.clone()));
    assert_eq!(
        context.abort_signal().unwrap().reason(),
        Some(AbortReason::Value(value))
    );
    assert_eq!(
        context.check().unwrap_err().message,
        "The operation was aborted"
    );
}

#[tokio::test]
async fn trace_in_preserves_values_and_creates_explicit_parent_chain() {
    let telemetry = Arc::new(InMemoryTelemetryContext::new());
    let key = create_context_key::<u64>("owner");
    let base = with_context_value(&key, 42, &BACKGROUND_CONTEXT);
    telemetry
        .start_span(SpanOptions::new("caller"), |span| async move {
            let context = with_telemetry(span, &base);
            let result = trace_in(
                "outer",
                |child| async move {
                    assert_eq!(*child.value(&key).unwrap(), 42);
                    trace_in("inner", |_| async { 123 }, &child).await
                },
                &context,
            )
            .await;
            assert_eq!(result, 123);
        })
        .await;
    let spans = telemetry.get_spans();
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[1].parent_id, Some(spans[0].id));
    assert_eq!(spans[2].parent_id, Some(spans[1].id));
}

fn same_telemetry(a: &pi_telemetry::ContextRef, b: &pi_telemetry::ContextRef) -> bool {
    std::ptr::eq(Arc::as_ptr(a) as *const (), Arc::as_ptr(b) as *const ())
}

// context.test.ts > Context > provides distinct empty root contexts
#[test]
fn provides_distinct_empty_root_contexts() {
    let key = create_context_key::<String>("value");
    let noop: pi_telemetry::ContextRef = pi_telemetry::noop_telemetry_context();
    assert_ne!(TODO_CONTEXT.to_string(), BACKGROUND_CONTEXT.to_string());
    assert!(TODO_CONTEXT.abort_signal().is_none());
    assert!(same_telemetry(TODO_CONTEXT.telemetry(), &noop));
    assert!(same_telemetry(BACKGROUND_CONTEXT.telemetry(), &noop));
    assert!(TODO_CONTEXT.value(&key).is_none());
    assert_eq!(
        BACKGROUND_CONTEXT.to_string(),
        "[Context BACKGROUND_CONTEXT]"
    );
}

// context.test.ts > Context > derives telemetry independently from values and cancellation
#[tokio::test]
async fn derives_telemetry_independently_from_values_and_cancellation() {
    let telemetry = Arc::new(InMemoryTelemetryContext::new());
    let other = Arc::new(InMemoryTelemetryContext::new());
    let noop: pi_telemetry::ContextRef = pi_telemetry::noop_telemetry_context();
    let other_for_span = other.clone();
    telemetry
        .start_span(SpanOptions::new("caller"), |parent| async move {
            other_for_span
                .start_span(SpanOptions::new("other"), |other_span| async move {
                    let key = create_context_key::<String>("principal");
                    let (with_signal, controller) = with_cancel(&BACKGROUND_CONTEXT);
                    let base = with_context_value(&key, "owner".into(), &with_signal);
                    let parent_ref: pi_telemetry::ContextRef = parent.clone();
                    let context = with_telemetry(parent_ref.clone(), &base);
                    let replaced = with_telemetry(other_span.clone(), &context);
                    assert!(same_telemetry(base.telemetry(), &noop));
                    assert!(same_telemetry(context.telemetry(), &parent_ref));
                    let other_ref: pi_telemetry::ContextRef = other_span.clone();
                    assert!(same_telemetry(replaced.telemetry(), &other_ref));
                    assert_eq!(&*replaced.value(&key).unwrap(), "owner");
                    assert!(replaced.abort_signal().is_some());
                    assert!(!replaced.abort_signal().unwrap().aborted());
                    assert!(same_telemetry(
                        without_abort_signal(&context).telemetry(),
                        &parent_ref
                    ));
                    trace_in(
                        "outer",
                        |child| async move {
                            assert_eq!(&*child.value(&key).unwrap(), "owner");
                            assert!(child.abort_signal().is_some());
                            child
                                .telemetry()
                                .record_event("started", Default::default());
                            let outer_telemetry = child.telemetry().clone();
                            trace_in(
                                "inner",
                                |nested| async move {
                                    nested
                                        .telemetry()
                                        .record_link(SpanLink::new(outer_telemetry));
                                    42
                                },
                                &child,
                            )
                            .await;
                        },
                        &context,
                    )
                    .await;
                    assert!(same_telemetry(context.telemetry(), &parent_ref));
                    controller.abort();
                    assert!(context.check().is_err());
                })
                .await;
        })
        .await;
    let spans = telemetry.get_spans();
    let (caller, outer, inner) = (&spans[0], &spans[1], &spans[2]);
    assert_eq!(outer.parent_id, Some(caller.id));
    assert_eq!(inner.parent_id, Some(outer.id));
    assert_eq!(outer.events[0].name, "started");
    assert_eq!(inner.links[0].context, outer.context);
    assert_eq!(other.get_spans().len(), 1);
}

// context.test.ts > Context > isolates concurrent traceIn branches and preserves callback results and errors
#[tokio::test]
async fn isolates_concurrent_trace_in_branches_and_preserves_callback_results_and_errors() {
    let telemetry = Arc::new(InMemoryTelemetryContext::new());
    telemetry
        .start_span(SpanOptions::new("caller"), |parent| async move {
            let parent_ref: pi_telemetry::ContextRef = parent.clone();
            let context = with_telemetry(parent_ref.clone(), &BACKGROUND_CONTEXT);
            let (release, gate) = tokio::sync::oneshot::channel::<()>();
            let first = trace_in(
                "first",
                |child| async move {
                    gate.await.unwrap();
                    trace_in("first.child", |_| async { "first" }, &child).await
                },
                &context,
            );
            let second = trace_in(
                "second",
                |child| async move {
                    let result = trace_in("second.child", |_| async { "second" }, &child).await;
                    release.send(()).unwrap();
                    result
                },
                &context,
            );
            assert_eq!(tokio::join!(first, second), ("first", "second"));
            let failure = value_of("same value");
            let thrown: Result<(), _> =
                trace_in("throw", |_| async { Err(failure.clone()) }, &context).await;
            assert_eq!(thrown, Err(failure.clone()));
            let rejected: Result<(), _> =
                trace_in("reject", |_| async { Err(failure.clone()) }, &context).await;
            assert_eq!(rejected, Err(failure));
            let result = trace_in(
                "noop",
                |child| async move {
                    child.telemetry().record_link(SpanLink::new(parent_ref));
                    42
                },
                &BACKGROUND_CONTEXT,
            )
            .await;
            assert_eq!(result, 42);
        })
        .await;
    let spans = telemetry.get_spans();
    for name in ["first", "second"] {
        let owner = spans.iter().find(|s| s.name == name).unwrap();
        let child = spans
            .iter()
            .find(|s| s.name == format!("{name}.child"))
            .unwrap();
        assert_eq!(child.parent_id, Some(owner.id));
    }
}

fn value_of(text: &str) -> pi_chord::JsonValue {
    pi_chord::json::parse(&format!("{text:?}")).unwrap()
}
