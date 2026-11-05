//! Provider HTTP error normalization (port of `utils/error-body.ts`).
//!
//! The TypeScript version probes SDK error objects. The Rust port has one HTTP error type,
//! [`ProviderHttpError`], produced by the HTTP layer of every API implementation.

use reqwest::header::HeaderMap;
use serde_json::Value;

pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// Which SDK's error-message convention to reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkErrorStyle {
    /// `@anthropic-ai/sdk`: the whole parsed JSON body is the error object.
    Anthropic,
    /// `openai`: the body's `error` field is the error object.
    OpenAI,
}

/// An HTTP-level provider error (the equivalent of the SDKs' `APIError`).
/// `status: None` is a connection error (`APIConnectionError`).
#[derive(Debug, Clone)]
pub struct ProviderHttpError {
    pub status: Option<u16>,
    pub headers: Option<HeaderMap>,
    /// Display message, formatted like the SDK error message.
    pub message: String,
    /// Raw response body text, when one was read.
    pub body: Option<String>,
    /// The SDK's `error` field: the parsed error object.
    pub error: Option<Value>,
    pub name: String,
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderHttpError {}

/// JavaScript truthiness of a JSON value.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `APIError.makeMessage` of the `openai`/`@anthropic-ai/sdk` SDKs.
fn sdk_make_message(status: Option<u16>, error: Option<&Value>, message: Option<&str>) -> String {
    use crate::utils::js_json::js_json_stringify;
    let msg: Option<String> = match error.filter(|e| js_truthy(e)) {
        Some(err) => match err.get("message").filter(|m| js_truthy(m)) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(m) => Some(js_json_stringify(m)),
            None => Some(js_json_stringify(err)),
        },
        None => message.map(str::to_string),
    };
    let msg = msg.filter(|m| !m.is_empty());
    match (status, msg) {
        (Some(status), Some(msg)) => format!("{status} {msg}"),
        (Some(status), None) => format!("{status} status code (no body)"),
        (None, Some(msg)) => msg,
        (None, None) => "(no status code or body)".to_string(),
    }
}

impl ProviderHttpError {
    /// Build an HTTP status error the way the SDK would from a response body.
    pub fn from_response(status: u16, headers: HeaderMap, body_text: String, style: SdkErrorStyle) -> Self {
        let parsed: Option<Value> = serde_json::from_str(&body_text).ok();
        let (error, message) = match style {
            SdkErrorStyle::Anthropic => match &parsed {
                Some(json) => (Some(json.clone()), None),
                None => (None, Some(body_text.clone())),
            },
            SdkErrorStyle::OpenAI => match &parsed {
                Some(json) => (json.get("error").cloned(), None),
                None => (None, Some(body_text.clone())),
            },
        };
        let display = sdk_make_message(Some(status), error.as_ref(), message.as_deref());
        ProviderHttpError {
            status: Some(status),
            headers: Some(headers),
            message: display,
            body: if body_text.is_empty() { None } else { Some(body_text) },
            error,
            name: status_error_name(status).to_string(),
        }
    }

    /// A connection-level failure (no HTTP status).
    pub fn connection(message: impl Into<String>) -> Self {
        ProviderHttpError {
            status: None,
            headers: None,
            message: message.into(),
            body: None,
            error: None,
            name: "APIConnectionError".to_string(),
        }
    }

    /// Map a reqwest transport error to the SDK's connection error messages.
    pub fn from_reqwest(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            let mut e = Self::connection("Request timed out.");
            e.name = "APIConnectionTimeoutError".to_string();
            e
        } else {
            // Like the SDKs' APIConnectionError: the cause is not part of the message.
            Self::connection("Connection error.")
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.as_ref().and_then(|h| h.get(name)).and_then(|v| v.to_str().ok())
    }
}

fn status_error_name(status: u16) -> &'static str {
    match status {
        400 => "BadRequestError",
        401 => "AuthenticationError",
        403 => "PermissionDeniedError",
        404 => "NotFoundError",
        409 => "ConflictError",
        422 => "UnprocessableEntityError",
        429 => "RateLimitError",
        s if s >= 500 => "InternalServerError",
        _ => "APIError",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedProviderError {
    pub status: Option<u16>,
    pub body: Option<String>,
    pub message: String,
    pub message_carries_body: bool,
}

/// Normalize any error into status/body/message.
pub fn normalize_provider_error(error: &anyhow::Error) -> NormalizedProviderError {
    if let Some(http) = error.downcast_ref::<ProviderHttpError>() {
        // Like the SDK error objects: the parsed error body (`error.error`) is the body when it is
        // a non-empty object.
        let body = match &http.error {
            Some(Value::Object(map)) if !map.is_empty() => Some(Value::Object(map.clone()).to_string()),
            _ => None,
        }
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .map(|b| truncate_error_text(&b, MAX_PROVIDER_ERROR_BODY_CHARS));
        let message_carries_body = match &body {
            None => true,
            Some(b) => http.message.contains(b.as_str()),
        };
        return NormalizedProviderError { status: http.status, body, message: http.message.clone(), message_carries_body };
    }
    NormalizedProviderError { status: None, body: None, message: error.to_string(), message_carries_body: true }
}

/// Compose a display string from a normalized error.
pub fn format_provider_error(norm: &NormalizedProviderError, prefix: Option<&str>) -> String {
    match (norm.message_carries_body, norm.status, &norm.body) {
        (false, Some(status), Some(body)) => match prefix {
            Some(prefix) => format!("{prefix} ({status}): {body}"),
            None => format!("{status}: {body}"),
        },
        _ => match (prefix, norm.status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", norm.message),
            _ => norm.message.clone(),
        },
    }
}

/// Truncate to `max_chars` UTF-16-ish characters (counted as chars here).
pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}... [truncated {} chars]", count - max_chars)
}

pub fn safe_json_stringify(value: &Value) -> String {
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    #[test]
    fn sdk_messages_and_normalization() {
        let anthropic = ProviderHttpError::from_response(
            400,
            HeaderMap::new(),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#.into(),
            SdkErrorStyle::Anthropic,
        );
        assert_eq!(anthropic.message, r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#);
        let norm = normalize_provider_error(&anyhow::Error::new(anthropic));
        assert!(norm.message_carries_body);
        assert_eq!(format_provider_error(&norm, None), norm.message);

        let openai = ProviderHttpError::from_response(
            400,
            HeaderMap::new(),
            r#"{"error":{"message":"Bad things","code":null}}"#.into(),
            SdkErrorStyle::OpenAI,
        );
        assert_eq!(openai.message, "400 Bad things");
        let norm = normalize_provider_error(&anyhow::Error::new(openai));
        assert_eq!(format_provider_error(&norm, None), r#"400: {"message":"Bad things","code":null}"#);

        let no_body = ProviderHttpError::from_response(403, HeaderMap::new(), String::new(), SdkErrorStyle::OpenAI);
        assert_eq!(no_body.message, "403 status code (no body)");
        let detail = ProviderHttpError::from_response(502, HeaderMap::new(), r#"{"detail":"x"}"#.into(), SdkErrorStyle::OpenAI);
        assert_eq!(detail.message, "502 status code (no body)");
        assert_eq!(truncate_error_text("abcdef", 3), "abc... [truncated 3 chars]");
    }

    #[tokio::test]
    async fn connection_failures_read_like_the_sdks() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        drop(listener);
        let error = reqwest::Client::new().get(url).send().await.unwrap_err();
        let http = ProviderHttpError::from_reqwest(&error);
        assert_eq!(http.message, "Connection error.");
        assert_eq!(http.name, "APIConnectionError");
    }
}
