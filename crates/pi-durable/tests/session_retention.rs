use parking_lot::Mutex;
use pi_durable::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
type Callback = Box<dyn FnOnce() + Send>;
#[derive(Default)]
struct Timers {
    slots: Mutex<Vec<Arc<Mutex<Option<Callback>>>>>,
    calls: AtomicUsize,
}
impl Timers {
    fn host(self: &Arc<Self>) -> session::SetTimer {
        let timers = self.clone();
        Arc::new(move |callback, _, background| {
            assert!(background);
            timers.calls.fetch_add(1, Ordering::SeqCst);
            let slot = Arc::new(Mutex::new(Some(callback)));
            timers.slots.lock().push(slot.clone());
            Some(Box::new(move || {
                slot.lock().take();
            }))
        })
    }
    fn fire(&self) {
        let callback = self
            .slots
            .lock()
            .iter()
            .find_map(|s| s.lock().take())
            .expect("armed timer");
        callback();
    }
    fn armed(&self) -> usize {
        self.slots
            .lock()
            .iter()
            .filter(|s| s.lock().is_some())
            .count()
    }
}
fn token() -> SessionDocToken<JsonObject> {
    define_doc(DocDefinition::new(
        "value",
        1,
        SessionScope,
        || Ok(JsonObject::default()),
        |_, _, _| false,
    ))
    .unwrap()
}
async fn create(session: &Session) {
    session
        .commit(
            |tx| async move {
                tx.doc(&token(), ()).await?;
                Ok(())
            },
            &Context::default(),
        )
        .await
        .unwrap();
}
async fn barrier(session: &Session) {
    session
        .commit(|_| async { Ok(()) }, &Context::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn idle_use_extends_retention_and_early_timers_rearm() {
    let clock = Arc::new(AtomicU64::new(0));
    let read_clock = clock.clone();
    let timers = Arc::new(Timers::default());
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            now: Arc::new(move || read_clock.load(Ordering::SeqCst) as f64),
            document_retention_ms: 100.0,
            set_timer: Some(timers.host()),
            ..Default::default()
        },
    );
    create(&session).await;
    assert_eq!(session.cache_sizes().cached, 1);
    assert_eq!(timers.armed(), 1);
    clock.store(50, Ordering::SeqCst);
    session
        .snapshot(&token(), (), &Context::default())
        .await
        .unwrap();
    clock.store(100, Ordering::SeqCst);
    timers.fire();
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 1);
    assert_eq!(timers.armed(), 1);
    clock.store(150, Ordering::SeqCst);
    timers.fire();
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 0);
    assert_eq!(timers.armed(), 0);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn observers_pin_incarnations_and_last_detach_restarts_retention() {
    let clock = Arc::new(AtomicU64::new(0));
    let read_clock = clock.clone();
    let timers = Arc::new(Timers::default());
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            now: Arc::new(move || read_clock.load(Ordering::SeqCst) as f64),
            document_retention_ms: 100.0,
            set_timer: Some(timers.host()),
            ..Default::default()
        },
    );
    create(&session).await;
    let first = session
        .watch_doc(&token(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let second = session
        .watch_doc(&token(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.cache_sizes().attached, 1);
    assert_eq!(session.cache_sizes().idle, 0);
    clock.store(1000, Ordering::SeqCst);
    timers.fire();
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 1);
    first.stop();
    assert_eq!(session.cache_sizes().idle, 0);
    second.stop();
    assert_eq!(session.cache_sizes().idle, 1);
    assert_eq!(timers.armed(), 1);
    clock.store(1100, Ordering::SeqCst);
    timers.fire();
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 0);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn zero_retention_cold_snapshots_evict_without_host_timers() {
    let timers = Arc::new(Timers::default());
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            document_retention_ms: 0.0,
            set_timer: Some(timers.host()),
            ..Default::default()
        },
    );
    create(&session).await;
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 0);
    session
        .snapshot(&token(), (), &Context::default())
        .await
        .unwrap();
    // The sweep was queued by cold loading ahead of this barrier.
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 0);
    assert_eq!(timers.calls.load(Ordering::SeqCst), 0);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn declined_background_timers_still_sweep_at_commit_end() {
    let clock = Arc::new(AtomicU64::new(0));
    let read_clock = clock.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = calls.clone();
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            now: Arc::new(move || read_clock.load(Ordering::SeqCst) as f64),
            document_retention_ms: 100.0,
            set_timer: Some(Arc::new(move |_, _, _| {
                capture.fetch_add(1, Ordering::SeqCst);
                None
            })),
            ..Default::default()
        },
    );
    create(&session).await;
    clock.store(1000, Ordering::SeqCst);
    barrier(&session).await;
    assert_eq!(session.cache_sizes().cached, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn infinite_retention_never_arms_timers_and_unload_preserves_observation() {
    let timers = Arc::new(Timers::default());
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            document_retention_ms: f64::INFINITY,
            set_timer: Some(timers.host()),
            ..Default::default()
        },
    );
    create(&session).await;
    let watch = session
        .watch_doc(&token(), (), &Context::default())
        .await
        .unwrap()
        .unwrap();
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Arc::new(Mutex::new(Some(send)));
    watch
        .start(move |frame| {
            send.lock()
                .take()
                .unwrap()
                .send(frame.value.unwrap().len())
                .unwrap();
            async { Ok(()) }
        })
        .unwrap();
    session.unload_documents().await.unwrap();
    assert_eq!(session.cache_sizes().cached, 0);
    session
        .commit(
            |tx| async move {
                tx.doc(&token(), ()).await?.update(|value| {
                    value.insert("x".into(), 1.into());
                    Ok(())
                })
            },
            &Context::default(),
        )
        .await
        .unwrap();
    assert_eq!(receive.await.unwrap(), 1);
    assert_eq!(timers.calls.load(Ordering::SeqCst), 0);
    session.close(&Context::default()).await.unwrap();
}

#[tokio::test]
async fn commits_keep_the_pending_sweep_without_rearming_the_host_timer() {
    // Upstream `#sweepDocuments` keeps a pending sweep due no later than the oldest idle document's.
    let clock = Arc::new(AtomicU64::new(0));
    let read_clock = clock.clone();
    let timers = Arc::new(Timers::default());
    let session = Session::new(
        Arc::new(MemoryStorage::default()),
        SessionOptions {
            now: Arc::new(move || read_clock.load(Ordering::SeqCst) as f64),
            document_retention_ms: 100.0,
            set_timer: Some(timers.host()),
            ..Default::default()
        },
    );
    create(&session).await;
    assert_eq!(timers.calls.load(Ordering::SeqCst), 1);
    for at in 1..=20 {
        clock.store(at, Ordering::SeqCst);
        barrier(&session).await;
    }
    assert_eq!(timers.calls.load(Ordering::SeqCst), 1);
    assert_eq!(timers.armed(), 1);
    assert_eq!(session.cache_sizes().cached, 1);
    session.close(&Context::default()).await.unwrap();
}
