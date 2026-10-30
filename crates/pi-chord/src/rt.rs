//! Runtime seams. Native builds use tokio directly (these are re-exports, so
//! native behaviour is unchanged). wasm32-unknown-unknown has no tokio runtime
//! or timer driver: tasks run on the JavaScript microtask queue and timers on
//! the host's `setTimeout` (browser window, worker or Cloudflare Worker).
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::{
    spawn,
    task::{JoinError, JoinHandle},
    time::{Instant, error::Elapsed, sleep, sleep_until, timeout},
};

#[cfg(target_arch = "wasm32")]
pub use wasm::*;
#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };
    use wasm_bindgen::{JsCast, JsValue};

    #[cfg(target_feature = "atomics")]
    compile_error!("pi-chord's wasm32 runtime assumes a single-threaded target (no atomics)");

    /// Spawn on the microtask queue; the handle aborts or awaits the task.
    pub fn spawn<F: Future + 'static>(future: F) -> JoinHandle<F::Output> {
        let (send, receive) = futures::channel::oneshot::channel();
        let (task, abort) = futures::future::abortable(future);
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(output) = task.await {
                let _ = send.send(output);
            }
        });
        JoinHandle { receive, abort }
    }
    pub struct JoinHandle<T> {
        receive: futures::channel::oneshot::Receiver<T>,
        abort: futures::future::AbortHandle,
    }
    impl<T> JoinHandle<T> {
        pub fn abort(&self) {
            self.abort.abort();
        }
    }
    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.receive)
                .poll(cx)
                .map(|output| output.map_err(|_| JoinError))
        }
    }
    /// The task was aborted, or panicked, before producing output.
    #[derive(Debug)]
    pub struct JoinError;
    impl std::fmt::Display for JoinError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("task was cancelled")
        }
    }
    impl std::error::Error for JoinError {}

    pub use web_time::Instant;
    pub fn sleep_until(deadline: Instant) -> Sleep {
        sleep(deadline.saturating_duration_since(Instant::now()))
    }
    /// The deadline of a [`timeout`] passed first.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Elapsed;
    impl std::fmt::Display for Elapsed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }
    impl std::error::Error for Elapsed {}
    pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
        let future = std::pin::pin!(future);
        let timer = std::pin::pin!(sleep(duration));
        match futures::future::select(future, timer).await {
            futures::future::Either::Left((output, _)) => Ok(output),
            futures::future::Either::Right(_) => Err(Elapsed),
        }
    }

    /// `setTimeout` as a future. Dropping it before it fires clears the timer,
    /// so a cancelled sleep does not keep a Worker or page busy. It is `Send`
    /// because wasm32 without atomics has one thread.
    pub fn sleep(duration: Duration) -> Sleep {
        let global = js_sys::global();
        let function = |name: &str| {
            js_sys::Reflect::get(&global, &JsValue::from_str(name))
                .ok()
                .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        };
        let (set_timeout, clear_timeout) = (function("setTimeout"), function("clearTimeout"));
        let mut id = None;
        let promise = js_sys::Promise::new(&mut |resolve, _reject| match &set_timeout {
            Some(set_timeout) => {
                id = set_timeout
                    .call2(&global, &resolve, &JsValue::from_f64(duration.as_secs_f64() * 1000.0))
                    .ok();
            }
            // No timer host: resolve at once rather than never.
            None => {
                let _ = resolve.call0(&JsValue::NULL);
            }
        });
        Sleep {
            fired: wasm_bindgen_futures::JsFuture::from(promise),
            pending: id.zip(clear_timeout),
        }
    }
    pub struct Sleep {
        fired: wasm_bindgen_futures::JsFuture,
        /// The timer id and `clearTimeout` while the timer has not fired.
        pending: Option<(JsValue, js_sys::Function)>,
    }
    // SAFETY: wasm32 without atomics has exactly one thread.
    unsafe impl Send for Sleep {}
    // SAFETY: as above.
    unsafe impl Sync for Sleep {}
    impl Future for Sleep {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let ready = Pin::new(&mut self.fired).poll(cx).is_ready();
            if ready {
                self.pending = None;
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for Sleep {
        fn drop(&mut self) {
            if let Some((id, clear_timeout)) = self.pending.take() {
                let _ = clear_timeout.call1(&JsValue::UNDEFINED, &id);
            }
        }
    }
}
