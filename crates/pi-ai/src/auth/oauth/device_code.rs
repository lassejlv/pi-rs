//! RFC 8628 device authorization polling (port of `auth/oauth/device-code.ts`).

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

const CANCEL_MESSAGE: &str = "Login cancelled";
const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: f64 = 1000.0;
/// RFC 8628 section 3.2: without `interval`, the client must use 5 seconds.
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 section 3.5: `slow_down` increases the interval by 5 seconds.
const SLOW_DOWN_INTERVAL_INCREMENT_MS: f64 = 5000.0;

/// Result of one token poll.
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthDeviceCodePollResult<T> {
    Pending,
    SlowDown { interval_seconds: Option<f64> },
    Failed { message: String },
    Complete(T),
}

/// Sleep `ms`, failing with `cancel_message` when `signal` aborts.
pub async fn abortable_sleep(
    ms: f64,
    signal: &CancellationToken,
    cancel_message: &str,
) -> anyhow::Result<()> {
    if signal.is_cancelled() {
        anyhow::bail!("{cancel_message}");
    }
    tokio::select! {
        _ = signal.cancelled() => anyhow::bail!("{cancel_message}"),
        _ = crate::utils::wasm::sleep(Duration::from_millis(ms.max(0.0) as u64)) => Ok(()),
    }
}

/// Options for [`poll_oauth_device_code_flow`].
pub struct OAuthDeviceCodePollOptions {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<f64>,
    pub wait_before_first_poll: bool,
    pub signal: CancellationToken,
}

/// Milliseconds since `origin` on tokio's clock (so paused test time applies).
fn elapsed_ms(origin: crate::utils::wasm::Instant) -> f64 {
    crate::utils::wasm::Instant::now()
        .saturating_duration_since(origin)
        .as_secs_f64()
        * 1000.0
}

/// Poll until the device authorization completes, fails, expires or is cancelled.
pub async fn poll_oauth_device_code_flow<T, F, Fut>(
    options: OAuthDeviceCodePollOptions,
    mut poll: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<OAuthDeviceCodePollResult<T>>>,
{
    let origin = crate::utils::wasm::Instant::now();
    let now_ms = || elapsed_ms(origin);
    let deadline = match options.expires_in_seconds {
        Some(seconds) => seconds * 1000.0,
        None => f64::INFINITY,
    };
    let mut interval_ms = MINIMUM_INTERVAL_MS.max(
        (options
            .interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
            * 1000.0)
            .floor(),
    );
    let mut slow_down_responses = 0u32;
    if options.wait_before_first_poll {
        let remaining = deadline - now_ms();
        if remaining > 0.0 {
            abortable_sleep(interval_ms.min(remaining), &options.signal, CANCEL_MESSAGE).await?;
        }
    }
    while now_ms() < deadline {
        if options.signal.is_cancelled() {
            anyhow::bail!("{CANCEL_MESSAGE}");
        }
        match poll().await? {
            OAuthDeviceCodePollResult::Complete(value) => return Ok(value),
            OAuthDeviceCodePollResult::Failed { message } => anyhow::bail!("{message}"),
            OAuthDeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                interval_ms = match interval_seconds.filter(|s| s.is_finite() && *s > 0.0) {
                    Some(seconds) => MINIMUM_INTERVAL_MS.max((seconds * 1000.0).floor()),
                    None => MINIMUM_INTERVAL_MS.max(interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS),
                };
            }
            OAuthDeviceCodePollResult::Pending => {}
        }
        let remaining = deadline - now_ms();
        if remaining <= 0.0 {
            break;
        }
        abortable_sleep(interval_ms.min(remaining), &options.signal, CANCEL_MESSAGE).await?;
    }
    anyhow::bail!(
        "{}",
        if slow_down_responses > 0 {
            SLOW_DOWN_TIMEOUT_MESSAGE
        } else {
            TIMEOUT_MESSAGE
        }
    )
}
