//! Port of `packages/coding-agent/src/utils/abort.ts`.

use std::future::Future;

use tokio_util::sync::CancellationToken;

/// The default abort reason (`DOMException` named `AbortError`, "This operation was aborted").
///
/// TS falls back to `new Error("The operation was aborted")` only for a signal without a reason,
/// which no `AbortController.abort()` produces; a `CancellationToken` aborts like `abort()`
/// without an argument, so its reason is the `DOMException`.
pub use pi_ai::utils::abort::SignalAbortError as OperationAbortedError;

/// Normalize an optional public signal without imposing a deadline.
pub fn operation_signal(signal: Option<CancellationToken>) -> CancellationToken {
    signal.unwrap_or_default()
}

/// Stop waiting on abort while observing the abandoned operation through settlement.
///
/// When the signal fires first, the operation keeps running in the background (its result
/// is discarded), like the abandoned promise in TS.
pub async fn race_with_abort_signal<T, F>(
    operation: F,
    signal: Option<CancellationToken>,
) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let Some(signal) = signal else {
        return operation.await;
    };
    if signal.is_cancelled() {
        crate::utils::uncaught::spawn(async move {
            let _ = operation.await;
        });
        return Err(OperationAbortedError.into());
    }
    let mut operation = Box::pin(operation);
    tokio::select! {
        biased;
        result = &mut operation => result,
        _ = signal.cancelled() => {
            crate::utils::uncaught::spawn(async move {
                let _ = operation.await;
            });
            Err(OperationAbortedError.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passes_through_without_signal() {
        let value = race_with_abort_signal(async { Ok(5) }, None).await.unwrap();
        assert_eq!(value, 5);
    }

    #[tokio::test]
    async fn rejects_when_already_aborted() {
        let signal = CancellationToken::new();
        signal.cancel();
        let err = race_with_abort_signal(async { Ok(5) }, Some(signal)).await.unwrap_err();
        assert_eq!(err.to_string(), "This operation was aborted");
    }

    #[tokio::test]
    async fn rejects_when_aborted_while_pending() {
        let signal = CancellationToken::new();
        let s = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            s.cancel();
        });
        let err = race_with_abort_signal(
            async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                Ok(1)
            },
            Some(signal),
        )
        .await
        .unwrap_err();
        assert!(err.downcast_ref::<OperationAbortedError>().is_some());
    }

    #[test]
    fn operation_signal_defaults() {
        assert!(!operation_signal(None).is_cancelled());
        let s = CancellationToken::new();
        s.cancel();
        assert!(operation_signal(Some(s)).is_cancelled());
    }
}
