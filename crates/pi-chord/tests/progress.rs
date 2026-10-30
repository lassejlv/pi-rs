use parking_lot::Mutex;
use pi_chord::output::{PROGRESS_BYTES_PER_SECOND, Progress, ProgressClock};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Timer {
    at: f64,
    cancelled: Arc<AtomicBool>,
    callback: Option<Box<dyn FnOnce() + Send>>,
}
#[derive(Default)]
struct ManualClock {
    now: Mutex<f64>,
    timers: Mutex<Vec<Timer>>,
    delays: Mutex<Vec<f64>>,
    disarmed: Mutex<Vec<Arc<AtomicBool>>>,
}
impl ProgressClock for ManualClock {
    fn now(&self) -> f64 {
        *self.now.lock()
    }
    fn set_timer(
        &self,
        callback: Box<dyn FnOnce() + Send>,
        delay_ms: f64,
    ) -> Box<dyn FnOnce() + Send> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.delays.lock().push(delay_ms);
        self.disarmed.lock().push(cancelled.clone());
        self.timers.lock().push(Timer {
            at: self.now() + delay_ms,
            cancelled: cancelled.clone(),
            callback: Some(callback),
        });
        Box::new(move || cancelled.store(true, Ordering::SeqCst))
    }
}
impl ManualClock {
    fn advance(&self, now: f64) {
        *self.now.lock() = now;
        loop {
            let timer = {
                let mut timers = self.timers.lock();
                timers
                    .iter()
                    .position(|t| t.at <= now && !t.cancelled.load(Ordering::SeqCst))
                    .map(|i| timers.remove(i))
            };
            let Some(mut timer) = timer else {
                break;
            };
            timer.callback.take().unwrap()();
        }
    }
    fn fire_early(&self) {
        let mut timer = {
            let mut timers = self.timers.lock();
            let index = timers
                .iter()
                .position(|t| !t.cancelled.load(Ordering::SeqCst))
                .unwrap();
            timers.remove(index)
        };
        timer.callback.take().unwrap()();
    }
    fn delays(&self) -> Vec<f64> {
        self.delays.lock().clone()
    }
    fn disarmed(&self, index: usize) -> bool {
        self.disarmed.lock()[index].load(Ordering::SeqCst)
    }
    fn pending(&self) -> usize {
        self.timers
            .lock()
            .iter()
            .filter(|t| !t.cancelled.load(Ordering::SeqCst))
            .count()
    }
}

#[tokio::test]
async fn first_change_is_immediate_and_later_changes_coalesce_by_written_size() {
    let clock = Arc::new(ManualClock::default());
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let progress = Progress::with_clock(
        move || {
            send.send(()).unwrap();
            async { Ok(PROGRESS_BYTES_PER_SECOND) }
        },
        |_| panic!("unexpected error"),
        100.0,
        clock.clone(),
    );
    let first = progress.mark_and_wait();
    receive.recv().await.unwrap();
    first.await.unwrap().unwrap();
    let second = progress.mark_and_wait();
    progress.mark();
    assert_eq!(clock.pending(), 1);
    clock.advance(999.0);
    tokio::task::yield_now().await;
    assert!(receive.try_recv().is_err());
    clock.advance(1000.0);
    receive.recv().await.unwrap();
    second.await.unwrap().unwrap();
    assert!(progress.stop().await.is_empty());
}

#[tokio::test]
async fn early_timer_rechecks_clock_and_stop_disarms_it() {
    let clock = Arc::new(ManualClock::default());
    let progress = Progress::with_clock(|| async { Ok(1) }, |_| {}, 100.0, clock.clone());
    progress.mark_and_wait().await.unwrap().unwrap();
    let pending = progress.mark_and_wait();
    clock.fire_early();
    assert_eq!(clock.pending(), 1);
    let final_waiters = progress.stop().await;
    assert_eq!(clock.pending(), 0);
    assert_eq!(final_waiters.len(), 1);
    final_waiters
        .into_iter()
        .next()
        .unwrap()
        .send(Ok(()))
        .unwrap();
    pending.await.unwrap().unwrap();
    clock.advance(10000.0);
}

#[tokio::test]
async fn at_most_one_write_runs_and_stop_returns_only_the_pending_change() {
    let clock = Arc::new(ManualClock::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let progress = Progress::with_clock(
        {
            let gate = gate.clone();
            move || {
                send.send(()).unwrap();
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(1)
                }
            }
        },
        |_| {},
        100.0,
        clock,
    );
    let first = progress.mark_and_wait();
    receive.recv().await.unwrap();
    let second = progress.mark_and_wait();
    let stopped = tokio::spawn({
        let progress = progress.clone();
        async move { progress.stop().await }
    });
    tokio::task::yield_now().await;
    assert!(!stopped.is_finished());
    assert!(receive.try_recv().is_err());
    gate.add_permits(1);
    let final_waiters = stopped.await.unwrap();
    first.await.unwrap().unwrap();
    assert_eq!(final_waiters.len(), 1);
    final_waiters
        .into_iter()
        .next()
        .unwrap()
        .send(Ok(()))
        .unwrap();
    second.await.unwrap().unwrap();
    assert!(receive.try_recv().is_err());
}

