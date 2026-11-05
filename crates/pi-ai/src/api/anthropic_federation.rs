//! Anthropic workload identity federation.
//!
//! The TS adapter hands an `oidc_federation` config to `@anthropic-ai/sdk` 0.129.0, which exchanges
//! the identity token (RFC 7523 jwt-bearer grant against `POST {baseURL}/v1/oauth/token`) for a
//! short-lived access token, caches it with two-tier proactive refresh (`TokenCache`), sends it as
//! `Authorization: Bearer`, appends the `oauth-2025-04-20` beta and invalidates it after a 401.
//! This module reproduces that SDK behavior (`lib/credentials/{oidc-federation,identity-token,
//! token-cache,types}.ts`).

use std::sync::{Arc, LazyLock};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::env_api_keys::{
    ANTHROPIC_FEDERATION_RULE_ID_ENV, ANTHROPIC_IDENTITY_TOKEN_FILE_ENV, ANTHROPIC_ORGANIZATION_ID_ENV,
    ANTHROPIC_SERVICE_ACCOUNT_ID_ENV, ANTHROPIC_WORKSPACE_ID_ENV,
};
use crate::types::ProviderEnv;
use crate::utils::provider_env::get_provider_env_value;

pub const GRANT_TYPE_JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
pub const TOKEN_ENDPOINT: &str = "/v1/oauth/token";
/// `anthropic-beta` value required on requests authenticated with an OAuth bearer token.
pub const OAUTH_API_BETA_HEADER: &str = "oauth-2025-04-20";
/// `anthropic-beta` value required on jwt-bearer exchanges.
pub const FEDERATION_BETA_HEADER: &str = "oidc-federation-2026-04-01";
const ADVISORY_REFRESH_THRESHOLD_IN_SECONDS: i64 = 120;
const MANDATORY_REFRESH_THRESHOLD_IN_SECONDS: i64 = 30;
const ADVISORY_REFRESH_BACKOFF_IN_SECONDS: i64 = 5;
const MAX_TOKEN_RESPONSE_BYTES: usize = 1 << 20;
const MAX_ERROR_BODY_CHARS: usize = 2000;
const MAX_ASSERTION_CHARS: usize = 16 * 1024;

/// The SDK's `oidc_federation` config as pi builds it from the ANTHROPIC_* variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicFederationConfig {
    pub organization_id: String,
    pub workspace_id: Option<String>,
    pub federation_rule_id: String,
    pub service_account_id: Option<String>,
    pub identity_token_file: String,
}

impl AnthropicFederationConfig {
    /// `getAnthropicFederation` without the provider/auth preconditions.
    pub fn from_env(env: Option<&ProviderEnv>) -> Option<Self> {
        let federation_rule_id = get_provider_env_value(ANTHROPIC_FEDERATION_RULE_ID_ENV, env)?;
        let organization_id = get_provider_env_value(ANTHROPIC_ORGANIZATION_ID_ENV, env)?;
        let identity_token_file = get_provider_env_value(ANTHROPIC_IDENTITY_TOKEN_FILE_ENV, env)?;
        Some(Self {
            organization_id,
            workspace_id: get_provider_env_value(ANTHROPIC_WORKSPACE_ID_ENV, env),
            federation_rule_id,
            service_account_id: get_provider_env_value(ANTHROPIC_SERVICE_ACCOUNT_ID_ENV, env),
            identity_token_file,
        })
    }

    /// `JSON.stringify(federation)` (keys with undefined values are omitted).
    fn cache_key(&self) -> String {
        let mut authentication = Map::new();
        authentication.insert("type".into(), json!("oidc_federation"));
        authentication.insert("federation_rule_id".into(), json!(self.federation_rule_id));
        if let Some(id) = &self.service_account_id {
            authentication.insert("service_account_id".into(), json!(id));
        }
        authentication.insert("identity_token".into(), json!({ "source": "file", "path": self.identity_token_file }));
        let mut config = Map::new();
        config.insert("organization_id".into(), json!(self.organization_id));
        if let Some(id) = &self.workspace_id {
            config.insert("workspace_id".into(), json!(id));
        }
        config.insert("authentication".into(), Value::Object(authentication));
        Value::Object(config).to_string()
    }
}

