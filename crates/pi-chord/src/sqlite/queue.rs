use super::{SqliteError, SqliteFuture, SqliteResult};
use futures::FutureExt;
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc};

/// Run `task` in the background. Workers have no tokio runtime, so wasm32 uses the JavaScript microtask queue.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_task(task: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(task);
}
#[cfg(target_arch = "wasm32")]
fn spawn_task(task: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(task);
}

type Job = Box<dyn FnOnce(Arc<SerialQueue>) + Send>;
#[derive(Default)]
struct State {
    busy: bool,
    waiting: VecDeque<Job>,
}
#[derive(Default)]
pub(super) struct SerialQueue {
    state: Mutex<State>,
}
impl SerialQueue {
    fn enqueue(self: &Arc<Self>, job: Job) {
        let start = {
            let mut state = self.state.lock();
            if state.busy {
                state.waiting.push_back(job);
                None
            } else {
                state.busy = true;
                Some(job)
            }
        };
        if let Some(job) = start {
            job(self.clone());
        }
    }
    fn finished(self: &Arc<Self>) {
        let next = {
            let mut state = self.state.lock();
            if let Some(job) = state.waiting.pop_front() {
                Some(job)
            } else {
                state.busy = false;
                None
            }
        };
        if let Some(job) = next {
            let queue = self.clone();
            spawn_task(async move {
                job(queue);
            });
        }
    }
    pub fn submit<T, F>(self: &Arc<Self>, operation: F) -> SqliteFuture<T>
    where
        T: Send + 'static,
        F: FnOnce() -> SqliteFuture<T> + Send + 'static,
    {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.enqueue(Box::new(move |queue| {
            spawn_task(async move {
                let result = std::panic::AssertUnwindSafe(async move { operation().await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|panic| Err(panic_error(panic)));
                let _ = send.send(result);
                queue.finished();
            });
        }));
        Box::pin(async move {
            receive
                .await
                .map_err(|_| SqliteError::new("SQLite operation queue ended"))?
        })
    }
    pub fn submit_sync<T, F>(self: &Arc<Self>, operation: F) -> SqliteFuture<T>
    where
        T: Send + 'static,
        F: FnOnce() -> SqliteResult<T> + Send + 'static,
    {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.enqueue(Box::new(move |queue| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                .unwrap_or_else(|panic| Err(panic_error(panic)));
            let _ = send.send(result);
            queue.finished();
        }));
        Box::pin(async move {
            receive
                .await
                .map_err(|_| SqliteError::new("SQLite operation queue ended"))?
        })
    }
}
pub(super) fn panic_error(panic: Box<dyn std::any::Any + Send>) -> SqliteError {
    SqliteError::new(
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).into()))
            .unwrap_or_else(|| "SQLite callback panicked".into()),
    )
}