#[tokio::test]
async fn write_failure_rejects_its_waiters_reports_and_preserves_minimum_interval() {
    let clock = Arc::new(ManualClock::default());
    let (reported, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let progress = Progress::with_clock(
        || async { Err("failure".into()) },
        move |error| {
            reported.send(error).unwrap();
        },
        100.0,
        clock.clone(),
    );
    assert_eq!(
        progress.mark_and_wait().await.unwrap().unwrap_err(),
        "failure"
    );
    assert_eq!(reports.recv().await.unwrap(), "failure");
    let second = progress.mark_and_wait();
    assert_eq!(clock.pending(), 1);
    clock.advance(100.0);
    assert_eq!(second.await.unwrap().unwrap_err(), "failure");
    assert_eq!(reports.recv().await.unwrap(), "failure");
    assert!(progress.stop().await.is_empty());
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn progress_commits_the_first_change_at_once_then_waits_at_least_100_ms_and_the_written_size_at_100_kib_s()
 {
    let clock = Arc::new(ManualClock::default());
    let commits = Arc::new(Mutex::new(Vec::<f64>::new()));
    let size = Arc::new(std::sync::atomic::AtomicUsize::new(50 * 1024));
    let progress = Progress::with_clock(
        {
            let (clock, commits, size) = (clock.clone(), commits.clone(), size.clone());
            move || {
                commits.lock().push(clock.now());
                let size = size.load(Ordering::SeqCst);
                async move { Ok(size) }
            }
        },
        |_| {},
        100.0,
        clock.clone(),
    );
    progress.mark();
    settle().await;
    assert_eq!(*commits.lock(), [0.0]);
    // 50 KiB buys 500 ms; changes meanwhile coalesce into one commit.
    size.store(10, Ordering::SeqCst);
    progress.mark();
    progress.mark();
    clock.advance(499.0);
    settle().await;
    assert_eq!(*commits.lock(), [0.0]);
    clock.advance(500.0);
    settle().await;
    assert_eq!(*commits.lock(), [0.0, 500.0]);
    // A small commit still waits the minimum 100 ms.
    progress.mark();
    clock.advance(599.0);
    settle().await;
    assert_eq!(*commits.lock(), [0.0, 500.0]);
    clock.advance(600.0);
    settle().await;
    assert_eq!(*commits.lock(), [0.0, 500.0, 600.0]);
    assert!(progress.stop().await.is_empty());
}

#[tokio::test]
async fn progress_waits_the_configured_minimum_interval_between_small_commits() {
    let clock = Arc::new(ManualClock::default());
    let commits = Arc::new(Mutex::new(Vec::<f64>::new()));
    let progress = Progress::with_clock(
        {
            let (clock, commits) = (clock.clone(), commits.clone());
            move || {
                commits.lock().push(clock.now());
                async { Ok(10) }
            }
        },
        |_| {},
        500.0,
        clock.clone(),
    );
    progress.mark();
    settle().await;
    progress.mark();
    clock.advance(499.0);
    settle().await;
    assert_eq!(*commits.lock(), [0.0]);
    clock.advance(500.0);
    settle().await;
    assert_eq!(*commits.lock(), [0.0, 500.0]);
    assert!(progress.stop().await.is_empty());
}

#[tokio::test]
async fn progress_paces_on_the_clock_it_is_given() {
    let clock = Arc::new(ManualClock::default());
    let commits = Arc::new(Mutex::new(Vec::<f64>::new()));
    *clock.now.lock() = 1000.0;
    let progress = Progress::with_clock(
        {
            let (clock, commits) = (clock.clone(), commits.clone());
            move || {
                commits.lock().push(clock.now());
                async { Ok(10) }
            }
        },
        |_| {},
        100.0,
        clock.clone(),
    );
    progress.mark_and_wait().await.unwrap().unwrap();
    assert_eq!(*commits.lock(), [1000.0]);
    // Let the first commit finish settling.
    settle().await;
    *clock.now.lock() = 1040.0;
    let second = progress.mark_and_wait();
    assert_eq!(clock.delays(), [60.0]);
    // A timer that fires early commits nothing and waits for the rest.
    *clock.now.lock() = 1080.0;
    clock.fire_early();
    settle().await;
    assert_eq!(*commits.lock(), [1000.0]);
    assert_eq!(clock.delays(), [60.0, 20.0]);
    *clock.now.lock() = 1100.0;
    clock.fire_early();
    second.await.unwrap().unwrap();
    assert_eq!(*commits.lock(), [1000.0, 1100.0]);
    settle().await;
    progress.mark();
    assert_eq!(clock.delays(), [60.0, 20.0, 100.0]);
    assert!(progress.stop().await.is_empty());
    assert!(clock.disarmed(2));
}

#[tokio::test]
async fn progress_rejects_the_waiters_of_a_failed_commit_and_reports_its_error() {
    let (reported, mut reports) = tokio::sync::mpsc::unbounded_channel();
    let progress = Progress::new(
        || async { Err("commit failed".to_string()) },
        move |error| {
            reported.send(error).unwrap();
        },
        100.0,
    );
    assert_eq!(
        progress.mark_and_wait().await.unwrap().unwrap_err(),
        "commit failed"
    );
    assert_eq!(reports.recv().await.unwrap(), "commit failed");
    assert!(reports.try_recv().is_err());
}

#[tokio::test]
async fn progress_stops_waits_for_the_commit_in_flight_and_hands_back_waiters_no_commit_covered_yet()
 {
    let clock = Arc::new(ManualClock::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let progress = Progress::with_clock(
        {
            let gate = gate.clone();
            move || {
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(0)
                }
            }
        },
        |_| {},
        100.0,
        clock,
    );
    let first = progress.mark_and_wait();
    let second = progress.mark_and_wait();
    let stopped = tokio::spawn({
        let progress = progress.clone();
        async move { progress.stop().await }
    });
    gate.add_permits(1);
    let pending = stopped.await.unwrap();
    first.await.unwrap().unwrap();
    assert_eq!(pending.len(), 1);
    pending.into_iter().next().unwrap().send(Ok(())).unwrap();
    second.await.unwrap().unwrap();
}
