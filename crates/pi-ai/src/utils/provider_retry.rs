//! SDK-style retry of provider requests with interruptible backoff (port of `utils/provider-retry.ts`).

use std::future::Future;

use rand::Rng;
use tokio_util::sync::CancellationToken;

use super::error_body::ProviderHttpError;

const DEFAULT_MAX_RETRY_DELAY_MS: f64 = 60_000.0;

#[derive(Debug, Clone, Default)]
pub struct ProviderRetryOptions {
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub signal: Option<CancellationToken>,
    /// HTTP statuses that fail at once although the default policy would retry them.
    pub no_retry_statuses: Vec<u16>,
}

/// The error returned when the request was aborted.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Request aborted")]
pub struct AbortError;

/// Mirrors the pinned OpenAI/Anthropic SDK retry policy.
fn is_retryable_provider_error(error: &ProviderHttpError) -> bool {
    match error.header("x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match error.status {
        None => true,
        Some(status) => status == 408 || status == 409 || status == 429 || status >= 500,
    }
}

fn validate_server_retry_delay_ms(delay_ms: f64, max_retry_delay_ms: Option<u64>, message: &str) -> anyhow::Result<f64> {
    let max_delay = max_retry_delay_ms.map(|v| v as f64).unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_delay > 0.0 && delay_ms > max_delay {
        anyhow::bail!(
            "Server requested {}s retry delay (max: {}s). {}",
            (delay_ms / 1000.0).ceil(),
            (max_delay / 1000.0).ceil(),
            message
        );
    }
    Ok(delay_ms)
}

/// `Number.parseFloat` semantics: parse the longest numeric prefix.
pub fn js_parse_float(text: &str) -> f64 {
    let t = text.trim_start();
    let mut end = 0;
    let bytes = t.as_bytes();
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exp = false;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_digit() {
            seen_digit = true;
        } else if (c == b'+' || c == b'-') && (end == 0 || matches!(bytes[end - 1], b'e' | b'E')) {
        } else if c == b'.' && !seen_dot && !seen_exp {
            seen_dot = true;
        } else if (c == b'e' || c == b'E') && seen_digit && !seen_exp {
            seen_exp = true;
        } else {
            break;
        }
        end += 1;
    }
    let mut candidate = &t[..end];
    while !candidate.is_empty() {
        if let Ok(v) = candidate.parse::<f64>() {
            return v;
        }
        candidate = &candidate[..candidate.len() - 1];
    }
    if t.starts_with("Infinity") || t.starts_with("+Infinity") {
        return f64::INFINITY;
    }
    if t.starts_with("-Infinity") {
        return f64::NEG_INFINITY;
    }
    f64::NAN
}

fn get_retry_delay_ms(error: &ProviderHttpError, retry_index: u32, max_retry_delay_ms: Option<u64>) -> anyhow::Result<f64> {
    if let Some(retry_after_ms) = error.header("retry-after-ms").filter(|v| !v.is_empty()) {
        let value = js_parse_float(retry_after_ms);
        if value.is_finite() {
            return validate_server_retry_delay_ms(value, max_retry_delay_ms, &error.message);
        }
    }
    if let Some(retry_after) = error.header("retry-after").filter(|v| !v.is_empty()) {
        let seconds = js_parse_float(retry_after);
        let delay_ms = if seconds.is_nan() {
            match httpdate_parse(retry_after) {
                Some(ms) => ms - crate::utils::now_ms() as f64,
                None => f64::NAN,
            }
        } else {
            seconds * 1000.0
        };
        if delay_ms.is_finite() {
            return validate_server_retry_delay_ms(delay_ms, max_retry_delay_ms, &error.message);
        }
    }
    let exponential = (0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0;
    Ok(exponential * (1.0 - rand::thread_rng().r#gen::<f64>() * 0.25))
}

/// Parse an HTTP date (`Date.parse` for the IMF-fixdate format) into epoch milliseconds.
fn httpdate_parse(text: &str) -> Option<f64> {
    // Format: "Sun, 06 Nov 1994 08:49:37 GMT"
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| *m == parts[2])? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<i64> = parts[4].split(':').filter_map(|p| p.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2]) * 1000) as f64)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

async fn abortable_sleep(ms: f64, signal: Option<&CancellationToken>) -> Result<(), AbortError> {
    let duration = std::time::Duration::from_millis(ms.max(0.0) as u64);
    match signal {
        Some(signal) => {
            if signal.is_cancelled() {
                return Err(AbortError);
            }
            tokio::select! {
                _ = signal.cancelled() => Err(AbortError),
                _ = crate::utils::wasm::sleep(duration) => Ok(()),
            }
        }
        None => {
            crate::utils::wasm::sleep(duration).await;
            Ok(())
        }
    }
}

/// Reproduce the SDK retry behavior with interruptible backoff.
pub async fn retry_provider_request<T, F, Fut>(mut request: F, options: &ProviderRetryOptions) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let max_retries = options.max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries;
    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if options.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
                    return Err(AbortError.into());
                }
                let Some(http) = error.downcast_ref::<ProviderHttpError>() else {
                    return Err(error);
                };
                if retries_remaining == 0 || !is_retryable_provider_error(http) {
                    return Err(error);
                }
                if let Some(status) = http.status
                    && options.no_retry_statuses.contains(&status) {
                        return Err(error);
                    }
                let retry_index = max_retries - retries_remaining;
                retries_remaining -= 1;
                let delay = get_retry_delay_ms(http, retry_index, options.max_retry_delay_ms)?;
                abortable_sleep(delay, options.signal.as_ref()).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_float_prefix() {
        assert_eq!(js_parse_float("1.5s"), 1.5);
        assert!(js_parse_float("abc").is_nan());
        assert_eq!(js_parse_float(" 20 "), 20.0);
    }

    #[test]
    fn http_date() {
        assert_eq!(httpdate_parse("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777000.0));
    }
}
