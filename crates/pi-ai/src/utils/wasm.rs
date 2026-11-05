//! Target seams for wasm32-unknown-unknown. That target has one thread and runs
//! futures on the JavaScript microtask queue, so reqwest's fetch futures and
//! streams are not `Send`. The provider API keeps its `Send` bounds on every
//! target; on wasm32 such values are wrapped in [`assert_send`] where they cross
//! into a `Send` context. On native targets every function here is the identity
//! or the tokio call it replaces, so native behaviour is unchanged.
use std::future::Future;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
compile_error!("pi-ai's wasm32 support assumes a single-threaded target (no atomics)");

/// Identity on native targets.
#[cfg(not(target_arch = "wasm32"))]
#[inline(always)]
pub fn assert_send<T>(value: T) -> T {
    value
}
/// Present a single-threaded future or stream as `Send`.
#[cfg(target_arch = "wasm32")]
pub fn assert_send<T>(value: T) -> AssertSend<T> {
    AssertSend(value)
}

#[cfg(target_arch = "wasm32")]
pub struct AssertSend<T>(T);
// SAFETY: wasm32 without atomics has exactly one thread, so the value can
// never be observed from another thread.
#[cfg(target_arch = "wasm32")]
unsafe impl<T> Send for AssertSend<T> {}
// SAFETY: as above.
#[cfg(target_arch = "wasm32")]
unsafe impl<T> Sync for AssertSend<T> {}
#[cfg(target_arch = "wasm32")]
impl<T: Future> Future for AssertSend<T> {
    type Output = T::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<T::Output> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|this| &mut this.0) }.poll(cx)
    }
}
#[cfg(target_arch = "wasm32")]
impl<T: futures::Stream> futures::Stream for AssertSend<T> {
    type Item = T::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T::Item>> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|this| &mut this.0) }.poll_next(cx)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// `Box::pin` into a `Send` [`futures::future::BoxFuture`].
#[cfg(not(target_arch = "wasm32"))]
#[inline(always)]
pub fn boxed<'a, F>(future: F) -> futures::future::BoxFuture<'a, F::Output>
where
    F: Future + Send + 'a,
{
    Box::pin(future)
}
/// `Box::pin` into a `Send` [`futures::future::BoxFuture`].
#[cfg(target_arch = "wasm32")]
pub fn boxed<'a, F>(future: F) -> futures::future::BoxFuture<'a, F::Output>
where
    F: Future + 'a,
{
    Box::pin(AssertSend(future))
}

/// `tokio::task::JoinHandle` natively; on wasm32 the handle [`spawn`] returns.
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::{AbortHandle, JoinHandle};
/// Aborts a task spawned with [`spawn`].
#[cfg(target_arch = "wasm32")]
pub use futures::future::AbortHandle;

