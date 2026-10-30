//! Retry classification and policy-driven retry loop (port of `utils/retry.ts`).

use std::future::Future;

use futures::future::BoxFuture;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessage, StopReason};

static NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN: Lazy<Regex> = Lazy::new(|| {
    build_provider_error_pattern(&[
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
        "subscription_sharing_usage_limit_exceeded",
        "subscription_sharing_workplace_individual_usage_limit_exceeded",
    ])
});

static RETRYABLE_PROVIDER_ERROR_PATTERN: Lazy<Regex> = Lazy::new(|| {
    build_provider_error_pattern(&[
        "overloaded",
        "server_busy",
        "servers are currently busy",
        "currently experiencing high demand",
        "model is at capacity",
        "rate.?limit",
        "too many requests",
        // Status codes are matched separately (they need look-around).
        "service.?(?:is.?)?(?:temporarily.?)?unavailable",
        "server.?error",
        "internal.?error",
        "provider.?returned.?(?:an.?)?error",
        "exceeded request buffer limit while retrying upstream",
        "unable to forward upstream",
        "upstream.?request.?failed",
        "upstream_stream_read_error",
        "stream was interrupted",
        "http/?2 stream failed",
        "network.?error",
        "connection.?error",
        "connection.?refused",
        "connection.?lost",
        "other side closed",
        "fetch failed",
        r"request to \S+ failed, reason:",
        "getaddrinfo",
        "ENOTFOUND",
        "EAI_AGAIN",
        "upstream.?connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed? out",
        "timeout",
        "terminated",
        "websocket.?closed",
        "websocket.?error",
        "ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "stream disconnected before completion",
        "pending stream has been canceled",
        "retry delay",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "ResourceExhausted",
        "subscription_sharing_usage_unavailable",
        "subscription_sharing_user_unavailable",
    ])
});

fn build_provider_error_pattern(patterns: &[&str]) -> Regex {
    Regex::new(&format!("(?i){}", patterns.join("|"))).expect("valid provider error pattern")
}

const RETRYABLE_STATUS_CODES: [&str; 7] = ["429", "500", "502", "503", "504", "520", "524"];

/// `(?<![a-z0-9])(?:429|500|...)(?![a-z0-9])` with the `i` flag.
fn contains_retryable_status_code(text: &str) -> bool {
    let bytes = text.as_bytes();
    for code in RETRYABLE_STATUS_CODES {
        let mut start = 0;
        while let Some(pos) = text[start..].find(code) {
            let index = start + pos;
            let before_ok = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
            let end = index + code.len();
            let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
            start = index + 1;
        }
    }
    false
}

/// Retry policy: bounded attempts with exponential backoff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Base delay in ms. Per-attempt delay is `base_delay_ms * 2^(attempt-1)`.
    pub base_delay_ms: u64,
    /// Optional cap for agent-level retry delays in ms. Defaults to 60 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
}

pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Delay before retry `attempt` (1-indexed).
pub fn retry_delay_ms(base_delay_ms: u64, max_agent_delay_ms: Option<u64>, attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1);
    let delay = 2u64
        .checked_pow(exponent)
        .and_then(|factor| base_delay_ms.checked_mul(factor))
        .filter(|d| *d <= MAX_SAFE_INTEGER)
        .unwrap_or(MAX_SAFE_INTEGER);
    delay.min(max_agent_delay_ms.unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS))
}

/// Optional callbacks emitted by [`retry_assistant_call`] around each retry.
pub type OnRetryScheduled = Box<dyn Fn(u32, u32, u64, String) -> BoxFuture<'static, ()> + Send + Sync>;
pub type OnRetryAttemptStart = Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
pub type OnRetryFinished = Box<dyn Fn(bool, u32, Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Default)]
pub struct RetryCallbacks {
    /// Emitted before the backoff sleep: (attempt, max_attempts, delay_ms, error_message).
    pub on_retry_scheduled: Option<OnRetryScheduled>,
    /// Emitted after the backoff sleep, before the retried call starts.
    pub on_retry_attempt_start: Option<OnRetryAttemptStart>,
    /// Emitted once when the loop ends: (success, attempt, final_error).
    pub on_retry_finished: Option<OnRetryFinished>,
}

/// Run a single assistant-producing call with bounded retry on transient errors.
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<&RetryPolicy>,
    signal: Option<&CancellationToken>,
    callbacks: Option<&RetryCallbacks>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let max_attempts = match policy {
        Some(p) if p.enabled => p.max_retries,
        _ => 0,
    };
    let mut attempt = 0u32;
    let mut last_retry: Option<(u32, String)> = None;
    loop {
        let response = produce().await;

        if response.stop_reason == StopReason::Aborted {
            if let (Some((a, _)), Some(cb)) = (&last_retry, callbacks.and_then(|c| c.on_retry_finished.as_ref())) {
                cb(false, *a, None).await;
            }
            return response;
        }
        if response.stop_reason != StopReason::Error {
            if let (Some((a, _)), Some(cb)) = (&last_retry, callbacks.and_then(|c| c.on_retry_finished.as_ref())) {
                cb(true, *a, None).await;
            }
            return response;
        }
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let (Some((a, _)), Some(cb)) = (&last_retry, callbacks.and_then(|c| c.on_retry_finished.as_ref())) {
                cb(false, *a, response.error_message.clone()).await;
            }
            return response;
        }

        attempt += 1;
        let error_message = response.error_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "Unknown error".into());
        last_retry = Some((attempt, error_message.clone()));
        let policy = policy.expect("policy is set when retries are enabled");
        let delay = retry_delay_ms(policy.base_delay_ms, policy.max_agent_delay_ms, attempt);
        if let Some(cb) = callbacks.and_then(|c| c.on_retry_scheduled.as_ref()) {
            cb(attempt, max_attempts, delay, error_message.clone()).await;
        }

        let aborted = match signal {
            Some(signal) if signal.is_cancelled() => true,
            Some(signal) => tokio::select! {
                _ = signal.cancelled() => true,
                _ = crate::utils::wasm::sleep(std::time::Duration::from_millis(delay)) => false,
            },
            None => {
                crate::utils::wasm::sleep(std::time::Duration::from_millis(delay)).await;
                false
            }
        };
        if aborted {
            if let Some(cb) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
                cb(false, attempt, Some(error_message)).await;
            }
            let mut rest = response;
            rest.error_message = None;
            rest.stop_reason = StopReason::Aborted;
            return rest;
        }
        if let Some(cb) = callbacks.and_then(|c| c.on_retry_attempt_start.as_ref()) {
            cb().await;
        }
    }
}

/// Classifies whether a failed assistant message looks like a transient provider or transport error.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = message.error_message.as_deref().filter(|m| !m.is_empty()) else {
        return false;
    };
    if NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN.is_match(error_message) {
        return false;
    }
    RETRYABLE_PROVIDER_ERROR_PATTERN.is_match(error_message) || contains_retryable_status_code(error_message)
}
