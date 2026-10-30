//! Abort signal helpers (port of `utils/abort.ts` and `utils/abort-signals.ts`).
//!
//! `tokio_util::sync::CancellationToken` is the Rust counterpart of `AbortSignal`.

use std::future::Future;

use tokio_util::sync::CancellationToken;

use super::provider_retry::AbortError;

/// The reason of an `AbortSignal` aborted without one: the `DOMException` named `AbortError`
/// that `signal.throwIfAborted()`, `raceWithAbortSignal` and `fetch` reject with.
///
/// `CancellationToken` carries no reason, and every TS signal pi aborts is aborted by
/// `AbortController.abort()` without one, so this is the error of every abort observed through
/// the signal itself. Provider requests wrapped in `retryProviderRequest` report its own
/// [`AbortError`] ("Request aborted") instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("This operation was aborted")]
pub struct SignalAbortError;

impl SignalAbortError {
    /// The JS error name.
    pub fn name(&self) -> &'static str {
        "AbortError"
    }
}

/// `signal?.throwIfAborted()`.
pub fn throw_if_aborted(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

/// Whether `error` (or an error it wraps) is an abort: the signal's own reason or
/// provider-retry's "Request aborted" (both named `AbortError` in TS).
pub fn is_abort_error(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<SignalAbortError>() || e.is::<AbortError>())
}

/// An operation-local signal for APIs whose signal is optional.
pub fn operation_signal(signal: Option<&CancellationToken>) -> CancellationToken {
    signal.cloned().unwrap_or_default()
}

/// Stop waiting for `operation` when `signal` is cancelled (the operation is dropped).
pub async fn race_with_abort_signal<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    signal: &CancellationToken,
) -> anyhow::Result<T> {
    if signal.is_cancelled() {
        return Err(SignalAbortError.into());
    }
    tokio::select! {
        result = operation => result,
        _ = signal.cancelled() => Err(SignalAbortError.into()),
    }
}

/// Combine signals: `None` when there are none, the signal itself when there is one, and
/// otherwise a new token cancelled when any input is cancelled (requires a tokio runtime).
pub fn combine_abort_signals(signals: &[Option<CancellationToken>]) -> Option<CancellationToken> {
    let active: Vec<CancellationToken> = signals.iter().flatten().cloned().collect();
    match active.len() {
        0 => None,
        1 => Some(active[0].clone()),
        _ => {
            let combined = CancellationToken::new();
            if active.iter().any(CancellationToken::is_cancelled) {
                combined.cancel();
                return Some(combined);
            }
            let target = combined.clone();
            crate::utils::wasm::spawn(async move {
                let waits = active.iter().map(|s| Box::pin(s.cancelled())).collect::<Vec<_>>();
                tokio::select! {
                    _ = futures::future::select_all(waits) => target.cancel(),
                    _ = target.cancelled() => {}
                }
            });
            Some(combined)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn combines_signals() {
        assert!(combine_abort_signals(&[None, None]).is_none());
        let a = CancellationToken::new();
        let b = CancellationToken::new();
        let combined = combine_abort_signals(&[Some(a.clone()), None, Some(b.clone())]).unwrap();
        assert!(!combined.is_cancelled());
        b.cancel();
        crate::utils::wasm::timeout(std::time::Duration::from_secs(1), combined.cancelled()).await.unwrap();
        let err = race_with_abort_signal(async { Ok::<_, anyhow::Error>(1) }, &a).await.unwrap();
        assert_eq!(err, 1);
    }

    #[tokio::test]
    async fn signal_aborts_reject_with_the_default_reason() {
        // `AbortController.abort()` without a reason: the DOMException "This operation was aborted".
        let signal = CancellationToken::new();
        assert!(throw_if_aborted(Some(&signal)).is_ok());
        assert!(throw_if_aborted(None).is_ok());
        signal.cancel();
        let error = throw_if_aborted(Some(&signal)).unwrap_err();
        assert_eq!(error.to_string(), "This operation was aborted");
        assert!(is_abort_error(&error));
        let error = race_with_abort_signal(std::future::pending::<anyhow::Result<()>>(), &signal).await.unwrap_err();
        assert_eq!(error.to_string(), "This operation was aborted");
        assert_eq!(error.downcast_ref::<SignalAbortError>().map(SignalAbortError::name), Some("AbortError"));

        let pending = CancellationToken::new();
        let trigger = pending.clone();
        let raced = crate::utils::wasm::spawn(async move { race_with_abort_signal(std::future::pending::<anyhow::Result<()>>(), &pending).await });
        trigger.cancel();
        assert_eq!(raced.await.unwrap().unwrap_err().to_string(), "This operation was aborted");

        // provider-retry's error is an abort too, and so is a wrapped one.
        assert!(is_abort_error(&anyhow::Error::new(AbortError).context("auth failed")));
        assert!(!is_abort_error(&anyhow::anyhow!("This operation was aborted")));
    }
}