/// `tokio::time::{sleep, sleep_until, timeout, Instant}` natively (so paused
/// test time still applies). wasm32 has no tokio timer driver: the host's
/// `setTimeout` (window, worker or Cloudflare Worker) drives sleeps and the
/// JavaScript clock drives [`Instant`].
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{Instant, error::Elapsed, sleep, sleep_until, timeout};
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;
#[cfg(target_arch = "wasm32")]
pub fn sleep_until(deadline: Instant) -> Sleep {
    sleep(deadline.saturating_duration_since(Instant::now()))
}
/// The deadline of a [`timeout`] passed first.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;
#[cfg(target_arch = "wasm32")]
impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline has elapsed")
    }
}
#[cfg(target_arch = "wasm32")]
impl std::error::Error for Elapsed {}
#[cfg(target_arch = "wasm32")]
pub async fn timeout<F: Future>(duration: std::time::Duration, future: F) -> Result<F::Output, Elapsed> {
    let future = std::pin::pin!(future);
    let timer = std::pin::pin!(sleep(duration));
    match futures::future::select(future, timer).await {
        futures::future::Either::Left((output, _)) => Ok(output),
        futures::future::Either::Right(_) => Err(Elapsed),
    }
}
/// `setTimeout` as a future; dropping it before it fires clears the timer, so a
/// cancelled sleep does not keep a Worker or page busy.
#[cfg(target_arch = "wasm32")]
pub fn sleep(duration: std::time::Duration) -> Sleep {
    use wasm_bindgen::{JsCast, JsValue};
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
    Sleep { fired: wasm_bindgen_futures::JsFuture::from(promise), pending: id.zip(clear_timeout) }
}
#[cfg(target_arch = "wasm32")]
pub struct Sleep {
    fired: wasm_bindgen_futures::JsFuture,
    /// The timer id and `clearTimeout` while the timer has not fired.
    pending: Option<(wasm_bindgen::JsValue, js_sys::Function)>,
}
// SAFETY: wasm32 without atomics has exactly one thread.
#[cfg(target_arch = "wasm32")]
unsafe impl Send for Sleep {}
// SAFETY: as above.
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Sleep {}
#[cfg(target_arch = "wasm32")]
impl Future for Sleep {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if std::pin::Pin::new(&mut self.fired).poll(cx).is_ready() {
            self.pending = None;
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}
#[cfg(target_arch = "wasm32")]
impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some((id, clear_timeout)) = self.pending.take() {
            let _ = clear_timeout.call1(&wasm_bindgen::JsValue::UNDEFINED, &id);
        }
    }
}

/// `tokio::spawn`.
#[cfg(not(target_arch = "wasm32"))]
#[inline(always)]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}
/// Run a task on the JavaScript microtask queue. The handle aborts it or waits
/// for its output, like tokio's.
#[cfg(target_arch = "wasm32")]
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
#[cfg(target_arch = "wasm32")]
pub struct JoinHandle<T> {
    receive: futures::channel::oneshot::Receiver<T>,
    abort: futures::future::AbortHandle,
}
#[cfg(target_arch = "wasm32")]
impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.abort.abort();
    }
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }
}
#[cfg(target_arch = "wasm32")]
impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.receive)
            .poll(cx)
            .map(|output| output.map_err(|_| JoinError))
    }
}
/// The task was aborted or panicked before producing output.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct JoinError;
#[cfg(target_arch = "wasm32")]
impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("task was cancelled")
    }
}
#[cfg(target_arch = "wasm32")]
impl std::error::Error for JoinError {}

/// `tokio::fs` on native targets. wasm32 has no filesystem: these delegate to
/// `std::fs`, whose operations fail with `Unsupported` there.
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::fs;
#[cfg(target_arch = "wasm32")]
pub mod fs {
    use std::{io, path::Path};
    pub async fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    pub async fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    pub async fn metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
        std::fs::metadata(path)
    }
}

/// `tokio::net` on native targets. wasm32 has no sockets: binding or connecting
/// fails with `Unsupported`, so loopback OAuth callbacks fall back to manual
/// code entry.
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::net;
#[cfg(target_arch = "wasm32")]
pub mod net {
    use std::{
        io,
        net::SocketAddr,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "TCP sockets are not available on wasm32")
    }
    pub struct TcpListener(());
    impl TcpListener {
        pub async fn bind<A>(_address: A) -> io::Result<Self> {
            Err(unsupported())
        }
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            Err(unsupported())
        }
        pub fn poll_accept(&self, _cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
            Poll::Ready(Err(unsupported()))
        }
        pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
            Err(unsupported())
        }
    }
    pub struct TcpStream(());
    impl TcpStream {
        pub async fn connect<A>(_address: A) -> io::Result<Self> {
            Err(unsupported())
        }
    }
    impl AsyncRead for TcpStream {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(unsupported()))
        }
    }
    impl AsyncWrite for TcpStream {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(unsupported()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(unsupported()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(unsupported()))
        }
    }
}
