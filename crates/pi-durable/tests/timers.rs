use parking_lot::Mutex;
use pi_durable::{timers::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn throwing_host_timers_preserve_the_original_native_error() {
    let error = Error::message("original timer error");
    let thrown = error.clone();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = reports.clone();
    let timers = Timers::new(
        Some(Arc::new(move |_, _, _| {
            std::panic::panic_any(thrown.clone())
        })),
        Arc::new(move |error| seen.lock().push(error)),
    );
    let (send, receive) = tokio::sync::oneshot::channel();
    let _timer = timers.foreground(
        Box::new(move || {
            let _ = send.send(());
        }),
        0.0,
    );
    receive.await.unwrap();
    assert!(reports.lock()[0].same_error(&error));
    assert!(
        timers
            .background(Box::new(|| panic!("declined timer must not run")), 100.0)
            .is_none()
    );
    let reports = reports.lock();
    assert_eq!(reports.len(), 2);
    assert!(reports[1].same_error(&error));
}

#[tokio::test]
async fn foreground_decline_and_panic_fall_back_and_report_once() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let capture = reports.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = calls.clone();
    let timers = Timers::new(
        Some(Arc::new(move |_, _, background| {
            assert!(!background);
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                None
            } else {
                panic!("host unavailable")
            }
        })),
        Arc::new(move |e| capture.lock().push(e.to_string())),
    );
    for _ in 0..2 {
        let (send, receive) = tokio::sync::oneshot::channel();
        let _timer = timers.foreground(
            Box::new(move || {
                let _ = send.send(());
            }),
            0.0,
        );
        receive.await.unwrap();
    }
    assert_eq!(reports.lock().len(), 1);
    assert!(reports.lock()[0].contains("declined a timer"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn background_decline_is_silent_and_panics_report_each_time() {
    let reports = Arc::new(AtomicUsize::new(0));
    let capture = reports.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = calls.clone();
    let timers = Timers::new(
        Some(Arc::new(move |_, _, background| {
            assert!(background);
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                None
            } else {
                panic!("host unavailable")
            }
        })),
        Arc::new(move |_| {
            capture.fetch_add(1, Ordering::SeqCst);
        }),
    );
    for _ in 0..3 {
        assert!(
            timers
                .background(Box::new(|| panic!("must not run")), 0.0)
                .is_none()
        );
    }
    assert_eq!(reports.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn disarming_cancels_the_native_timer_but_dropping_the_handle_does_not() {
    let (send, receive) = tokio::sync::oneshot::channel();
    let disarm = native_timer(
        Box::new(move || {
            let _ = send.send(());
        }),
        60_000.0,
        false,
    );
    disarm();
    assert!(receive.await.is_err());
    let (send, receive) = tokio::sync::oneshot::channel();
    drop(native_timer(
        Box::new(move || {
            let _ = send.send(());
        }),
        0.0,
        false,
    ));
    receive.await.unwrap();
}

#[tokio::test]
async fn successful_host_timer_receives_delay_and_disarms_without_fallback() {
    type Slot = Arc<Mutex<Option<TimerCallback>>>;
    let slot: Slot = Arc::default();
    let target = slot.clone();
    let disarmed = Arc::new(AtomicUsize::new(0));
    let capture = disarmed.clone();
    let timers = Timers::new(
        Some(Arc::new(move |callback, delay, background| {
            assert_eq!(delay, 123.0);
            assert!(!background);
            *target.lock() = Some(callback);
            let capture = capture.clone();
            Some(Box::new(move || {
                capture.fetch_add(1, Ordering::SeqCst);
            }))
        })),
        Arc::new(|_: Error| panic!("should not report")),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = calls.clone();
    let disarm = timers.foreground(
        Box::new(move || {
            capture.fetch_add(1, Ordering::SeqCst);
        }),
        123.0,
    );
    disarm();
    slot.lock().take().unwrap()();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(disarmed.load(Ordering::SeqCst), 1);
}
