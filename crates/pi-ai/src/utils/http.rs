//! Shared HTTP client construction (the `fetch` equivalent).
//!
//! Clients honor `HTTP(S)_PROXY`/`ALL_PROXY`/`NO_PROXY` (with provider-scoped env overrides
//! taking precedence) using the same resolution rules as `node-http-proxy.ts`.

use std::collections::HashMap;
use std::time::Duration;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::node_http_proxy::get_proxy_url_for_target;
use crate::types::ProviderEnv;

static CLIENTS: Lazy<Mutex<HashMap<String, reqwest::Client>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// SDK default request timeout (10 minutes).
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;

/// Idle timeout for response headers and body reads (`None` = disabled).
static IDLE_TIMEOUT: Lazy<Mutex<Option<Duration>>> = Lazy::new(|| Mutex::new(None));

/// Set the idle timeout applied to every shared client: a request fails when no response data
/// arrives for this long (undici's `headersTimeout`/`bodyTimeout`). `None` or zero disables it.
/// Cached clients are rebuilt.
pub fn set_http_idle_timeout(timeout: Option<Duration>) {
    *IDLE_TIMEOUT.lock() = timeout.filter(|t| !t.is_zero());
    CLIENTS.lock().clear();
}

/// The idle timeout set with [`set_http_idle_timeout`].
pub fn http_idle_timeout() -> Option<Duration> {
    *IDLE_TIMEOUT.lock()
}

#[cfg(not(target_arch = "wasm32"))]
fn build_client(proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(60));
    if let Some(timeout) = http_idle_timeout() {
        builder = builder.read_timeout(timeout);
    }
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}
/// wasm32: requests go through the host's `fetch`, which owns proxies,
/// connection pooling and timeouts.
#[cfg(target_arch = "wasm32")]
fn build_client(_proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().build()?)
}

/// A client for requests to `target_url`, with the proxy configured from the environment.
/// Clients are cached per proxy URL so connections are pooled.
pub fn client_for(target_url: &str, env: Option<&ProviderEnv>) -> anyhow::Result<reqwest::Client> {
    let proxy = get_proxy_url_for_target(target_url, env);
    let key = proxy.clone().unwrap_or_default();
    if let Some(client) = CLIENTS.lock().get(&key) {
        return Ok(client.clone());
    }
    let client = build_client(proxy.as_deref())?;
    CLIENTS.lock().insert(key, client.clone());
    Ok(client)
}

/// Ordered header list with JavaScript `Headers`-like case-insensitive merging.
/// Later entries replace earlier ones with the same name; `None` removes a header.
#[derive(Debug, Clone, Default)]
pub struct HeaderList {
    entries: Vec<(String, String)>,
}

impl HeaderList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &str, value: Option<&str>) {
        self.entries.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            self.entries.push((name.to_string(), value.to_string()));
        }
    }

    pub fn extend_strings<'a>(&mut self, headers: impl IntoIterator<Item = (&'a String, &'a String)>) {
        for (name, value) in headers {
            self.set(name, Some(value));
        }
    }

    pub fn extend_optional<'a>(&mut self, headers: impl IntoIterator<Item = (&'a String, &'a Option<String>)>) {
        for (name, value) in headers {
            self.set(name, value.as_deref());
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    pub fn to_header_map(&self) -> anyhow::Result<reqwest::header::HeaderMap> {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in &self.entries {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())?;
            let value = reqwest::header::HeaderValue::from_str(value)?;
            map.append(name, value);
        }
        Ok(map)
    }
}

/// `process.version` reported in the Stainless runtime headers.
const STAINLESS_NODE_VERSION: &str = "v22.19.0";

/// Stainless `normalizePlatform` applied to the Node `process.platform` of this target.
fn stainless_os() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" | "illumos" => "sunos",
        other => other,
    };
    match platform {
        p if p.contains("ios") => "iOS".into(),
        "android" => "Android".into(),
        "darwin" => "MacOS".into(),
        "win32" => "Windows".into(),
        "freebsd" => "FreeBSD".into(),
        "openbsd" => "OpenBSD".into(),
        "linux" => "Linux".into(),
        "" => "Unknown".into(),
        other => format!("Other:{other}"),
    }
}

/// Stainless `normalizeArch` applied to the Node `process.arch` of this target.
fn stainless_arch() -> String {
    let arch = match std::env::consts::ARCH {
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "powerpc" => "ppc",
        "loongarch64" => "loong64",
        other => other,
    };
    match arch {
        "x32" => "x32".into(),
        "x86_64" | "x64" => "x64".into(),
        "arm" => "arm".into(),
        "aarch64" | "arm64" => "arm64".into(),
        "" => "unknown".into(),
        other => format!("other:{other}"),
    }
}

