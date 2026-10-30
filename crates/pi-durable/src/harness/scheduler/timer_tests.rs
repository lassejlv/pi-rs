use super::*;

#[tokio::test]
async fn wake_updates_precede_later_session_continuations() {
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    let arms = Arc::new(AtomicU64::new(0));
    let count = arms.clone();
    let scheduler = TaskScheduler::open(
        Arc::new(crate::MemoryStorage::default()),
        crate::harness::catalog::TaskCatalog::default(),
        SchedulerOptions::default(),
        SessionOptions {
            now: Arc::new(move || clock.load(Ordering::SeqCst) as f64),
            set_timer: Some(Arc::new(move |_, _, background| {
                if !background {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                Some(Box::new(|| {}))
            })),
            ..Default::default()
        },
        &Context::default(),
    )
    .await
    .unwrap();
    while scheduler.inner.state.lock().wake_pending {
        tokio::task::yield_now().await;
    }
    let record: TaskRecord = crate::wire::decode(
        &pi_chord::json::parse(
            r#"{"id":1,"conversationId":1,"kind":"test.sleep","version":1,"input":null,"background":false,"abortRequested":false,"state":{"status":"sleeping","checkpoint":{"phase":"run"},"until":10}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    scheduler.track(record);
    scheduler.inner.state.lock().enabled = true;
    scheduler.schedule_wake();
    let (done, finished) = tokio::sync::oneshot::channel();
    scheduler.inner.session.spawn(async move {
        now.store(10, Ordering::SeqCst);
        done.send(()).unwrap();
    });
    finished.await.unwrap();
    while scheduler.inner.state.lock().wake_pending {
        tokio::task::yield_now().await;
    }
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
    assert_eq!(
        arms.load(Ordering::SeqCst),
        1,
        "a later Session continuation must not overtake wake calculation and erase the sleep deadline"
    );
}

async fn scheduler(timer: crate::timers::SetTimer) -> TaskScheduler {
    TaskScheduler::open(
        Arc::new(crate::MemoryStorage::default()),
        crate::harness::catalog::TaskCatalog::default(),
        SchedulerOptions::default(),
        SessionOptions {
            now: Arc::new(|| 0.0),
            set_timer: Some(timer),
            ..Default::default()
        },
        &Context::default(),
    )
    .await
    .unwrap()
}

#[test]
fn pending_foreground_timers_retain_the_scheduler_until_runtime_shutdown() {
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let capture = callbacks.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owner = runtime.block_on(async {
        let scheduler = scheduler(Arc::new(move |callback, _, background| {
            assert!(!background);
            capture.lock().push(callback);
            // A host disarm closure may share ownership of its callback slot.
            let capture = capture.clone();
            Some(Box::new(move || capture.lock().clear()))
        }))
        .await;
        while scheduler.inner.state.lock().wake_pending {
            tokio::task::yield_now().await;
        }
        scheduler.arm_timer(Some(10.0));
        let owner = Arc::downgrade(&scheduler.inner);
        drop(scheduler);
        tokio::task::yield_now().await;
        assert!(
            owner.upgrade().is_some(),
            "the foreground wake is still pending"
        );
        owner
    });
    drop(runtime);
    assert!(
        owner.upgrade().is_none(),
        "runtime shutdown must release the scheduler even if the host retains its callback"
    );
    let callback = callbacks.lock().pop().unwrap();
    callback();
}

#[tokio::test]
async fn a_callback_fired_before_registration_returns_does_not_remain_armed() {
    let arms = Arc::new(AtomicU64::new(0));
    let count = arms.clone();
    let scheduler = scheduler(Arc::new(move |callback, _, background| {
        if !background && count.fetch_add(1, Ordering::SeqCst) == 0 {
            callback();
        }
        Some(Box::new(|| {}))
    }))
    .await;
    scheduler.arm_timer(Some(10.0));
    scheduler.arm_timer(Some(10.0));
    assert_eq!(
        arms.load(Ordering::SeqCst),
        2,
        "a fired timer cannot satisfy the next wake deadline"
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_superseded_callback_cannot_clear_the_new_deadline() {
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let capture = callbacks.clone();
    let scheduler = scheduler(Arc::new(move |callback, _, background| {
        if !background {
            capture.lock().push(callback);
        }
        Some(Box::new(|| {}))
    }))
    .await;
    scheduler.arm_timer(Some(10.0));
    scheduler.arm_timer(Some(20.0));
    let old = callbacks.lock().remove(0);
    old();
    scheduler.arm_timer(Some(20.0));
    assert_eq!(
        callbacks.lock().len(),
        1,
        "the replacement timer must remain registered"
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}
