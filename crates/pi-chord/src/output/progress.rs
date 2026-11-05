use super::PROGRESS_BYTES_PER_SECOND;
use futures::FutureExt;
use parking_lot::Mutex;
use std::{future::Future, pin::Pin, sync::Arc};

type CancelTimer = Box<dyn FnOnce() + Send>;
type Write<E> =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<usize, E>> + Send>> + Send + Sync>;
pub type ProgressWaiter<E = String> = tokio::sync::oneshot::Receiver<Result<(), E>>;
pub type ProgressCompletion<E = String> = tokio::sync::oneshot::Sender<Result<(), E>>;

pub trait ProgressClock: Send + Sync {
    fn now(&self) -> f64;
    fn set_timer(&self, callback: Box<dyn FnOnce() + Send>, delay_ms: f64) -> CancelTimer;
}
pub struct SystemProgressClock;
impl ProgressClock for SystemProgressClock {
    fn now(&self) -> f64 {
        web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0
    }
    fn set_timer(&self, callback: Box<dyn FnOnce() + Send>, delay_ms: f64) -> CancelTimer {
        let task = crate::rt::spawn(async move {
            crate::rt::sleep(std::time::Duration::from_secs_f64(
                delay_ms.max(0.0) / 1000.0,
            ))
            .await;
            callback();
        });
        Box::new(move || task.abort())
    }
}

struct State<E> {
    waiters: Vec<ProgressCompletion<E>>,
    timer: Option<(u64, CancelTimer)>,
    timer_generation: Option<u64>,
    next_generation: u64,
    in_flight: bool,
    next_at: f64,
    dirty: bool,
    stopped: bool,
}
struct Inner<E> {
    state: Mutex<State<E>>,
    write: Write<E>,
    on_error: Arc<dyn Fn(E) + Send + Sync>,
    min_interval_ms: f64,
    clock: Arc<dyn ProgressClock>,
    settled: tokio::sync::watch::Sender<u64>,
}

/// One progress commit at a time, paced by interval and the previous write's size.
/// Changes during a commit coalesce into the following commit.
#[derive(Clone)]
pub struct ProgressWithError<E> {
    inner: Arc<Inner<E>>,
}
pub type Progress = ProgressWithError<String>;
impl<E: Clone + From<String> + Send + Sync + 'static> ProgressWithError<E> {
    pub fn new<F, Fut>(
        write: F,
        on_error: impl Fn(E) + Send + Sync + 'static,
        min_interval_ms: f64,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<usize, E>> + Send + 'static,
    {
        Self::with_clock(
            write,
            on_error,
            min_interval_ms,
            Arc::new(SystemProgressClock),
        )
    }
    pub fn with_clock<F, Fut>(
        write: F,
        on_error: impl Fn(E) + Send + Sync + 'static,
        min_interval_ms: f64,
        clock: Arc<dyn ProgressClock>,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<usize, E>> + Send + 'static,
    {
        let (settled, _) = tokio::sync::watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    waiters: vec![],
                    timer: None,
                    timer_generation: None,
                    next_generation: 0,
                    in_flight: false,
                    next_at: 0.0,
                    dirty: false,
                    stopped: false,
                }),
                write: Arc::new(move || Box::pin(write())),
                on_error: Arc::new(on_error),
                min_interval_ms,
                clock,
                settled,
            }),
        }
    }
    pub fn mark(&self) {
        self.inner.state.lock().dirty = true;
        self.schedule();
    }
    /// The receiver completes with the commit containing this mark. A stop returns
    /// unsent completion handles to the caller for its final durable write.
    pub fn mark_and_wait(&self) -> ProgressWaiter<E> {
        let (send, receive) = tokio::sync::oneshot::channel();
        {
            let mut state = self.inner.state.lock();
            state.waiters.push(send);
            state.dirty = true;
        }
        self.schedule();
        receive
    }
    fn schedule(&self) {
        let now = self.inner.clock.now();
        enum Action<E> {
            None,
            Timer(u64, f64),
            Write(Vec<ProgressCompletion<E>>),
        }
        let action = {
            let mut state = self.inner.state.lock();
            if state.stopped || state.timer_generation.is_some() || state.in_flight {
                Action::None
            } else if state.next_at > now {
                let id = state.next_generation;
                state.next_generation += 1;
                state.timer_generation = Some(id);
                Action::Timer(id, state.next_at - now)
            } else if !state.dirty {
                Action::None
            } else {
                state.dirty = false;
                state.in_flight = true;
                Action::Write(std::mem::take(&mut state.waiters))
            }
        };
        match action {
            Action::None => {}
            Action::Timer(id, delay) => {
                let weak = Arc::downgrade(&self.inner);
                let cancel = self.inner.clock.set_timer(
                    Box::new(move || {
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        let timer = {
                            let mut state = inner.state.lock();
                            if state.timer_generation != Some(id) {
                                return;
                            }
                            state.timer_generation = None;
                            state.timer.take()
                        };
                        drop(timer);
                        ProgressWithError { inner }.schedule();
                    }),
                    delay,
                );
                let unused = {
                    let mut state = self.inner.state.lock();
                    if state.timer_generation == Some(id) && !state.stopped {
                        state.timer = Some((id, cancel));
                        None
                    } else {
                        Some(cancel)
                    }
                };
                if let Some(cancel) = unused {
                    cancel();
                }
            }
            Action::Write(waiters) => {
                let inner = self.inner.clone();
                let started =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (inner.write)()));
                crate::rt::spawn(async move {
                    let result = match started {
                        Ok(operation) => {
                            std::panic::AssertUnwindSafe(operation).catch_unwind().await
                        }
                        Err(panic) => Err(panic),
                    };
                    let result = match result {
                        Ok(result) => result,
                        Err(panic) => Err(panic
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).into()))
                            .unwrap_or_else(|| "Progress write panicked".into())
                            .into()),
                    };
                    let interval = match &result {
                        Ok(bytes) => inner
                            .min_interval_ms
                            .max(*bytes as f64 * 1000.0 / PROGRESS_BYTES_PER_SECOND as f64),
                        Err(_) => inner.min_interval_ms,
                    };
                    inner.state.lock().next_at = now + interval;
                    for waiter in waiters {
                        let _ = waiter.send(result.as_ref().map(|_| ()).map_err(Clone::clone));
                    }
                    if let Err(error) = result {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            (inner.on_error)(error)
                        }));
                    }
                    let dirty = {
                        let mut state = inner.state.lock();
                        state.in_flight = false;
                        state.dirty
                    };
                    inner.settled.send_modify(|generation| *generation += 1);
                    if dirty {
                        ProgressWithError { inner }.schedule();
                    }
                });
            }
        }
    }
    pub async fn stop(&self) -> Vec<ProgressCompletion<E>> {
        let mut settled = self.inner.settled.subscribe();
        let timer = {
            let mut state = self.inner.state.lock();
            state.stopped = true;
            state.timer_generation = None;
            state.timer.take()
        };
        if let Some((_, cancel)) = timer {
            cancel();
        }
        loop {
            {
                let mut state = self.inner.state.lock();
                if !state.in_flight {
                    return std::mem::take(&mut state.waiters);
                }
            }
            if settled.changed().await.is_err() {
                return vec![];
            }
        }
    }
}