/// Stainless SDK platform headers (`X-Stainless-*`) sent by the OpenAI and Anthropic SDKs.
pub fn stainless_headers(package_version: &str, timeout_ms: u64) -> Vec<(String, String)> {
    let os = stainless_os();
    let arch = stainless_arch();
    vec![
        ("X-Stainless-Retry-Count".into(), "0".into()),
        ("X-Stainless-Timeout".into(), (timeout_ms / 1000).to_string()),
        ("X-Stainless-Lang".into(), "js".into()),
        ("X-Stainless-Package-Version".into(), package_version.into()),
        ("X-Stainless-OS".into(), os),
        ("X-Stainless-Arch".into(), arch),
        // The TS SDKs run on Node (or Bun, which reports itself as Node).
        ("X-Stainless-Runtime".into(), "node".into()),
        ("X-Stainless-Runtime-Version".into(), STAINLESS_NODE_VERSION.into()),
    ]
}

/// The `openai` SDK's `readEnv`: the trimmed process env value, empty counting as unset.
fn openai_read_env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `OpenAI-Organization`/`OpenAI-Project` that every `openai` SDK client sends from
/// `OPENAI_ORG_ID`/`OPENAI_PROJECT_ID` (set after the platform headers, before auth).
pub fn apply_openai_sdk_tenant_headers(headers: &mut HeaderList) {
    if let Some(org) = openai_read_env("OPENAI_ORG_ID") {
        headers.set("OpenAI-Organization", Some(&org));
    }
    if let Some(project) = openai_read_env("OPENAI_PROJECT_ID") {
        headers.set("OpenAI-Project", Some(&project));
    }
}

/// `OPENAI_CUSTOM_HEADERS` (`Name: value` lines) that the `openai` SDK client merges into its
/// default headers ahead of the caller's default headers (set after auth).
pub fn apply_openai_custom_headers_env(headers: &mut HeaderList) -> anyhow::Result<()> {
    let Some(value) = openai_read_env("OPENAI_CUSTOM_HEADERS") else { return Ok(()) };
    for line in value.split('\n') {
        let Some(colon) = line.find(':') else { continue };
        let name = line[..colon].trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
            anyhow::bail!("Header name must be a valid HTTP token [\"{name}\"]");
        }
        headers.set(name, Some(line[colon + 1..].trim()));
    }
    Ok(())
}

/// Send a JSON POST request; non-2xx responses become [`ProviderHttpError`]s in the SDK's style.
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderList,
    body: &serde_json::Value,
    timeout_ms: Option<u64>,
    signal: Option<&tokio_util::sync::CancellationToken>,
    style: super::error_body::SdkErrorStyle,
) -> anyhow::Result<reqwest::Response> {
    use super::error_body::ProviderHttpError;
    let mut request = client
        .post(url)
        .headers(headers.to_header_map()?)
        .body(serde_json::to_vec(body)?);
    if let Some(timeout) = timeout_ms {
        request = request.timeout(Duration::from_millis(timeout));
    }
    let send = request.send();
    let result = match signal {
        Some(signal) => {
            if signal.is_cancelled() {
                anyhow::bail!(super::provider_retry::AbortError);
            }
            tokio::select! {
                r = send => r,
                _ = signal.cancelled() => anyhow::bail!(super::provider_retry::AbortError),
            }
        }
        None => send.await,
    };
    let response = result.map_err(|e| anyhow::Error::new(ProviderHttpError::from_reqwest(&e)))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let response_headers = response.headers().clone();
    let text = response.text().await.unwrap_or_default();
    Err(ProviderHttpError::from_response(status.as_u16(), response_headers, text, style).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stainless_headers_report_a_node_runtime() {
        let headers = stainless_headers("1.2.3", 600_000);
        let get = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(get("X-Stainless-Runtime"), Some("node"));
        assert_eq!(get("X-Stainless-Runtime-Version"), Some("v22.19.0"));
        assert_eq!(get("X-Stainless-Timeout"), Some("600"));
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(get("X-Stainless-OS"), Some("Linux"));
            assert_eq!(get("X-Stainless-Arch"), Some("x64"));
        }
    }
}

/// A `fetch` that failed before a response arrived: undici's `TypeError: fetch failed`.
///
/// The message is exactly `fetch failed`, as in Node; the network error is the
/// [`source`](std::error::Error::source) (`error.cause`), worded like Node's
/// (`connect ECONNREFUSED 127.0.0.1:8080`, `getaddrinfo ENOTFOUND example.invalid`) and carrying
/// its errno-style `code`. Callers that show causes (version check, tool downloads, OAuth
/// details) read it from the chain.
#[derive(Debug, Clone, thiserror::Error)]
#[error("fetch failed")]
pub struct FetchFailed {
    #[source]
    pub cause: NetworkError,
}

/// The `cause` of a [`FetchFailed`] (or of a body read that was `terminated`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct NetworkError {
    /// Node's `error.code` (`ECONNREFUSED`, `ENOTFOUND`, `ETIMEDOUT`, ...), when known.
    pub code: Option<&'static str>,
    pub message: String,
}

