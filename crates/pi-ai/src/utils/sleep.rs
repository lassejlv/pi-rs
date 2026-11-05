//! Abortable sleep (port of `utils/sleep.ts`).

use tokio_util::sync::CancellationToken;

use super::abort::SignalAbortError;

/// Sleep for `ms` milliseconds, failing with [`SignalAbortError`] (`signal.reason`) when `signal` is cancelled.
pub async fn sleep(ms: u64, signal: &CancellationToken) -> Result<(), SignalAbortError> {
    if signal.is_cancelled() {
        return Err(SignalAbortError);
    }
    tokio::select! {
        _ = signal.cancelled() => Err(SignalAbortError),
        _ = super::wasm::sleep(std::time::Duration::from_millis(ms)) => Ok(()),
    }
}
