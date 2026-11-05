//! `AbortController`/`AbortSignal` on top of [`CancellationToken`], keeping the abort reason,
//! which becomes the message of an aborted execution.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// An abort signal: a cancellation token plus an optional reason message.
#[derive(Clone, Default, Debug)]
pub struct AbortSignal {
    token: CancellationToken,
    reason: Arc<Mutex<Option<String>>>,
}

impl AbortSignal {
    /// A signal that is not aborted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the signal has fired.
    pub fn aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The message passed to [`AbortController::abort_with_reason`], if any.
    pub fn reason(&self) -> Option<String> {
        self.reason.lock().clone()
    }

    /// Resolves once the signal fires.
    pub async fn cancelled(&self) {
        self.token.cancelled().await
    }

    /// The underlying token.
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl From<CancellationToken> for AbortSignal {
    fn from(token: CancellationToken) -> Self {
        AbortSignal { token, reason: Arc::default() }
    }
}

impl From<&CancellationToken> for AbortSignal {
    fn from(token: &CancellationToken) -> Self {
        token.clone().into()
    }
}

/// Fires an [`AbortSignal`].
#[derive(Clone, Default, Debug)]
pub struct AbortController {
    signal: AbortSignal,
}

impl AbortController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Abort without a reason; like Node's `controller.abort()`, an aborted execution then reports
    /// the default reason "This operation was aborted".
    pub fn abort(&self) {
        self.signal.token.cancel();
    }

    /// Abort with a reason, like `controller.abort(new Error(reason))`.
    pub fn abort_with_reason(&self, reason: impl Into<String>) {
        if !self.signal.aborted() {
            let mut slot = self.signal.reason.lock();
            if slot.is_none() {
                *slot = Some(reason.into());
            }
        }
        self.signal.token.cancel();
    }
}