impl FetchFailed {
    pub fn new(error: &reqwest::Error) -> Self {
        Self { cause: NetworkError::from_reqwest(error) }
    }
}

/// `TypeError: terminated`: the connection failed while the response body was being read.
#[derive(Debug, Clone, thiserror::Error)]
#[error("terminated")]
pub struct FetchTerminated {
    #[source]
    pub cause: NetworkError,
}

impl NetworkError {
    /// Describe a reqwest transport error the way Node's network stack words it.
    pub fn from_reqwest(error: &reqwest::Error) -> Self {
        let url = error.url();
        let host = url.and_then(|u| u.host_str()).unwrap_or("localhost").trim_start_matches('[').trim_end_matches(']');
        // Connections go to the proxy when the environment configures one for this URL.
        let proxy = url
            .and_then(|u| get_proxy_url_for_target(u.as_str(), None))
            .and_then(|p| reqwest::Url::parse(&p).ok());
        let connect_url = proxy.as_ref().or(url);
        let connect_host = connect_url.and_then(|u| u.host_str()).unwrap_or(host).trim_start_matches('[').trim_end_matches(']');
        let address = match connect_url.and_then(|u| u.port_or_known_default()) {
            Some(port) => format!("{connect_host}:{port}"),
            None => connect_host.to_string(),
        };
        let mut texts: Vec<String> = Vec::new();
        let mut io_kind = None;
        let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
        while let Some(current) = source {
            if let Some(io) = current.downcast_ref::<std::io::Error>() {
                io_kind.get_or_insert(io.kind());
            }
            texts.push(current.to_string());
            source = current.source();
        }
        let all = texts.join(": ").to_ascii_lowercase();
        let node = |code: &'static str, message: String| Self { code: Some(code), message };
        if all.contains("dns error") || all.contains("failed to lookup address") {
            return node("ENOTFOUND", format!("getaddrinfo ENOTFOUND {connect_host}"));
        }
        use std::io::ErrorKind;
        match io_kind {
            Some(ErrorKind::ConnectionRefused) => return node("ECONNREFUSED", format!("connect ECONNREFUSED {address}")),
            Some(ErrorKind::ConnectionReset) => return node("ECONNRESET", "read ECONNRESET".into()),
            Some(ErrorKind::ConnectionAborted) => return node("ECONNABORTED", "read ECONNABORTED".into()),
            Some(ErrorKind::TimedOut) => return node("ETIMEDOUT", format!("connect ETIMEDOUT {address}")),
            Some(ErrorKind::HostUnreachable) => return node("EHOSTUNREACH", format!("connect EHOSTUNREACH {address}")),
            Some(ErrorKind::NetworkUnreachable) => return node("ENETUNREACH", format!("connect ENETUNREACH {address}")),
            Some(ErrorKind::BrokenPipe) => return node("EPIPE", "write EPIPE".into()),
            _ => {}
        }
        if error.is_timeout() {
            return node("UND_ERR_CONNECT_TIMEOUT", format!("Connect Timeout Error (attempted address: {address})"));
        }
        if all.contains("unknownissuer") {
            return node("UNABLE_TO_GET_ISSUER_CERT_LOCALLY", "unable to get local issuer certificate".into());
        }
        if all.contains("expired") && all.contains("certificate") {
            return node("CERT_HAS_EXPIRED", "certificate has expired".into());
        }
        if all.contains("notvalidforname") {
            return node(
                "ERR_TLS_CERT_ALTNAME_INVALID",
                format!("Hostname/IP does not match certificate's altnames: Host: {host}. is not in the cert's altnames"),
            );
        }
        if all.contains("connection closed") || all.contains("incomplete message") {
            return node("UND_ERR_SOCKET", "other side closed".into());
        }
        // Anything else: the innermost description.
        Self { code: None, message: texts.pop().unwrap_or_else(|| error.to_string()) }
    }
}

/// `true` when `error`'s chain holds a [`FetchFailed`] or [`FetchTerminated`]: a network failure
/// rather than an HTTP error.
pub fn is_fetch_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<FetchFailed>() || e.is::<FetchTerminated>())
}

#[cfg(test)]
mod fetch_error_tests {
    use super::*;

    #[tokio::test]
    async fn connection_refused_reads_like_node() {
        // Bind and drop a listener to get a port nothing listens on.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        let failed = anyhow::Error::new(FetchFailed::new(&error));
        assert_eq!(failed.to_string(), "fetch failed");
        assert_eq!(format!("{failed:#}"), format!("fetch failed: connect ECONNREFUSED 127.0.0.1:{port}"));
        let cause = failed.chain().nth(1).unwrap().downcast_ref::<NetworkError>().unwrap();
        assert_eq!(cause.code, Some("ECONNREFUSED"));
        assert!(is_fetch_failure(&failed.context("outer")));
    }
}
