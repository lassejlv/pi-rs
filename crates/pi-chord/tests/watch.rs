use pi_chord::context::*;
use pi_chord::delta::Op;
use pi_chord::{JsonValue, NativeWatch, Watch, WatchEnd};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn number(i: usize) -> JsonValue {
    JsonValue::from(i)
}
fn advance(watch: &Watch, i: usize) {
    watch.advance(
        Arc::new(number(i)),
        vec![Op::Replace(number(i))],
        BACKGROUND_CONTEXT.clone(),
    );
}

#[tokio::test]
async fn acquisition_value_and_exact_frames_are_held_until_async_start() {
    let watch = Watch::new(number(0), || {}, |_| {});
    advance(&watch, 1);
    advance(&watch, 2);
    assert_eq!(*watch.value(), number(0));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start(move |frame| {
            send.send(frame).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    assert!(receive.try_recv().is_err());
    for i in 1..=2 {
        let frame = receive.recv().await.unwrap();
        assert_eq!(*frame.value, number(i));
        assert_eq!(&*frame.ops, &[Op::Replace(number(i))]);
    }
    assert_eq!(*watch.value(), number(2));
    watch.stop();
}

#[tokio::test]
async fn delivery_is_serialized_and_frame_cancellation_is_masked() {
    let key = create_context_key::<String>("frame");
    let (context, controller) = with_cancel(&with_context_value(
        &key,
        "value".into(),
        &BACKGROUND_CONTEXT,
    ));
    controller.abort();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let watch = Watch::new(number(0), || {}, |_| {});
    watch
        .start({
            let gate = gate.clone();
            move |frame| {
                send.send(frame.clone()).unwrap();
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        })
        .unwrap();
    watch.advance(Arc::new(number(1)), vec![], context);
    let first = receive.recv().await.unwrap();
    assert!(first.context.abort_signal().is_none());
    assert_eq!(&*first.context.value(&key).unwrap(), "value");
    advance(&watch, 2);
    tokio::task::yield_now().await;
    assert!(receive.try_recv().is_err());
    gate.add_permits(1);
    assert_eq!(*receive.recv().await.unwrap().value, number(2));
    watch.stop();
    gate.add_permits(1);
}

#[tokio::test]
async fn overflow_collapses_101_pending_frames_but_100_remain_exact() {
    for count in [100, 101] {
        let watch = Watch::new(number(0), || {}, |_| {});
        for i in 1..=count {
            advance(&watch, i);
        }
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        watch
            .start(move |frame| {
                send.send(frame).unwrap();
                async { Ok(()) }
            })
            .unwrap();
        if count == 100 {
            for i in 1..=count {
                assert_eq!(*receive.recv().await.unwrap().value, number(i));
            }
        } else {
            let frame = receive.recv().await.unwrap();
            assert_eq!(*frame.value, number(101));
            assert_eq!(&*frame.ops, &[Op::Replace(number(101))]);
        }
        watch.stop();
    }
}

#[tokio::test]
async fn overflow_uses_source_replacement_without_folding_in_flight_frame() {
    let watch = Watch::with_replacement(
        number(0),
        || {},
        |_| {},
        Some(Arc::new(|| Arc::new(number(999)))),
    );
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start({
            let gate = gate.clone();
            move |frame| {
                send.send(frame).unwrap();
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        })
        .unwrap();
    advance(&watch, 1);
    assert_eq!(*receive.recv().await.unwrap().value, number(1));
    for i in 2..=102 {
        advance(&watch, i);
    }
    gate.add_permits(1);
    let frame = receive.recv().await.unwrap();
    assert_eq!(*frame.value, number(999));
    assert_eq!(&*frame.ops, &[Op::Replace(number(999))]);
    watch.stop();
    gate.add_permits(1);
}

#[tokio::test]
async fn retirement_delivers_null_last_and_ends_only_after_its_callback() {
    let watch = Watch::new(number(0), || {}, |_| {});
    for i in 1..=100 {
        advance(&watch, i);
    }
    watch.retire(BACKGROUND_CONTEXT.clone());
    advance(&watch, 101);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start({
            let gate = gate.clone();
            move |frame| {
                send.send(frame).unwrap();
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        })
        .unwrap();
    let frame = receive.recv().await.unwrap();
    assert_eq!(*frame.value, JsonValue::Null);
    assert_eq!(&*frame.ops, &[Op::Replace(JsonValue::Null)]);
    assert!(watch.end_reason().is_none());
    gate.add_permits(1);
    assert_eq!(watch.closed().await, WatchEnd::Retired);
}

#[tokio::test]
async fn stopping_is_idempotent_does_not_await_callback_and_releases_source() {
    let source = Arc::new(());
    let weak_source = Arc::downgrade(&source);
    let detached = Arc::new(AtomicUsize::new(0));
    let watch = Watch::new(
        number(0),
        {
            let detached = detached.clone();
            move || {
                drop(source);
                detached.fetch_add(1, Ordering::SeqCst);
            }
        },
        |_| {},
    );
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (entered, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start({
            let gate = gate.clone();
            move |_| {
                entered.send(()).unwrap();
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        })
        .unwrap();
    advance(&watch, 1);
    receive.recv().await.unwrap();
    advance(&watch, 2);
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(watch.closed().await, WatchEnd::Stopped);
    assert_eq!(detached.load(Ordering::SeqCst), 1);
    assert!(weak_source.upgrade().is_none());
    assert_eq!(
        watch.start(|_| async { Ok(()) }).unwrap_err(),
        "Watch is already started"
    );
    gate.add_permits(1);
    let never_started = Watch::new(number(0), || {}, |_| {});
    never_started.stop();
    assert_eq!(
        never_started.start(|_| async { Ok(()) }).unwrap_err(),
        "Watch is stopped"
    );
}

#[tokio::test]
async fn listener_failure_reports_once_and_drops_pending_frames() {
    let reports = Arc::new(AtomicUsize::new(0));
    let watch = Watch::new(number(0), || {}, {
        let reports = reports.clone();
        move |error| {
            assert_eq!(error, "failure");
            reports.fetch_add(1, Ordering::SeqCst);
        }
    });
    let calls = Arc::new(AtomicUsize::new(0));
    watch
        .start({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("failure".into()) }
            }
        })
        .unwrap();
    advance(&watch, 1);
    advance(&watch, 2);
    assert_eq!(
        watch.closed().await,
        WatchEnd::ListenerError {
            error: "failure".into()
        }
    );
    assert_eq!(reports.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_and_source_end_discard_pending_retirement() {
    for already in [false, true] {
        let watch = Watch::new(number(0), || {}, |_| {});
        let controller = AbortController::new();
        if already {
            controller.abort();
        }
        watch.observe_cancellation(controller.signal()).unwrap();
        controller.abort();
        assert_eq!(watch.closed().await, WatchEnd::Cancelled);
    }
    let watch = Watch::new(number(0), || {}, |_| {});
    watch.retire(BACKGROUND_CONTEXT.clone());
    assert_eq!(watch.end(None), WatchEnd::Ended);
    assert_eq!(*watch.value(), number(0));
    let watch = Watch::new(number(0), || {}, |_| {});
    assert_eq!(
        watch.end(Some("storage".into())),
        WatchEnd::Failed {
            error: "storage".into()
        }
    );
}

#[tokio::test]
async fn native_watch_keeps_identity_and_collapses_backlog() {
    let initial = Arc::new(String::from("initial"));
    let watch = NativeWatch::new(initial.clone(), || {}, |_| {});
    assert!(Arc::ptr_eq(&watch.value(), &initial));
    let newest = Arc::new(String::from("newest"));
    for _ in 0..101 {
        watch.advance(newest.clone(), BACKGROUND_CONTEXT.clone());
    }
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start(move |value, _| {
            send.send(value).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    assert!(Arc::ptr_eq(&receive.recv().await.unwrap(), &newest));
    assert_eq!(watch.stop(), WatchEnd::Stopped);
}

#[tokio::test]
async fn cancellation_can_only_be_installed_once_on_a_live_watch() {
    let watch = Watch::new(number(0), || {}, |_| {});
    let controller = AbortController::new();
    watch.observe_cancellation(controller.signal()).unwrap();
    assert_eq!(
        watch.observe_cancellation(controller.signal()).unwrap_err(),
        "Watch cancellation is already installed"
    );
    watch.stop();
    assert!(watch.observe_cancellation(controller.signal()).is_ok());
}

#[tokio::test]
async fn delivery_idle_waits_for_the_listener_and_wakes_every_waiter_on_stop() {
    let watch = Watch::new(number(0), || {}, |_| {});
    advance(&watch, 1);
    // An unstarted watch has no active delivery loop.
    watch.delivery_idle().await;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Notify::new());
    let release = gate.clone();
    watch
        .start(move |_| {
            send.send(()).unwrap();
            let gate = gate.clone();
            async move {
                gate.notified().await;
                Ok(())
            }
        })
        .unwrap();
    receive.recv().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), watch.delivery_idle())
            .await
            .is_err()
    );
    let first = watch.clone();
    let second = watch.clone();
    let first = tokio::spawn(async move { first.delivery_idle().await });
    let second = tokio::spawn(async move { second.delivery_idle().await });
    watch.stop();
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        first.await.unwrap();
        second.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn watch_failures_retain_the_original_native_cause() {
    use pi_chord::WatchError;
    let original = Arc::new(String::from("original failure"));
    let observed = Arc::new(parking_lot::Mutex::new(None));
    let capture = observed.clone();
    let watch = Watch::new(
        number(0),
        || {},
        move |error| {
            *capture.lock() = Some(error);
        },
    );
    let cause = original.clone();
    watch
        .start_with_error(move |_| {
            let cause = cause.clone();
            async move { Err(WatchError::with_cause("original failure", cause)) }
        })
        .unwrap();
    advance(&watch, 1);
    let WatchEnd::ListenerError { error } = watch.closed().await else {
        panic!("listener failure missing")
    };
    assert!(Arc::ptr_eq(
        error.cause::<Arc<String>>().unwrap(),
        &original
    ));
    assert!(Arc::ptr_eq(
        observed
            .lock()
            .as_ref()
            .unwrap()
            .cause::<Arc<String>>()
            .unwrap(),
        &original
    ));
    let source = Watch::new(number(0), || {}, |_| {});
    let WatchEnd::Failed { error } = source.end_with_error(Some(WatchError::with_cause(
        "source failure",
        original.clone(),
    ))) else {
        panic!("source failure missing")
    };
    assert!(Arc::ptr_eq(
        error.cause::<Arc<String>>().unwrap(),
        &original
    ));
}

// Ports of the upstream `test/watch.test.ts` cases.

use parking_lot::Mutex;
use pi_chord::watch::WatchFrame;

async fn flush() {
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
}
fn number_of(frame: &WatchFrame<JsonValue>) -> usize {
    frame.value.as_u64().unwrap() as usize
}
fn root(value: JsonValue) -> Vec<Op> {
    vec![Op::Replace(value)]
}
type Frames = Arc<Mutex<Vec<(JsonValue, Vec<Op>)>>>;

/// A watch whose detach count is recorded; `replace` is the source's overflow value.
fn recorded(replace: Option<JsonValue>) -> (Watch, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let detached = Arc::new(AtomicUsize::new(0));
    let reported = Arc::new(Mutex::new(Vec::new()));
    let watch = Watch::with_replacement(
        number(0),
        {
            let detached = detached.clone();
            move || {
                detached.fetch_add(1, Ordering::SeqCst);
            }
        },
        {
            let reported = reported.clone();
            move |error| reported.lock().push(error.to_string())
        },
        replace.map(|value| {
            Arc::new(move || Arc::new(value.clone()))
                as Arc<dyn Fn() -> Arc<JsonValue> + Send + Sync>
        }),
    );
    (watch, detached, reported)
}
/// A started watch collecting its frames.
fn collecting() -> (Watch, Arc<AtomicUsize>, Frames) {
    let (watch, detached, _) = recorded(None);
    let frames = Frames::default();
    watch
        .start({
            let frames = frames.clone();
            move |frame| {
                frames
                    .lock()
                    .push((frame.value.as_ref().clone(), frame.ops.as_ref().clone()));
                async { Ok(()) }
            }
        })
        .unwrap();
    (watch, detached, frames)
}

#[tokio::test]
async fn watch_delivery_never_invokes_the_listener_inline() {
    let (watch, _, frames) = collecting();
    watch.advance(
        Arc::new(number(1)),
        root(number(1)),
        BACKGROUND_CONTEXT.clone(),
    );
    assert!(frames.lock().is_empty());
    flush().await;
    assert_eq!(*frames.lock(), [(number(1), root(number(1)))]);
    watch.stop();
}

#[tokio::test]
async fn watch_delivery_awaits_each_callback_and_buffers_frames_advanced_while_one_is_in_flight() {
    let watch = Watch::new(number(0), || {}, |_| {});
    let gate = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    watch
        .start({
            let (gate, calls) = (gate.clone(), calls.clone());
            move |frame| {
                let (gate, calls) = (gate.clone(), calls.clone());
                async move {
                    let value = number_of(&frame);
                    calls.lock().push(format!("start {value}"));
                    if value == 1 {
                        gate.notified().await;
                    }
                    calls.lock().push(format!("end {value}"));
                    Ok(())
                }
            }
        })
        .unwrap();
    advance(&watch, 1);
    flush().await;
    advance(&watch, 2);
    advance(&watch, 3);
    flush().await;
    assert_eq!(*calls.lock(), ["start 1"]);
    gate.notify_one();
    flush().await;
    assert_eq!(
        *calls.lock(),
        ["start 1", "end 1", "start 2", "end 2", "start 3", "end 3"]
    );
    watch.stop();
}

#[tokio::test]
async fn watch_overflow_delivers_the_value_of_replace_on_overflow() {
    let replacement = JsonValue::String("replacement".into());
    let (watch, _, _) = recorded(Some(replacement.clone()));
    for i in 1..=101 {
        watch.advance(Arc::new(number(i)), vec![], BACKGROUND_CONTEXT.clone());
    }
    let frames = Frames::default();
    watch
        .start({
            let frames = frames.clone();
            move |frame| {
                frames
                    .lock()
                    .push((frame.value.as_ref().clone(), frame.ops.as_ref().clone()));
                async { Ok(()) }
            }
        })
        .unwrap();
    flush().await;
    assert_eq!(
        *frames.lock(),
        [(replacement.clone(), root(replacement.clone()))]
    );
    watch.stop();
}

#[tokio::test]
async fn watch_overflow_never_folds_the_in_flight_frame_into_an_overflow() {
    let watch = Watch::new(number(0), || {}, |_| {});
    let gate = Arc::new(tokio::sync::Notify::new());
    let values = Arc::new(Mutex::new(Vec::<usize>::new()));
    watch
        .start({
            let (gate, values) = (gate.clone(), values.clone());
            move |frame| {
                let (gate, values) = (gate.clone(), values.clone());
                async move {
                    let value = number_of(&frame);
                    values.lock().push(value);
                    if value == 1 {
                        gate.notified().await;
                    }
                    Ok(())
                }
            }
        })
        .unwrap();
    advance(&watch, 1);
    flush().await;
    for i in 2..=102 {
        advance(&watch, i);
    }
    gate.notify_one();
    flush().await;
    assert_eq!(*values.lock(), [1, 102]);
    watch.stop();
}

#[tokio::test]
async fn watch_retirement_delivers_r_null_as_the_last_frame_and_then_ends_retired() {
    let (watch, detached, frames) = collecting();
    advance(&watch, 2);
    watch.retire(BACKGROUND_CONTEXT.clone());
    advance(&watch, 3);
    assert_eq!(watch.closed().await, WatchEnd::Retired);
    assert_eq!(
        *frames.lock(),
        [
            (number(2), root(number(2))),
            (JsonValue::Null, root(JsonValue::Null)),
        ]
    );
    assert_eq!(*watch.value(), JsonValue::Null);
    assert_eq!(detached.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn watch_retirement_ends_retired_only_on_its_sources_retirement_never_on_a_null_value() {
    let (watch, _, frames) = collecting();
    watch.advance(
        Arc::new(JsonValue::Null),
        root(JsonValue::Null),
        BACKGROUND_CONTEXT.clone(),
    );
    advance(&watch, 2);
    flush().await;
    assert_eq!(
        *frames.lock(),
        [
            (JsonValue::Null, root(JsonValue::Null)),
            (number(2), root(number(2))),
        ]
    );
    assert!(watch.end_reason().is_none());
    assert_eq!(watch.stop(), WatchEnd::Stopped);
}

#[tokio::test]
async fn watch_ends_stops_idempotently_and_rejects_a_repeated_or_late_start() {
    let (watch, detached, _) = recorded(None);
    watch.start(|_| async { Ok(()) }).unwrap();
    assert_eq!(
        watch.start(|_| async { Ok(()) }).unwrap_err(),
        "Watch is already started"
    );
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    assert_eq!(detached.load(Ordering::SeqCst), 1);

    let late = Watch::new(number(0), || {}, |_| {});
    late.stop();
    assert_eq!(
        late.start(|_| async { Ok(()) }).unwrap_err(),
        "Watch is stopped"
    );
}

#[tokio::test]
async fn watch_ends_ends_ended_or_failed_from_its_source_dropping_pending_frames() {
    let (watch, _, frames) = collecting();
    advance(&watch, 1);
    watch.end(None);
    flush().await;
    assert!(frames.lock().is_empty());
    assert_eq!(watch.closed().await, WatchEnd::Ended);

    let failed = Watch::new(number(0), || {}, |_| {});
    failed.end(Some("source failed".into()));
    failed.end(None);
    assert_eq!(
        failed.closed().await,
        WatchEnd::Failed {
            error: "source failed".into()
        }
    );
}

#[tokio::test]
async fn watch_ends_stops_future_delivery_without_aborting_or_awaiting_an_in_flight_callback() {
    let watch = Watch::new(number(0), || {}, |_| {});
    let gate = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    watch
        .start({
            let (gate, calls) = (gate.clone(), calls.clone());
            move |frame| {
                let (gate, calls) = (gate.clone(), calls.clone());
                async move {
                    let value = number_of(&frame);
                    calls.lock().push(format!("start {value}"));
                    gate.notified().await;
                    calls.lock().push(format!("end {value}"));
                    Ok(())
                }
            }
        })
        .unwrap();
    advance(&watch, 1);
    flush().await;
    advance(&watch, 2);
    assert_eq!(watch.stop(), WatchEnd::Stopped);
    gate.notify_one();
    flush().await;
    assert_eq!(*calls.lock(), ["start 1", "end 1"]);
}

#[tokio::test]
async fn watch_ends_ends_cancelled_when_its_acquisition_signal_aborts_also_when_already_aborted() {
    let controller = AbortController::new();
    let (watch, detached, _) = recorded(None);
    watch.observe_cancellation(controller.signal()).unwrap();
    assert_eq!(
        watch.observe_cancellation(controller.signal()).unwrap_err(),
        "Watch cancellation is already installed"
    );
    controller.abort();
    assert_eq!(watch.closed().await, WatchEnd::Cancelled);
    assert_eq!(detached.load(Ordering::SeqCst), 1);

    let aborted = Watch::new(number(0), || {}, |_| {});
    let already = AbortController::new();
    already.abort();
    aborted.observe_cancellation(already.signal()).unwrap();
    assert_eq!(aborted.closed().await, WatchEnd::Cancelled);

    let cancelled = Watch::new(number(0), || {}, |_| {});
    cancelled.cancel();
    assert_eq!(cancelled.closed().await, WatchEnd::Cancelled);
}

#[tokio::test]
async fn watch_ends_ends_listener_error_on_a_throw_reports_it_and_delivers_nothing_more() {
    let (watch, detached, reported) = recorded(None);
    let values = Arc::new(Mutex::new(Vec::<usize>::new()));
    watch
        .start({
            let values = values.clone();
            move |frame| {
                values.lock().push(number_of(&frame));
                async { Err("listener failed".to_string()) }
            }
        })
        .unwrap();
    advance(&watch, 1);
    advance(&watch, 2);
    assert_eq!(
        watch.closed().await,
        WatchEnd::ListenerError {
            error: "listener failed".into()
        }
    );
    assert_eq!(*values.lock(), [1]);
    assert_eq!(*reported.lock(), ["listener failed"]);
    assert_eq!(detached.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn watch_references_retains_neither_its_source_its_listener_nor_its_signal_once_ended_while_still_referenced()
 {
    type End = fn(&Watch) -> WatchEnd;
    struct Acquired {
        watch: Watch,
        source: std::sync::Weak<()>,
        listener: std::sync::Weak<()>,
        signal: WeakAbortSignal,
    }
    async fn acquire(end: Option<End>) -> Acquired {
        let source = Arc::new(());
        let listener_token = Arc::new(());
        let (source_weak, listener_weak) =
            (Arc::downgrade(&source), Arc::downgrade(&listener_token));
        let replace_source = source.clone();
        let report_source = source.clone();
        let watch = Watch::with_replacement(
            number(0),
            move || drop(source),
            move |_| {
                let _ = &report_source;
            },
            Some(Arc::new(move || {
                let _ = &replace_source;
                Arc::new(number(1))
            })),
        );
        let controller = AbortController::new();
        let signal = controller.signal();
        let signal_weak = signal.downgrade();
        watch.observe_cancellation(signal).unwrap();
        watch
            .start(move |_| {
                let _ = &listener_token;
                async { Ok(()) }
            })
            .unwrap();
        advance(&watch, 1);
        flush().await;
        if let Some(end) = end {
            end(&watch);
        }
        flush().await;
        Acquired {
            watch,
            source: source_weak,
            listener: listener_weak,
            signal: signal_weak,
        }
    }
    // Control: an open watch keeps its source, so the releases below are real.
    let open = acquire(None).await;
    assert!(open.source.upgrade().is_some());
    assert!(open.listener.upgrade().is_some());
    assert!(open.signal.upgrade().is_some());
    open.watch.stop();

    let ends: [End; 4] = [
        |watch| watch.stop(),
        |watch| watch.cancel(),
        |watch| watch.end(None),
        |watch| {
            watch.retire(BACKGROUND_CONTEXT.clone());
            WatchEnd::Retired
        },
    ];
    for end in ends {
        let ended = acquire(Some(end)).await;
        flush().await;
        assert!(ended.watch.end_reason().is_some());
        assert!(ended.source.upgrade().is_none());
        assert!(ended.listener.upgrade().is_none());
        assert!(ended.signal.upgrade().is_none());
    }
}

#[tokio::test]
async fn native_watch_delivers_values_by_identity_in_order_with_a_context_never_inline() {
    let key = create_context_key::<String>("frame");
    let (first, second) = (Arc::new(String::from("1")), Arc::new(String::from("2")));
    let watch = NativeWatch::new(Arc::new(String::from("0")), || {}, |_| {});
    let delivered = Arc::new(Mutex::new(Vec::<(Arc<String>, Option<String>)>::new()));
    watch
        .start({
            let (delivered, key) = (delivered.clone(), key.clone());
            move |value, context| {
                delivered
                    .lock()
                    .push((value, context.value(&key).map(|v| v.to_string())));
                async { Ok(()) }
            }
        })
        .unwrap();
    watch.advance(
        first.clone(),
        with_context_value(&key, "first".into(), &BACKGROUND_CONTEXT),
    );
    watch.advance(
        second.clone(),
        with_context_value(&key, "second".into(), &BACKGROUND_CONTEXT),
    );
    assert!(delivered.lock().is_empty());
    flush().await;
    let delivered = delivered.lock();
    assert_eq!(delivered.len(), 2);
    assert!(Arc::ptr_eq(&delivered[0].0, &first));
    assert!(Arc::ptr_eq(&delivered[1].0, &second));
    assert_eq!(
        delivered.iter().map(|d| d.1.clone()).collect::<Vec<_>>(),
        [Some("first".to_string()), Some("second".to_string())]
    );
    assert!(Arc::ptr_eq(&watch.value(), &second));
    watch.stop();
}

#[tokio::test]
async fn native_watch_ends_with_every_reason_of_its_lifecycle() {
    let create = || NativeWatch::new(Arc::new(0), || {}, |_| {});
    assert_eq!(create().stop(), WatchEnd::Stopped);

    let cancelled = create();
    let aborted = AbortController::new();
    aborted.abort();
    cancelled.observe_cancellation(aborted.signal()).unwrap();
    assert_eq!(cancelled.closed().await, WatchEnd::Cancelled);

    let ended = create();
    ended.end(None);
    assert_eq!(ended.closed().await, WatchEnd::Ended);

    let failed = create();
    failed.end(Some("source failed".into()));
    assert_eq!(
        failed.closed().await,
        WatchEnd::Failed {
            error: "source failed".into()
        }
    );

    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let throwing = NativeWatch::new(Arc::new(0), || {}, {
        let reported = reported.clone();
        move |error| reported.lock().push(error.to_string())
    });
    throwing
        .start(|_, _| async { Err("listener failed".to_string()) })
        .unwrap();
    throwing.advance(Arc::new(1), BACKGROUND_CONTEXT.clone());
    assert_eq!(
        throwing.closed().await,
        WatchEnd::ListenerError {
            error: "listener failed".into()
        }
    );
    assert_eq!(*reported.lock(), ["listener failed"]);
}

#[tokio::test]
async fn native_watch_retains_neither_its_source_nor_its_listener_once_stopped_while_still_referenced()
 {
    async fn acquire(stop: bool) -> (NativeWatch<i32>, std::sync::Weak<()>, std::sync::Weak<()>) {
        let source = Arc::new(());
        let listener_token = Arc::new(());
        let (source_weak, listener_weak) =
            (Arc::downgrade(&source), Arc::downgrade(&listener_token));
        let report_source = source.clone();
        let watch = NativeWatch::new(
            Arc::new(0),
            move || drop(source),
            move |_| {
                let _ = &report_source;
            },
        );
        watch
            .start(move |_, _| {
                let _ = &listener_token;
                async { Ok(()) }
            })
            .unwrap();
        watch.advance(Arc::new(1), BACKGROUND_CONTEXT.clone());
        flush().await;
        if stop {
            watch.stop();
        }
        flush().await;
        (watch, source_weak, listener_weak)
    }
    let (open, source, _) = acquire(false).await;
    assert!(source.upgrade().is_some());
    open.stop();

    let (stopped, source, listener) = acquire(true).await;
    assert!(source.upgrade().is_none());
    assert!(listener.upgrade().is_none());
    assert_eq!(*stopped.value(), 1);
}