/// The SDK's `WorkloadIdentityError` (and the `AnthropicError`s of the identity-token reader).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct WorkloadIdentityError {
    pub message: String,
    pub status_code: Option<u16>,
    pub request_id: Option<String>,
}

impl WorkloadIdentityError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code: None, request_id: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token: String,
    /// Unix epoch seconds; `None` means no expiry.
    pub expires_at: Option<i64>,
}

fn now_as_seconds() -> i64 {
    crate::utils::now_ms().div_euclid(1000)
}

/// Rejects base URLs that would send the assertion over cleartext HTTP (loopback is allowed).
pub fn require_secure_token_endpoint(base_url: &str) -> Result<(), WorkloadIdentityError> {
    if base_url.is_empty() {
        return Ok(());
    }
    let url = url::Url::parse(base_url).map_err(|_| {
        WorkloadIdentityError::new(format!("Invalid token endpoint base URL \"{base_url}\": TypeError: Invalid URL"))
    })?;
    if url.scheme() == "https" {
        return Ok(());
    }
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if url.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Ok(());
    }
    Err(WorkloadIdentityError::new(format!(
        "Refusing to send credential over non-https token endpoint \"{base_url}\""
    )))
}

/// `String(err)` of a Node `fs.promises.readFile` failure.
fn node_read_error(error: &std::io::Error, path: &str) -> String {
    let (code, text) = match error.kind() {
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        std::io::ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        _ => return format!("Error: {error}"),
    };
    if code == "EISDIR" {
        return format!("Error: {code}: {text}, read");
    }
    format!("Error: {code}: {text}, open '{path}'")
}

/// `identityTokenFromFile`: reads (and trims) the JWT on every exchange, so rotated tokens apply.
pub async fn read_identity_token(path: &str) -> Result<String, WorkloadIdentityError> {
    if path.is_empty() {
        return Err(WorkloadIdentityError::new("Identity token file path is empty"));
    }
    let content = crate::utils::wasm::fs::read(path).await.map_err(|error| {
        WorkloadIdentityError::new(format!(
            "Failed to read identity token file at {path}: {}",
            node_read_error(&error, path)
        ))
    })?;
    let token = String::from_utf8_lossy(&content).trim().to_string();
    if token.is_empty() {
        return Err(WorkloadIdentityError::new(format!("Identity token file at {path} is empty")));
    }
    Ok(token)
}

/// `redactSensitive` for a token-endpoint body: JSON keeps only RFC 6749 error fields, other
/// text is truncated.
fn redact_text(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(parsed) => redact_value(&parsed).to_string(),
        Err(_) => {
            let units: Vec<u16> = body.encode_utf16().collect();
            if units.len() <= MAX_ERROR_BODY_CHARS {
                body.to_string()
            } else {
                format!(
                    "{}... <{} more chars>",
                    String::from_utf16_lossy(&units[..MAX_ERROR_BODY_CHARS]),
                    units.len() - MAX_ERROR_BODY_CHARS
                )
            }
        }
    }
}

fn redact_value(body: &Value) -> Value {
    match body {
        Value::Null => Value::Null,
        Value::String(text) => Value::String(redact_text(text)),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| matches!(k.as_str(), "error" | "error_description" | "error_uri"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// `Number(value)` for the `expires_in` field.
fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(f64::NAN) }
        }
        Some(_) => f64::NAN,
    }
}

