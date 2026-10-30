//! HTTP transport for the OAuth flows (the `fetch` the TypeScript flows call).
//!
//! Flows capture [`current_oauth_fetch`] when they start. Tests (and embedders) can replace the
//! transport for a task with [`with_oauth_fetch`], the equivalent of stubbing `fetch`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::utils::http::client_for;

/// An outgoing OAuth HTTP request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl OAuthHttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// `application/x-www-form-urlencoded` body (`URLSearchParams`).
    pub fn form(self, fields: &[(&str, &str)]) -> Self {
        self.body(form_urlencode(fields))
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Parsed form body fields.
    pub fn form_fields(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(self.body.as_deref().unwrap_or_default().as_bytes())
            .into_owned()
            .collect()
    }

    pub fn form_field(&self, name: &str) -> Option<String> {
        self.form_fields()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    pub fn json_body(&self) -> Option<Value> {
        serde_json::from_str(self.body.as_deref()?).ok()
    }
}

/// `URLSearchParams` serialization.
pub fn form_urlencode(fields: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// A fully read OAuth HTTP response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl OAuthHttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        let status_text = reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default()
            .to_string();
        Self {
            status,
            status_text,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json_response(status: u16, body: &Value) -> Self {
        let mut response = Self::new(status, body.to_string());
        response
            .headers
            .push(("content-type".into(), "application/json".into()));
        response
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `response.json()`.
    pub fn json(&self) -> anyhow::Result<Value> {
        crate::utils::js_json::js_json_parse(&self.body).map_err(anyhow::Error::msg)
    }

    /// `readJson`: the body as an object, `None` when it is not a JSON object.
    pub fn json_object(&self) -> Option<serde_json::Map<String, Value>> {
        match serde_json::from_str::<Value>(&self.body) {
            Ok(Value::Object(map)) => Some(map),
            _ => None,
        }
    }
}

/// Transport failures, worded like `fetch` rejections.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OAuthFetchError {
    /// The caller's signal aborted the request (`AbortError`).
    #[error("This operation was aborted")]
    Aborted,
    /// The request timeout elapsed (`TimeoutError`).
    #[error("The operation was aborted due to timeout")]
    Timeout,
    /// A network failure (`TypeError: fetch failed`, the network error as its cause).
    #[error(transparent)]
    Network(crate::utils::http::FetchFailed),
    /// The connection failed while reading the body (`TypeError: terminated`).
    #[error(transparent)]
    Terminated(crate::utils::http::FetchTerminated),
}

pub type OAuthFetch = Arc<
    dyn Fn(OAuthHttpRequest) -> BoxFuture<'static, anyhow::Result<OAuthHttpResponse>> + Send + Sync,
>;

tokio::task_local! {
    static OAUTH_FETCH: OAuthFetch;
}

/// Run `future` with `fetch` as the OAuth transport of the current task.
pub async fn with_oauth_fetch<F: Future>(fetch: OAuthFetch, future: F) -> F::Output {
    OAUTH_FETCH.scope(fetch, future).await
}

/// The OAuth transport of the current task (reqwest unless overridden).
pub fn current_oauth_fetch() -> OAuthFetch {
    OAUTH_FETCH
        .try_with(Clone::clone)
        .unwrap_or_else(|_| default_oauth_fetch())
}

/// The reqwest-backed transport (honors the proxy environment).
pub fn default_oauth_fetch() -> OAuthFetch {
    Arc::new(|request: OAuthHttpRequest| {
        crate::utils::wasm::boxed(async move {
            let client = client_for(&request.url, None)?;
            let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
            let mut builder = client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder
                .send()
                .await
                .map_err(|e| OAuthFetchError::Network(crate::utils::http::FetchFailed::new(&e)))?;
            let status = response.status();
            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = response
                .text()
                .await
                .map_err(|e| {
                    OAuthFetchError::Terminated(crate::utils::http::FetchTerminated {
                        cause: crate::utils::http::NetworkError::from_reqwest(&e),
                    })
                })?;
            Ok(OAuthHttpResponse {
                status: status.as_u16(),
                status_text: status.canonical_reason().unwrap_or_default().to_string(),
                headers,
                body,
            })
        })
    })
}

/// Send a request, honoring an abort signal and an optional timeout.
pub async fn send(
    fetch: &OAuthFetch,
    request: OAuthHttpRequest,
    signal: Option<&CancellationToken>,
    timeout_ms: Option<u64>,
) -> anyhow::Result<OAuthHttpResponse> {
    if signal.is_some_and(|s| s.is_cancelled()) {
        return Err(OAuthFetchError::Aborted.into());
    }
    let request_future = fetch(request);
    let timeout = async {
        match timeout_ms {
            Some(ms) => crate::utils::wasm::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending::<()>().await,
        }
    };
    let cancelled = async {
        match signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        result = request_future => result,
        _ = timeout => Err(OAuthFetchError::Timeout.into()),
        _ = cancelled => Err(OAuthFetchError::Aborted.into()),
    }
}

/// Whether an error is a `fetch` abort or timeout.
pub fn is_fetch_abort(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<OAuthFetchError>(),
        Some(OAuthFetchError::Aborted | OAuthFetchError::Timeout)
    )
}
