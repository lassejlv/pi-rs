use crate::{Error, Result, storage::StorageFuture};
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use parking_lot::Mutex;
use std::future::Future;
pub(crate) fn eager<T: Send + 'static>(
    operation: impl Future<Output = Result<T>> + Send + 'static,
) -> StorageFuture<T> {
    let (job, result) = admitted(operation);
    crate::session::executor::spawn(job);
    result
}
pub(crate) fn eager_on<T: Send + 'static>(
    session: &crate::Session,
    operation: impl Future<Output = Result<T>> + Send + 'static,
) -> StorageFuture<T> {
    let (job, result) = admitted(operation);
    session.spawn(job);
    result
}
fn admitted<T: Send + 'static>(
    operation: impl Future<Output = Result<T>> + Send + 'static,
) -> (impl Future<Output = ()> + Send + 'static, StorageFuture<T>) {
    let (send, receive) = tokio::sync::oneshot::channel();
    let job = async move {
        let result = std::panic::AssertUnwindSafe(operation)
            .catch_unwind()
            .await
            .unwrap_or_else(|p| Err(crate::session::panic_error(p)));
        let _ = send.send(result);
    };
    (
        job,
        Box::pin(async move { receive.await.map_err(|e| Error::message(e.to_string()))? }),
    )
}
/// Enqueue at call time and keep running if the caller drops its waiter.
pub(crate) struct MutationLine {
    tail: Mutex<Shared<BoxFuture<'static, ()>>>,
}
impl Default for MutationLine {
    fn default() -> Self {
        Self {
            tail: Mutex::new(async {}.boxed().shared()),
        }
    }
}
impl MutationLine {
    pub fn enqueue<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T>> + Send + 'static,
    ) -> StorageFuture<T> {
        let (finish, finished) = tokio::sync::oneshot::channel();
        let previous = std::mem::replace(
            &mut *self.tail.lock(),
            async move {
                let _ = finished.await;
            }
            .boxed()
            .shared(),
        );
        eager(async move {
            previous.await;
            let result = std::panic::AssertUnwindSafe(operation)
                .catch_unwind()
                .await
                .unwrap_or_else(|p| Err(crate::session::panic_error(p)));
            let _ = finish.send(());
            result
        })
    }
}

/// Observe an operation that is already admitted and runs independently. No
/// extra spawned task may reorder an already-ready shared result.
pub(crate) async fn observe<F: Future>(
    operation: F,
    context: &crate::Context,
) -> std::result::Result<F::Output, pi_chord::context::AwaitError> {
    if let Some(signal) = context.abort_signal() {
        signal.check()?;
        tokio::select! {biased;_ = signal.cancelled()=>Err(signal.reason().expect("cancelled signal has a reason").waiter_error().into()),result=operation=>Ok(result)}
    } else {
        Ok(operation.await)
    }
}