/// wasm32: reqwest's fetch backend has no incremental `chunk()`.
#[cfg(target_arch = "wasm32")]
async fn read_limited_text(response: reqwest::Response) -> String {
    let bytes = response.bytes().await.unwrap_or_default();
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_TOKEN_RESPONSE_BYTES)]).into_owned()
}
#[cfg(not(target_arch = "wasm32"))]
async fn read_limited_text(response: reqwest::Response) -> String {
    let mut response = response;
    let mut bytes: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len() + chunk.len() > MAX_TOKEN_RESPONSE_BYTES {
            let remaining = MAX_TOKEN_RESPONSE_BYTES - bytes.len();
            bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One jwt-bearer exchange (`oidcFederationProvider`).
pub async fn exchange_token(
    config: &AnthropicFederationConfig,
    base_url: &str,
    client: &reqwest::Client,
    user_agent: &str,
) -> Result<AccessToken, WorkloadIdentityError> {
    require_secure_token_endpoint(base_url)?;
    let jwt = read_identity_token(&config.identity_token_file).await?;
    let jwt_chars = jwt.encode_utf16().count();
    if jwt_chars > MAX_ASSERTION_CHARS {
        return Err(WorkloadIdentityError::new(format!(
            "Identity token is {} KiB, exceeds the 16 KiB assertion limit",
            jwt_chars.div_ceil(1024)
        )));
    }
    let mut body = Map::new();
    body.insert("grant_type".into(), json!(GRANT_TYPE_JWT_BEARER));
    body.insert("assertion".into(), json!(jwt));
    body.insert("federation_rule_id".into(), json!(config.federation_rule_id));
    body.insert("organization_id".into(), json!(config.organization_id));
    if let Some(id) = config.service_account_id.as_deref().filter(|id| !id.is_empty()) {
        body.insert("service_account_id".into(), json!(id));
    }
    if let Some(id) = config.workspace_id.as_deref().filter(|id| !id.is_empty()) {
        body.insert("workspace_id".into(), json!(id));
    }
    let url = format!("{base_url}{TOKEN_ENDPOINT}");
    let user_agent = if user_agent.is_empty() {
        format!("anthropic-sdk-typescript/{} oidcFederationProvider", super::anthropic_messages::ANTHROPIC_SDK_VERSION)
    } else {
        user_agent.to_string()
    };
    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", format!("{OAUTH_API_BETA_HEADER},{FEDERATION_BETA_HEADER}"))
        .header("User-Agent", user_agent)
        .body(Value::Object(body).to_string())
        .send()
        .await
        .map_err(|error| {
            WorkloadIdentityError::new(format!("Failed to reach token endpoint {url}: TypeError: fetch failed ({error})"))
        })?;
    let status = response.status().as_u16();
    let request_id = response.headers().get("request-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let request_id_text = request_id.as_ref().map(|id| format!(" (request-id {id})")).unwrap_or_default();

    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        let redacted = redact_text(&text);
        let hint = if status == 401 {
            let middle = if config.workspace_id.as_deref().is_some_and(|id| !id.is_empty()) {
                ""
            } else {
                "If your federation rule is scoped to multiple workspaces, set the ANTHROPIC_WORKSPACE_ID environment variable, the 'workspace_id' config key, or the `workspaceId` option. "
            };
            format!(
                " Ensure your federation rule matches your identity token. {middle}View your authentication events in the Workload identity page of Claude Console for more details."
            )
        } else {
            String::new()
        };
        return Err(WorkloadIdentityError {
            message: format!("Token exchange failed with status {status}{request_id_text}: {redacted}{hint}"),
            status_code: Some(status),
            request_id,
        });
    }

    let text = read_limited_text(response).await;
    let error = |message: String| WorkloadIdentityError { message, status_code: Some(status), request_id: request_id.clone() };
    let data: Value = serde_json::from_str(&text)
        .map_err(|_| error(format!("Token endpoint returned non-JSON response (status {status})")))?;
    let access_token = match data.get("access_token") {
        Some(Value::String(token)) if !token.is_empty() => token.clone(),
        Some(value) if !value.is_null() && value != &json!(false) && value != &json!(0) && value != &json!("") => {
            value.to_string()
        }
        _ => {
            return Err(error(format!("Token endpoint response missing access_token: {}", redact_value(&data))));
        }
    };
    if let Some(token_type) = data.get("token_type").and_then(Value::as_str).filter(|t| !t.is_empty())
        && !token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(error(format!("Token endpoint response: unsupported token_type \"{token_type}\" (want Bearer)")));
    }
    let expires_in = js_number(data.get("expires_in"));
    if !expires_in.is_finite() {
        return Err(error(format!("Token endpoint response missing required fields: {}", redact_value(&data))));
    }
    Ok(AccessToken { token: access_token, expires_at: Some(now_as_seconds() + expires_in as i64) })
}

type ProviderFuture = BoxFuture<'static, Result<AccessToken, WorkloadIdentityError>>;
/// Mints a fresh access token; the flag is `forceRefresh`.
pub type AccessTokenProvider = Arc<dyn Fn(bool) -> ProviderFuture + Send + Sync>;
type SharedRefresh = Shared<ProviderFuture>;

#[derive(Default)]
struct CacheState {
    cached: Option<AccessToken>,
    pending: Option<(u64, SharedRefresh)>,
    next_force: bool,
    last_advisory_error: i64,
    generation: u64,
}

/// Port of the SDK's `TokenCache`: two-tier proactive refresh with concurrent deduplication.
///
/// More than 120 s left: cached; 30-120 s: cached plus a background refresh; less: blocking
/// refresh. [`invalidate`](Self::invalidate) (after a 401) forces the next refresh.
pub struct TokenCache {
    provider: AccessTokenProvider,
    state: Arc<Mutex<CacheState>>,
}

impl TokenCache {
    pub fn new(provider: AccessTokenProvider) -> Self {
        Self { provider, state: Arc::new(Mutex::new(CacheState::default())) }
    }

    pub async fn get_token(&self) -> Result<String, WorkloadIdentityError> {
        let (force, cached) = {
            let mut state = self.state.lock();
            let force = state.next_force;
            state.next_force = false;
            (force, state.cached.clone())
        };
        let Some(cached) = cached.filter(|_| !force) else {
            return self.refresh(force).await.map(|t| t.token);
        };
        let Some(expires_at) = cached.expires_at else { return Ok(cached.token) };
        let remaining = expires_at - now_as_seconds();
        if remaining > ADVISORY_REFRESH_THRESHOLD_IN_SECONDS {
            return Ok(cached.token);
        }
        if remaining > MANDATORY_REFRESH_THRESHOLD_IN_SECONDS {
            self.background_refresh();
            return Ok(cached.token);
        }
        self.refresh(false).await.map(|t| t.token)
    }

    /// Clear the cached token and force the next refresh (the server rejected the token).
    pub fn invalidate(&self) {
        let mut state = self.state.lock();
        state.cached = None;
        state.next_force = true;
    }

    fn refresh(&self, force: bool) -> SharedRefresh {
        {
            let state = self.state.lock();
            if let Some((_, pending)) = &state.pending
                && !force
            {
                return pending.clone();
            }
        }
        self.do_refresh(force)
    }

    fn background_refresh(&self) {
        {
            let state = self.state.lock();
            if state.pending.is_some() || now_as_seconds() - state.last_advisory_error < ADVISORY_REFRESH_BACKOFF_IN_SECONDS {
                return;
            }
        }
        let refresh = self.do_refresh(false);
        let state = self.state.clone();
        crate::utils::wasm::spawn(async move {
            if refresh.await.is_err() {
                // Keep serving the stale token; back off before the next advisory attempt.
                state.lock().last_advisory_error = now_as_seconds();
            }
        });
    }

    fn do_refresh(&self, force: bool) -> SharedRefresh {
        let state_ref = self.state.clone();
        let mut state = self.state.lock();
        state.generation += 1;
        let generation = state.generation;
        let request = (self.provider)(force);
        let shared: SharedRefresh = async move {
            let result = request.await;
            let mut state = state_ref.lock();
            if let Ok(token) = &result {
                state.cached = Some(token.clone());
            }
            if state.pending.as_ref().is_some_and(|(g, _)| *g == generation) {
                state.pending = None;
            }
            result
        }
        .boxed()
        .shared();
        state.pending = Some((generation, shared.clone()));
        shared
    }
}

/// One cached federation client per config (TS: the module-level `federationClient`), so the
/// token cache survives across requests.
type FederationClient = Option<(String, Arc<TokenCache>)>;
static FEDERATION_CLIENT: LazyLock<Mutex<FederationClient>> = LazyLock::new(|| Mutex::new(None));

/// The token cache for `config` against `base_url` (the model's base URL), creating it when the
/// config changed. `client` performs the exchanges.
pub fn federation_token_cache(
    config: &AnthropicFederationConfig,
    base_url: &str,
    client: reqwest::Client,
    user_agent: String,
) -> Arc<TokenCache> {
    let key = format!("[{},{}]", json!(base_url), config.cache_key());
    let mut current = FEDERATION_CLIENT.lock();
    if let Some((existing, cache)) = current.as_ref()
        && *existing == key
    {
        return cache.clone();
    }
    let exchange_base_url = base_url.trim_end_matches('/').to_string();
    let config = config.clone();
    let provider: AccessTokenProvider = Arc::new(move |_force| {
        let config = config.clone();
        let base_url = exchange_base_url.clone();
        let client = client.clone();
        let user_agent = user_agent.clone();
        crate::utils::wasm::boxed(async move { exchange_token(&config, &base_url, &client, &user_agent).await })
    });
    let cache = Arc::new(TokenCache::new(provider));
    *current = Some((key, cache.clone()));
    cache
}

/// Append the OAuth beta to an `anthropic-beta` value unless it is already listed.
pub fn with_oauth_beta(existing: Option<&str>) -> String {
    let mut features: Vec<String> = existing.map(|v| v.split(',').map(|s| s.trim().to_string()).collect()).unwrap_or_default();
    if !features.iter().any(|f| f == OAUTH_API_BETA_HEADER) {
        features.push(OAUTH_API_BETA_HEADER.to_string());
    }
    features.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn secure_token_endpoints() {
        assert!(require_secure_token_endpoint("https://api.anthropic.com").is_ok());
        assert!(require_secure_token_endpoint("http://localhost:8080").is_ok());
        assert!(require_secure_token_endpoint("http://[::1]:9").is_ok());
        assert_eq!(
            require_secure_token_endpoint("http://example.com").unwrap_err().message,
            "Refusing to send credential over non-https token endpoint \"http://example.com\""
        );
    }

    #[test]
    fn redacts_error_bodies() {
        assert_eq!(
            redact_text(r#"{"error":"invalid_grant","assertion":"secret","error_description":"bad"}"#),
            r#"{"error":"invalid_grant","error_description":"bad"}"#
        );
        let long = "x".repeat(2005);
        assert_eq!(redact_text(&long), format!("{}... <5 more chars>", "x".repeat(2000)));
    }

    #[test]
    fn appends_oauth_beta_once() {
        assert_eq!(with_oauth_beta(None), "oauth-2025-04-20");
        assert_eq!(with_oauth_beta(Some("a, b")), "a,b,oauth-2025-04-20");
        assert_eq!(with_oauth_beta(Some("oauth-2025-04-20")), "oauth-2025-04-20");
    }

    fn counting_cache(expires_in: i64) -> (TokenCache, Arc<AtomicUsize>, Arc<Mutex<Vec<bool>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let forces = Arc::new(Mutex::new(Vec::new()));
        let (c, f) = (calls.clone(), forces.clone());
        let cache = TokenCache::new(Arc::new(move |force| {
            let n = c.fetch_add(1, Ordering::SeqCst) + 1;
            f.lock().push(force);
            async move { Ok(AccessToken { token: format!("t{n}"), expires_at: Some(now_as_seconds() + expires_in) }) }.boxed()
        }));
        (cache, calls, forces)
    }

    #[tokio::test]
    async fn caches_tokens_and_forces_refresh_after_invalidate() {
        let (cache, calls, forces) = counting_cache(3600);
        let (a, b) = tokio::join!(cache.get_token(), cache.get_token());
        assert_eq!((a.unwrap(), b.unwrap()), ("t1".to_string(), "t1".to_string()));
        assert_eq!(cache.get_token().await.unwrap(), "t1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cache.invalidate();
        assert_eq!(cache.get_token().await.unwrap(), "t2");
        assert_eq!(*forces.lock(), [false, true]);
    }

    #[tokio::test]
    async fn refreshes_in_the_advisory_and_mandatory_windows() {
        let (cache, calls, _) = counting_cache(60);
        assert_eq!(cache.get_token().await.unwrap(), "t1");
        // 60 s left: serve the cached token and refresh in the background.
        assert_eq!(cache.get_token().await.unwrap(), "t1");
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) == 2 {
                break;
            }
            crate::utils::wasm::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let (cache, calls, _) = counting_cache(10);
        assert_eq!(cache.get_token().await.unwrap(), "t1");
        // 10 s left: block on a refresh.
        assert_eq!(cache.get_token().await.unwrap(), "t2");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
